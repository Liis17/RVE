use crate::error::BuildError;
use crate::temporary::TempDir;
use engine_assets::{Assets, PREBUILT_MODEL_MAX_BYTES, PREBUILT_MODEL_VERSION};
use engine_files::VirtualPath;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

const CACHE_MAGIC: &[u8; 8] = b"RVEMCACH";
const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_HEADER_BYTES: usize = 8 + 4 + 32 + 8 + 32;
pub(crate) const MODEL_CACHE_LIMIT_BYTES: u64 = 1024 * 1024 * 1024;
const STALE_MODEL_CACHE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);
const UFBX_VERSION: &str = "0.11.5";
const MIKKTSPACE_VERSION: &str = "1.0.0";
const CACHE_KEY_CONTEXT: &str = "RVE rve-build prepared model cache key v1";
const MAX_CACHE_RECORD_BYTES: u64 = (PREBUILT_MODEL_MAX_BYTES + CACHE_HEADER_BYTES) as u64;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelPreparationMode {
    Direct,
    ReadOnly,
    Build,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CacheOutcome {
    Hit,
    Miss,
    Rejected,
}

enum CacheRead {
    Hit(Vec<u8>),
    Miss,
    Rejected,
}

pub(crate) struct ModelPreparer {
    mode: ModelPreparationMode,
    cache: Option<ModelCache>,
    temp_dir: Option<TempDir>,
    files: BTreeMap<VirtualPath, PathBuf>,
    dependencies: BTreeMap<VirtualPath, Vec<String>>,
}

impl ModelPreparer {
    pub(crate) fn new(project_dir: &Path, mode: ModelPreparationMode) -> Self {
        let cache = match mode {
            ModelPreparationMode::Direct => None,
            ModelPreparationMode::ReadOnly => {
                Some(ModelCache::new(model_cache_dir(project_dir), false))
            }
            ModelPreparationMode::Build => {
                Some(ModelCache::new(model_cache_dir(project_dir), true))
            }
        };
        Self {
            mode,
            cache,
            temp_dir: None,
            files: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        }
    }

    pub(crate) fn converts_models(&self) -> bool {
        self.mode != ModelPreparationMode::Direct
    }

    pub(crate) fn prepare(
        &mut self,
        assets: &Assets,
        path: &VirtualPath,
        fbx_bytes: &[u8],
    ) -> Result<Vec<u8>, BuildError> {
        let asset_path = format!("/{path}");
        let (bytes, _) = match &mut self.cache {
            Some(cache) => cache.prepare(assets, &asset_path, fbx_bytes)?,
            None => (convert(assets, &asset_path, fbx_bytes)?, CacheOutcome::Miss),
        };

        if self.mode == ModelPreparationMode::Build {
            let temp_dir = match self.temp_dir.as_mut() {
                Some(temp_dir) => temp_dir,
                None => {
                    self.temp_dir =
                        Some(TempDir::create(&std::env::temp_dir(), "rve-build-models")?);
                    self.temp_dir.as_mut().expect("created above")
                }
            };
            let relative = path.as_str().trim_start_matches('/');
            let staged = temp_dir.path().join(relative);
            if let Some(parent) = staged.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    BuildError::operational(format!(
                        "не удалось подготовить временную модель /{path}: {error}"
                    ))
                })?;
            }
            fs::write(&staged, &bytes).map_err(|error| {
                BuildError::operational(format!(
                    "не удалось записать временную модель /{path}: {error}"
                ))
            })?;
            self.files.insert(path.clone(), staged);
        }
        Ok(bytes)
    }

    pub(crate) fn file_for(&self, path: &VirtualPath) -> Option<&Path> {
        self.files.get(path).map(PathBuf::as_path)
    }

    pub(crate) fn dependencies_for(&self, path: &VirtualPath) -> Option<Vec<String>> {
        self.dependencies.get(path).cloned()
    }

    pub(crate) fn remember_dependencies(&mut self, path: &VirtualPath, dependencies: &[String]) {
        self.dependencies
            .insert(path.clone(), dependencies.to_vec());
    }

    pub(crate) fn into_temp_dir(self) -> Option<Arc<TempDir>> {
        self.temp_dir.map(Arc::new)
    }
}

pub(crate) fn model_cache_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".rve").join("model-cache")
}

/// Удаляет старые записи модели после успешной упаковки, не затрагивая временные и чужие файлы.
pub(crate) fn prune_model_cache(project_dir: &Path, max_bytes: u64) -> io::Result<()> {
    prune_cache_at(&model_cache_dir(project_dir), max_bytes, |path| {
        fs::remove_file(path)
    })
}

fn prune_cache_at(
    root: &Path,
    max_bytes: u64,
    mut remove_file: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let Some(entries) = read_dir_if_exists(root)? else {
        return Ok(());
    };
    let now = SystemTime::now();
    let mut records = Vec::new();
    let mut total = 0_u64;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let directory = entry.path();
        let shard = entry.file_name();
        let shard = shard.to_string_lossy();
        if !is_hex_name(&shard, 2) {
            continue;
        }
        let Some(metadata) = symlink_metadata_if_exists(&directory)? else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Some(nested) = read_dir_if_exists(&directory)? else {
            continue;
        };
        for nested_entry in nested {
            let nested_entry = match nested_entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let name = nested_entry.file_name();
            let path = nested_entry.path();
            let name = name.to_string_lossy();
            let Some(metadata) = symlink_metadata_if_exists(&path)? else {
                continue;
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if is_cache_filename(&name) {
                total = total.saturating_add(metadata.len());
                records.push((modified, metadata.len(), path));
            } else if is_cache_temp_filename(&name)
                && now
                    .duration_since(modified)
                    .is_ok_and(|age| age >= STALE_MODEL_CACHE_TEMP_AGE)
            {
                let _ = remove_file(&path);
            }
        }
    }
    if total <= max_bytes {
        return Ok(());
    }
    records.sort_by_key(|(modified, _, _)| *modified);
    for (_, size, path) in records {
        if total <= max_bytes {
            break;
        }
        match remove_file(&path) {
            Ok(()) => total = total.saturating_sub(size),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                total = total.saturating_sub(size);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_dir_if_exists(path: &Path) -> io::Result<Option<fs::ReadDir>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn symlink_metadata_if_exists(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

struct ModelCache {
    root: PathBuf,
    writable: bool,
    write_disabled: bool,
    warned_unavailable: bool,
}

impl ModelCache {
    fn new(root: PathBuf, writable: bool) -> Self {
        Self {
            root,
            writable,
            write_disabled: false,
            warned_unavailable: false,
        }
    }

    fn prepare(
        &mut self,
        assets: &Assets,
        asset_path: &str,
        fbx_bytes: &[u8],
    ) -> Result<(Vec<u8>, CacheOutcome), BuildError> {
        let key = cache_key(asset_path, fbx_bytes);
        let path = self.entry_path(&key);
        let outcome = match self.read_entry(&path, &key, assets, asset_path) {
            CacheRead::Hit(bytes) => return Ok((bytes, CacheOutcome::Hit)),
            CacheRead::Miss => CacheOutcome::Miss,
            CacheRead::Rejected => CacheOutcome::Rejected,
        };
        let bytes = convert(assets, asset_path, fbx_bytes)?;
        if self.writable
            && !self.write_disabled
            && let Err(error) = self.write_entry(&path, &key, &bytes)
        {
            self.warn_unavailable(&error);
            self.write_disabled = true;
        }
        Ok((bytes, outcome))
    }

    fn read_entry(
        &mut self,
        path: &Path,
        key: &[u8; 32],
        assets: &Assets,
        asset_path: &str,
    ) -> CacheRead {
        let record = match read_cache_record(path) {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return CacheRead::Miss,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return CacheRead::Rejected;
            }
            Err(error) => {
                self.warn_unavailable(&error);
                return CacheRead::Miss;
            }
        };
        let Some(payload) = decode_cache_record(record, key) else {
            return CacheRead::Rejected;
        };
        match assets.validate_prebuilt_model(asset_path, &payload) {
            Ok(()) => CacheRead::Hit(payload),
            Err(_) => CacheRead::Rejected,
        }
    }

    fn write_entry(&mut self, path: &Path, key: &[u8; 32], payload: &[u8]) -> io::Result<()> {
        if payload.len() > PREBUILT_MODEL_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "готовая модель превышает предел 256 MiB",
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("путь кеша без родительского каталога"))?;
        fs::create_dir_all(parent)?;
        let temp = parent.join(format!(
            ".{}.tmp-{}-{}",
            path.file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("model"),
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(CACHE_MAGIC)?;
            file.write_all(&CACHE_SCHEMA_VERSION.to_le_bytes())?;
            file.write_all(key)?;
            file.write_all(&(payload.len() as u64).to_le_bytes())?;
            file.write_all(blake3::hash(payload).as_bytes())?;
            file.write_all(payload)?;
            file.sync_all()?;
            fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }

    fn entry_path(&self, key: &[u8; 32]) -> PathBuf {
        let hex = blake3::Hash::from_bytes(*key).to_hex().to_string();
        self.root.join(&hex[..2]).join(format!("{hex}.rvemodel"))
    }

    fn warn_unavailable(&mut self, error: &io::Error) {
        if !self.warned_unavailable {
            eprintln!(
                "предупреждение: кеш готовых моделей {} недоступен, преобразование выполняется без него: {error}",
                self.root.display()
            );
            self.warned_unavailable = true;
        }
    }
}

fn convert(assets: &Assets, asset_path: &str, fbx_bytes: &[u8]) -> Result<Vec<u8>, BuildError> {
    let bytes = assets
        .convert_model_from_bytes(asset_path, fbx_bytes)
        .map_err(|error| BuildError::input(format!("модель {asset_path}: {error}")))?;
    if bytes.len() > PREBUILT_MODEL_MAX_BYTES {
        return Err(BuildError::input(format!(
            "модель {asset_path}: готовая модель превышает предел размера 256 MiB"
        )));
    }
    Ok(bytes)
}

fn cache_key(asset_path: &str, fbx_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(CACHE_KEY_CONTEXT);
    hash_field(&mut hasher, &CACHE_SCHEMA_VERSION.to_le_bytes());
    hash_field(&mut hasher, &PREBUILT_MODEL_VERSION.to_le_bytes());
    hash_field(&mut hasher, converter_revision().as_bytes());
    hash_field(&mut hasher, asset_path.as_bytes());
    hash_field(&mut hasher, fbx_bytes);
    *hasher.finalize().as_bytes()
}

fn converter_revision() -> String {
    format!(
        "engine-assets-{};ufbx-{UFBX_VERSION};bevy_mikktspace-{MIKKTSPACE_VERSION}",
        engine_assets::VERSION
    )
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn read_cache_record(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    if length > MAX_CACHE_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "запись кеша превышает предел 256 MiB",
        ));
    }
    let capacity = usize::try_from(length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "длина кеша не помещается в памяти",
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|error| {
        io::Error::other(format!(
            "не удалось выделить память под кеш модели: {error}"
        ))
    })?;
    file.take(MAX_CACHE_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CACHE_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "запись кеша превышает предел 256 MiB",
        ));
    }
    Ok(bytes)
}

fn decode_cache_record(mut record: Vec<u8>, key: &[u8; 32]) -> Option<Vec<u8>> {
    if record.len() < CACHE_HEADER_BYTES || &record[..8] != CACHE_MAGIC {
        return None;
    }
    let schema = u32::from_le_bytes(record[8..12].try_into().ok()?);
    if schema != CACHE_SCHEMA_VERSION || &record[12..44] != key {
        return None;
    }
    let payload_len = u64::from_le_bytes(record[44..52].try_into().ok()?);
    if payload_len > PREBUILT_MODEL_MAX_BYTES as u64
        || payload_len as usize != record.len() - CACHE_HEADER_BYTES
    {
        return None;
    }
    let payload_start = CACHE_HEADER_BYTES;
    let payload = &record[payload_start..];
    if blake3::hash(payload).as_bytes() != &record[52..84] {
        return None;
    }
    record.copy_within(payload_start.., 0);
    record.truncate(payload_len as usize);
    Some(record)
}

fn is_cache_filename(name: &str) -> bool {
    name.strip_suffix(".rvemodel")
        .is_some_and(|key| is_hex_name(key, 64))
}

fn is_cache_temp_filename(name: &str) -> bool {
    let Some(name) = name.strip_prefix('.') else {
        return false;
    };
    let Some((key, suffix)) = name.split_once(".tmp-") else {
        return false;
    };
    let Some((process, counter)) = suffix.split_once('-') else {
        return false;
    };
    is_hex_name(key, 64)
        && !process.is_empty()
        && !counter.is_empty()
        && process.bytes().all(|byte| byte.is_ascii_digit())
        && counter.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_hex_name(name: &str, length: usize) -> bool {
    name.len() == length && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{
        CACHE_HEADER_BYTES, CACHE_MAGIC, CACHE_SCHEMA_VERSION, MIKKTSPACE_VERSION, ModelCache,
        STALE_MODEL_CACHE_TEMP_AGE, UFBX_VERSION, cache_key, decode_cache_record,
        is_cache_temp_filename, model_cache_dir, prune_cache_at, prune_model_cache,
    };
    use engine_assets::{Assets, PREBUILT_MODEL_MAX_BYTES};
    use std::fs::{self, OpenOptions};
    use std::time::{Duration, SystemTime};

    const FIXTURE: &[u8] =
        include_bytes!("../../../engine/engine-assets/tests/fixtures/skin_chain.fbx");

    #[test]
    fn cache_key_tracks_path_and_fbx_bytes() {
        assert_eq!(
            cache_key("/models/a.fbx", FIXTURE),
            cache_key("/models/a.fbx", FIXTURE)
        );
        assert_ne!(
            cache_key("/models/a.fbx", FIXTURE),
            cache_key("/models/b.fbx", FIXTURE)
        );
        assert_ne!(
            cache_key("/models/a.fbx", FIXTURE),
            cache_key("/models/a.fbx", b"changed")
        );
    }

    #[test]
    fn cache_reuses_and_rejects_corrupt_or_foreign_entries() {
        let root = temp_dir("rve-model-cache-test");
        let assets = Assets::new(root.path());
        let mut cache = ModelCache::new(model_cache_dir(root.path()), true);
        let (first, first_outcome) = cache.prepare(&assets, "/models/skin.fbx", FIXTURE).unwrap();
        assert_eq!(first_outcome, super::CacheOutcome::Miss);
        let (second, second_outcome) = cache.prepare(&assets, "/models/skin.fbx", FIXTURE).unwrap();
        assert_eq!(second_outcome, super::CacheOutcome::Hit);
        assert_eq!(second, first);

        let key = cache_key("/models/skin.fbx", FIXTURE);
        let entry = cache.entry_path(&key);
        fs::write(&entry, b"corrupt").unwrap();
        let (repaired, repaired_outcome) =
            cache.prepare(&assets, "/models/skin.fbx", FIXTURE).unwrap();
        assert_eq!(repaired_outcome, super::CacheOutcome::Rejected);
        assert_eq!(repaired, first);
        let bytes = fs::read(&entry).unwrap();
        assert!(bytes.starts_with(CACHE_MAGIC));

        let mut foreign = bytes;
        foreign[12] ^= 0x80;
        assert!(decode_cache_record(foreign, &key).is_none());
        let mut foreign = fs::read(&entry).unwrap();
        foreign[12] ^= 0x80;
        fs::write(&entry, foreign).unwrap();
        let (_, foreign_outcome) = cache.prepare(&assets, "/models/skin.fbx", FIXTURE).unwrap();
        assert_eq!(foreign_outcome, super::CacheOutcome::Rejected);
        assert!(fs::read(&entry).unwrap().starts_with(CACHE_MAGIC));

        let mut changed = FIXTURE.to_vec();
        changed.push(b'\n');
        let (_, changed_outcome) = cache
            .prepare(&assets, "/models/skin.fbx", &changed)
            .unwrap();
        assert_eq!(changed_outcome, super::CacheOutcome::Miss);
    }

    #[test]
    fn readonly_cache_does_not_create_or_repair_entries() {
        let cold_root = temp_dir("rve-model-cache-readonly-cold");
        let assets = Assets::new(cold_root.path());
        let cache_root = model_cache_dir(cold_root.path());
        let mut readonly = ModelCache::new(cache_root.clone(), false);
        let (_, outcome) = readonly
            .prepare(&assets, "/models/skin.fbx", FIXTURE)
            .unwrap();
        assert_eq!(outcome, super::CacheOutcome::Miss);
        assert!(!cache_root.exists());

        let warm_root = temp_dir("rve-model-cache-readonly-rejected");
        let assets = Assets::new(warm_root.path());
        let mut writer = ModelCache::new(model_cache_dir(warm_root.path()), true);
        writer
            .prepare(&assets, "/models/skin.fbx", FIXTURE)
            .unwrap();
        let key = cache_key("/models/skin.fbx", FIXTURE);
        let entry = writer.entry_path(&key);
        fs::write(&entry, b"corrupt entry").unwrap();
        let before = fs::read(&entry).unwrap();
        let modified = fs::metadata(&entry).unwrap().modified().unwrap();

        let mut readonly = ModelCache::new(model_cache_dir(warm_root.path()), false);
        let (_, outcome) = readonly
            .prepare(&assets, "/models/skin.fbx", FIXTURE)
            .unwrap();
        assert_eq!(outcome, super::CacheOutcome::Rejected);
        assert_eq!(fs::read(&entry).unwrap(), before);
        assert_eq!(fs::metadata(&entry).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn unavailable_cache_falls_back_to_conversion() {
        let root = temp_dir("rve-model-cache-unavailable");
        let cache_path = root.path().join(".rve/model-cache");
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(&cache_path, b"not a directory").unwrap();
        let assets = Assets::new(root.path());
        let mut cache = ModelCache::new(cache_path, true);

        let (bytes, outcome) = cache.prepare(&assets, "/models/skin.fbx", FIXTURE).unwrap();
        assert_eq!(outcome, super::CacheOutcome::Miss);
        assert!(engine_assets::is_prebuilt_model(&bytes));
    }

    #[test]
    fn model_cache_prune_evicts_oldest_entries_and_cleans_only_stale_temps() {
        let root = temp_dir("rve-model-cache-prune");
        let cache_root = model_cache_dir(root.path());
        let shard = cache_root.join("aa");
        fs::create_dir_all(&shard).unwrap();
        let oldest = shard.join(format!("{}.rvemodel", "a".repeat(64)));
        let newest = shard.join(format!("{}.rvemodel", "b".repeat(64)));
        fs::write(&oldest, b"old").unwrap();
        fs::write(&newest, b"new").unwrap();
        let base = SystemTime::now() - Duration::from_secs(10_000);
        for (path, time) in [(&oldest, base), (&newest, base + Duration::from_secs(10))] {
            OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        }

        let stale_temp = shard.join(format!(".{}.tmp-123-1", "c".repeat(64)));
        fs::write(&stale_temp, b"stale partial").unwrap();
        OpenOptions::new()
            .write(true)
            .open(&stale_temp)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_MODEL_CACHE_TEMP_AGE * 2)
            .unwrap();
        let fresh_temp = shard.join(format!(".{}.tmp-123-2", "d".repeat(64)));
        fs::write(&fresh_temp, b"writer in progress").unwrap();
        let foreign_temp = shard.join(format!(".{}.tmp-foreign", "e".repeat(64)));
        fs::write(&foreign_temp, b"foreign file").unwrap();
        let foreign = cache_root.join("notes.txt");
        fs::write(&foreign, b"foreign data larger than cache limit").unwrap();

        assert!(is_cache_temp_filename(
            stale_temp.file_name().unwrap().to_str().unwrap()
        ));
        assert!(!is_cache_temp_filename(
            foreign_temp.file_name().unwrap().to_str().unwrap()
        ));
        prune_cache_at(&cache_root, 3, |path| fs::remove_file(path)).unwrap();

        assert!(!oldest.exists());
        assert!(newest.exists());
        assert!(!stale_temp.exists());
        assert!(fresh_temp.exists());
        assert!(foreign_temp.exists());
        assert!(foreign.exists());
    }

    #[test]
    fn model_cache_prune_ignores_not_found_during_walk_and_eviction() {
        let root = temp_dir("rve-model-cache-prune-not-found");
        let cache_root = model_cache_dir(root.path());
        let shard = cache_root.join("aa");
        fs::create_dir_all(&shard).unwrap();
        let first = shard.join(format!("{}.rvemodel", "a".repeat(64)));
        let second = shard.join(format!("{}.rvemodel", "b".repeat(64)));
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        assert!(
            super::read_dir_if_exists(&cache_root.join("missing"))
                .unwrap()
                .is_none()
        );
        assert!(
            super::symlink_metadata_if_exists(&shard.join("missing"))
                .unwrap()
                .is_none()
        );

        let mut removed_before_race = false;
        prune_cache_at(&cache_root, 0, |path| {
            if !removed_before_race {
                removed_before_race = true;
                fs::remove_file(path)?;
            }
            fs::remove_file(path)
        })
        .unwrap();
        assert!(!first.exists());
        assert!(!second.exists());

        prune_model_cache(root.path(), 0).unwrap();
        assert!(prune_model_cache(&root.path().join("missing-project"), 0).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn model_cache_prune_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("rve-model-cache-prune-symlink");
        let cache_root = model_cache_dir(root.path());
        let shard = cache_root.join("aa");
        fs::create_dir_all(&shard).unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"target").unwrap();
        let link = shard.join(format!("{}.rvemodel", "a".repeat(64)));
        symlink(&target, &link).unwrap();

        prune_cache_at(&cache_root, 0, |path| fs::remove_file(path)).unwrap();

        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"target");
    }

    #[test]
    fn cache_record_decoder_rejects_oversized_payload_before_allocation() {
        let mut bytes = vec![0; CACHE_HEADER_BYTES];
        bytes[..8].copy_from_slice(CACHE_MAGIC);
        bytes[8..12].copy_from_slice(&CACHE_SCHEMA_VERSION.to_le_bytes());
        bytes[44..52].copy_from_slice(&((PREBUILT_MODEL_MAX_BYTES as u64) + 1).to_le_bytes());
        assert!(decode_cache_record(bytes, &[0; 32]).is_none());
    }

    #[test]
    fn converter_revision_versions_match_cargo_lock() {
        let lock = include_str!("../../../Cargo.lock");
        assert!(
            super::converter_revision()
                .contains(&format!("engine-assets-{}", engine_assets::VERSION))
        );
        assert!(super::converter_revision().contains(&format!("ufbx-{UFBX_VERSION}")));
        assert!(
            super::converter_revision().contains(&format!("bevy_mikktspace-{MIKKTSPACE_VERSION}"))
        );
        assert_eq!(locked_version(lock, "ufbx"), UFBX_VERSION);
        assert_eq!(locked_version(lock, "bevy_mikktspace"), MIKKTSPACE_VERSION);
    }

    fn locked_version<'a>(lock: &'a str, name: &str) -> &'a str {
        for package in lock.split("[[package]]") {
            let package_name = package
                .lines()
                .find_map(|line| line.strip_prefix("name = \""))
                .and_then(|line| line.strip_suffix('\"'));
            if package_name == Some(name) {
                return package
                    .lines()
                    .find_map(|line| line.strip_prefix("version = \""))
                    .and_then(|line| line.strip_suffix('\"'))
                    .expect("version line");
            }
        }
        panic!("package {name} is absent from Cargo.lock")
    }

    fn temp_dir(prefix: &str) -> super::TempDir {
        super::TempDir::create(&std::env::temp_dir(), prefix).unwrap()
    }
}
