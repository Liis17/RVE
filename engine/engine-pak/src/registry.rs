use crate::archive_name::{ArchiveNameError, validate_archive_stem};
use crate::{ArchiveCatalog, ArchiveId};
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

const REGISTRY_FORMAT: i64 = 1;
const REGISTRY_PATH: &str = "rve/assets.toml";

/// The archive names and declaring archive for every path in one archive set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveSetRegistry {
    archives: Vec<String>,
    assets: BTreeMap<VirtualPath, String>,
}

impl ArchiveSetRegistry {
    /// Creates a registry and validates its archive and path assignments.
    pub fn new(
        archives: Vec<String>,
        assets: BTreeMap<VirtualPath, String>,
    ) -> Result<Self, ArchiveSetRegistryError> {
        let registry = Self { archives, assets };
        registry.validate()?;
        Ok(registry)
    }

    /// Parses the versioned TOML registry stored at `/rve/assets.toml`.
    pub fn parse(source: &str) -> Result<Self, ArchiveSetRegistryError> {
        let raw: RawRegistry = toml::from_str(source).map_err(|error| {
            ArchiveSetRegistryError::new(format!("invalid registry TOML: {error}"))
        })?;
        if raw.format != REGISTRY_FORMAT {
            return Err(ArchiveSetRegistryError::new(format!(
                "unsupported registry version {}; supported version is {REGISTRY_FORMAT}",
                raw.format
            )));
        }

        let mut assets = BTreeMap::new();
        for (raw_path, archive) in raw.assets {
            let path = VirtualPath::parse(&raw_path).map_err(|error| {
                ArchiveSetRegistryError::new(format!("invalid registry path {raw_path:?}: {error}"))
            })?;
            if assets.insert(path.clone(), archive).is_some() {
                return Err(ArchiveSetRegistryError::new(format!(
                    "registry contains duplicate canonical path /{path}"
                )));
            }
        }
        Self::new(raw.archives, assets)
    }

    /// Serializes with archive order preserved and asset paths sorted canonically.
    pub fn to_toml(&self) -> Result<String, ArchiveSetRegistryError> {
        self.validate()?;
        let raw = RawRegistryOut {
            format: REGISTRY_FORMAT,
            archives: &self.archives,
            assets: self
                .assets
                .iter()
                .map(|(path, archive)| (format!("/{path}"), archive.clone()))
                .collect(),
        };
        toml::to_string(&raw).map_err(|error| {
            ArchiveSetRegistryError::new(format!("cannot serialize registry: {error}"))
        })
    }

    /// Archive names in the declared build order; `base` is always first.
    pub fn archives(&self) -> &[String] {
        &self.archives
    }

    /// Paths mapped to the archive whose file table declares each path.
    pub fn assets(&self) -> &BTreeMap<VirtualPath, String> {
        &self.assets
    }

    /// Checks registry invariants without opening archives.
    pub fn validate(&self) -> Result<(), ArchiveSetRegistryError> {
        if self.archives.is_empty() || self.archives.first().map(String::as_str) != Some("base") {
            return Err(ArchiveSetRegistryError::new(
                "archive list must start with required archive \"base\"",
            ));
        }

        let mut folded_names = BTreeSet::new();
        for name in &self.archives {
            validate_archive_name(name)?;
            if !folded_names.insert(name.to_ascii_lowercase()) {
                return Err(ArchiveSetRegistryError::new(format!(
                    "archive names collide without ASCII case sensitivity: {name}"
                )));
            }
        }

        let archive_names = self
            .archives
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let mut declared_counts = BTreeMap::<&str, usize>::new();
        for (path, archive) in &self.assets {
            if !archive_names.contains(archive.as_str()) {
                return Err(ArchiveSetRegistryError::new(format!(
                    "registry path /{path} refers to undeclared archive {archive:?}"
                )));
            }
            *declared_counts.entry(archive).or_default() += 1;
        }
        if self
            .assets
            .get(&VirtualPath::parse(REGISTRY_PATH).expect("constant virtual path"))
            .map(String::as_str)
            != Some("base")
        {
            return Err(ArchiveSetRegistryError::new(
                "registry path /rve/assets.toml must be declared by archive \"base\"",
            ));
        }
        for archive in self.archives.iter().skip(1) {
            if declared_counts
                .get(archive.as_str())
                .copied()
                .unwrap_or_default()
                == 0
            {
                return Err(ArchiveSetRegistryError::new(format!(
                    "archive {archive:?} declares no asset paths"
                )));
            }
        }
        Ok(())
    }

    /// Verifies the exact archive files, archive identities, and path ownership.
    ///
    /// `archive_ids` maps each logical archive name to the identity opened from
    /// `<name>.rvepak`; ownership is checked against the declaring archive, even
    /// when its blob is externally stored in another archive.
    pub fn validate_against_catalog(
        &self,
        catalog: &ArchiveCatalog,
        archive_ids: &BTreeMap<String, ArchiveId>,
        archive_directory: impl AsRef<Path>,
    ) -> Result<(), ArchiveSetRegistryError> {
        self.validate_archive_files(archive_directory)?;
        self.validate()?;

        let expected_names = self
            .archives
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let actual_names = archive_ids
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if actual_names != expected_names {
            let missing = expected_names.difference(&actual_names).next();
            if let Some(name) = missing {
                return Err(ArchiveSetRegistryError::new(format!(
                    "archive {name}.rvepak is listed but was not opened"
                )));
            }
            let extra = actual_names.difference(&expected_names).next().unwrap();
            return Err(ArchiveSetRegistryError::new(format!(
                "opened archive {extra}.rvepak is not listed in the registry"
            )));
        }

        let catalog_ids = catalog.archive_ids().collect::<BTreeSet<_>>();
        let mut name_by_id = BTreeMap::new();
        for (name, id) in archive_ids {
            if !catalog_ids.contains(id) {
                return Err(ArchiveSetRegistryError::new(format!(
                    "archive {name}.rvepak identity is absent from the catalog"
                )));
            }
            if let Some(previous) = name_by_id.insert(*id, name.as_str()) {
                return Err(ArchiveSetRegistryError::new(format!(
                    "archives {previous}.rvepak and {name}.rvepak share one ArchiveId"
                )));
            }
        }
        if name_by_id.len() != catalog_ids.len() {
            return Err(ArchiveSetRegistryError::new(
                "catalog contains an archive without a registry name",
            ));
        }
        for (id, path) in catalog.archive_paths() {
            if let Some(name) = name_by_id.get(&id) {
                let expected_file = format!("{name}.rvepak");
                let actual_file =
                    path.file_name()
                        .and_then(|file| file.to_str())
                        .ok_or_else(|| {
                            ArchiveSetRegistryError::new(format!(
                                "archive filename is not valid UTF-8: {}",
                                path.display()
                            ))
                        })?;
                if actual_file != expected_file {
                    return Err(ArchiveSetRegistryError::new(format!(
                        "catalog file {actual_file} was mapped to archive name {name:?}"
                    )));
                }
            }
        }

        let entries = catalog.entries().map_err(|error| {
            ArchiveSetRegistryError::new(format!("catalog validation failed: {error}"))
        })?;
        let mut catalog_paths = BTreeSet::new();
        for entry in entries {
            catalog_paths.insert(entry.path.clone());
            let declared_name = name_by_id.get(&entry.archive_id).ok_or_else(|| {
                ArchiveSetRegistryError::new(format!(
                    "catalog path /{} is declared by an unnamed archive",
                    entry.path
                ))
            })?;
            match self.assets.get(&entry.path) {
                None => {
                    return Err(ArchiveSetRegistryError::new(format!(
                        "catalog path /{} is missing from the registry",
                        entry.path
                    )));
                }
                Some(registry_name) if registry_name != declared_name => {
                    return Err(ArchiveSetRegistryError::new(format!(
                        "catalog path /{} is declared by {declared_name}.rvepak but registry assigns it to {registry_name}.rvepak",
                        entry.path
                    )));
                }
                Some(_) => {}
            }
        }
        for path in self.assets.keys() {
            if !catalog_paths.contains(path) {
                return Err(ArchiveSetRegistryError::new(format!(
                    "registry path /{path} is missing from the archive catalog"
                )));
            }
        }
        Ok(())
    }

    /// Confirms that the directory contains exactly the declared `.rvepak` files.
    pub fn validate_archive_files(
        &self,
        archive_directory: impl AsRef<Path>,
    ) -> Result<(), ArchiveSetRegistryError> {
        self.validate()?;
        let directory = archive_directory.as_ref();
        let entries = fs::read_dir(directory).map_err(|error| {
            ArchiveSetRegistryError::new(format!(
                "cannot inspect archive directory {}: {error}",
                directory.display()
            ))
        })?;
        let mut actual = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                ArchiveSetRegistryError::new(format!("cannot read archive directory: {error}"))
            })?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("rvepak"))
            {
                if !entry
                    .file_type()
                    .map_err(|error| {
                        ArchiveSetRegistryError::new(format!(
                            "cannot inspect archive file {file_name}: {error}"
                        ))
                    })?
                    .is_file()
                {
                    return Err(ArchiveSetRegistryError::new(format!(
                        "archive entry {file_name} is not a regular file"
                    )));
                }
                actual.insert(file_name);
            }
        }
        let expected = self
            .archives
            .iter()
            .map(|name| format!("{name}.rvepak"))
            .collect::<BTreeSet<_>>();
        if let Some(missing) = expected.difference(&actual).next() {
            if let Some(actual_case) = actual
                .iter()
                .find(|actual| actual.eq_ignore_ascii_case(missing))
            {
                return Err(ArchiveSetRegistryError::new(format!(
                    "archive filename has incorrect case: expected {missing}, found {actual_case}"
                )));
            }
            return Err(ArchiveSetRegistryError::new(format!(
                "archive file is missing: {missing}"
            )));
        }
        if let Some(extra) = actual.difference(&expected).next() {
            return Err(ArchiveSetRegistryError::new(format!(
                "unexpected archive file: {extra}"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ArchiveSetRegistryError {
    message: String,
}

impl ArchiveSetRegistryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ArchiveSetRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ArchiveSetRegistryError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistry {
    format: i64,
    archives: Vec<String>,
    assets: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct RawRegistryOut<'a> {
    format: i64,
    archives: &'a [String],
    assets: BTreeMap<String, String>,
}

fn validate_archive_name(name: &str) -> Result<(), ArchiveSetRegistryError> {
    match validate_archive_stem(name) {
        Ok(()) => Ok(()),
        Err(ArchiveNameError::InvalidStem) => Err(ArchiveSetRegistryError::new(format!(
            "unsafe archive name {name:?}; expected 1-64 ASCII letters, digits, underscores, or hyphens"
        ))),
        Err(ArchiveNameError::ReservedWindowsName) => Err(ArchiveSetRegistryError::new(format!(
            "reserved Windows archive name {name:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::ArchiveSetRegistry;
    use crate::{
        Archive, ArchiveCatalog, ArchiveId, ArchiveWriter, BuildCompression, BuildFileInput,
        BuildOptions, ProgrammaticArchive, build_archive_set,
    };
    use engine_files::VirtualPath;
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("rve-registry-{}-{sequence}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    fn valid_registry() -> ArchiveSetRegistry {
        ArchiveSetRegistry::new(
            vec!["base".into(), "shared".into()],
            BTreeMap::from([
                (path("/rve/assets.toml"), "base".into()),
                (path("/rve/project.toml"), "base".into()),
                (path("/models/cube.fbx"), "shared".into()),
            ]),
        )
        .unwrap()
    }

    fn write_archive(root: &Path, name: &str, entries: &[(&str, &[u8])]) {
        let mut writer = ArchiveWriter::new(
            File::create(root.join(format!("{name}.rvepak"))).unwrap(),
            Default::default(),
        )
        .unwrap();
        for (virtual_path, bytes) in entries {
            writer.add_local(path(virtual_path), *bytes, None).unwrap();
        }
        writer.finish().unwrap();
    }

    fn open_catalog(root: &Path) -> (ArchiveCatalog, BTreeMap<String, ArchiveId>) {
        let catalog =
            ArchiveCatalog::open_directory(root, Default::default(), Default::default()).unwrap();
        let archive_ids = catalog
            .archive_paths()
            .map(|(id, path)| (path.file_stem().unwrap().to_string_lossy().into_owned(), id))
            .collect();
        (catalog, archive_ids)
    }

    #[test]
    fn registry_round_trips_and_serializes_assets_in_path_order() {
        let registry = valid_registry();
        let first = registry.to_toml().unwrap();
        assert!(first.find("base").unwrap() < first.find("shared").unwrap());
        assert!(first.find("/models/cube.fbx").unwrap() < first.find("/rve/assets.toml").unwrap());
        assert_eq!(ArchiveSetRegistry::parse(&first).unwrap(), registry);
        assert_eq!(registry.to_toml().unwrap(), first);
    }

    #[test]
    fn registry_rejects_unsupported_version_unknown_fields_and_invalid_archive_rules() {
        let unsupported = ArchiveSetRegistry::parse(
            "format = 2\narchives = [\"base\"]\n[assets]\n\"/rve/assets.toml\" = \"base\"\n",
        )
        .unwrap_err();
        assert!(
            unsupported
                .to_string()
                .contains("unsupported registry version")
        );

        assert!(ArchiveSetRegistry::parse(
            "format = 1\narchives = [\"base\"]\nunknown = true\n[assets]\n\"/rve/assets.toml\" = \"base\"\n"
        )
        .is_err());

        let base_asset = (path("/rve/assets.toml"), "base".to_owned());
        for archives in [
            vec![],
            vec!["shared".into(), "base".into()],
            vec!["base".into(), "Base".into()],
            vec!["base".into(), "bad.name".into()],
            vec!["base".into(), "x".repeat(65)],
            vec!["base".into(), "CON".into()],
            vec!["base".into(), "com1".into()],
        ] {
            assert!(
                ArchiveSetRegistry::new(archives, BTreeMap::from([base_asset.clone()])).is_err()
            );
        }
        assert!(
            ArchiveSetRegistry::new(
                vec!["base".into(), "empty".into()],
                BTreeMap::from([base_asset.clone()]),
            )
            .unwrap_err()
            .to_string()
            .contains("declares no asset paths")
        );
        assert!(
            ArchiveSetRegistry::new(
                vec!["base".into()],
                BTreeMap::from([(path("/rve/other.toml"), "base".into())]),
            )
            .unwrap_err()
            .to_string()
            .contains("/rve/assets.toml must be declared by archive \"base\"")
        );
        assert!(
            ArchiveSetRegistry::new(
                vec!["base".into()],
                BTreeMap::from([
                    base_asset.clone(),
                    (path("/models/cube.fbx"), "missing".into())
                ]),
            )
            .unwrap_err()
            .to_string()
            .contains("undeclared archive")
        );

        let duplicate_canonical = ArchiveSetRegistry::parse(
            "format = 1\narchives = [\"base\"]\n[assets]\n\"/rve/assets.toml\" = \"base\"\n\"rve/assets.toml\" = \"base\"\n",
        )
        .unwrap_err();
        assert!(
            duplicate_canonical
                .to_string()
                .contains("duplicate canonical path")
        );
    }

    #[test]
    fn archive_file_set_reports_extra_and_missing_names() {
        let temp = TempDirectory::new();
        let registry = valid_registry();
        write_archive(temp.path(), "base", &[("/rve/assets.toml", b"registry")]);
        write_archive(temp.path(), "shared", &[("/models/cube.fbx", b"mesh")]);
        write_archive(temp.path(), "extra", &[("/other", b"x")]);
        assert!(
            registry
                .validate_archive_files(temp.path())
                .unwrap_err()
                .to_string()
                .contains("extra.rvepak")
        );
        fs::remove_file(temp.path().join("extra.rvepak")).unwrap();
        fs::remove_file(temp.path().join("shared.rvepak")).unwrap();
        assert!(
            registry
                .validate_archive_files(temp.path())
                .unwrap_err()
                .to_string()
                .contains("shared.rvepak")
        );
    }

    #[test]
    fn archive_file_set_reports_the_actual_case_mismatched_filename() {
        let temp = TempDirectory::new();
        let registry = valid_registry();
        write_archive(temp.path(), "Base", &[("/rve/assets.toml", b"registry")]);
        write_archive(temp.path(), "shared", &[("/models/cube.fbx", b"mesh")]);

        let error = registry.validate_archive_files(temp.path()).unwrap_err();
        assert!(error.to_string().contains("found Base.rvepak"));
    }

    #[test]
    fn archive_file_set_ignores_other_entries_and_rejects_non_file_archives() {
        let temp = TempDirectory::new();
        let registry = valid_registry();
        write_archive(temp.path(), "base", &[("/rve/assets.toml", b"registry")]);
        write_archive(temp.path(), "shared", &[("/models/cube.fbx", b"mesh")]);
        fs::write(temp.path().join("readme.txt"), b"not an archive").unwrap();
        fs::create_dir(temp.path().join("subdirectory")).unwrap();
        registry.validate_archive_files(temp.path()).unwrap();

        let archive_directory = temp.path().join("directory.rvepak");
        fs::create_dir(&archive_directory).unwrap();
        assert!(
            registry
                .validate_archive_files(temp.path())
                .unwrap_err()
                .to_string()
                .contains("directory.rvepak is not a regular file")
        );
        fs::remove_dir(&archive_directory).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            symlink(
                temp.path().join("shared.rvepak"),
                temp.path().join("linked.rvepak"),
            )
            .unwrap();
            assert!(
                registry
                    .validate_archive_files(temp.path())
                    .unwrap_err()
                    .to_string()
                    .contains("linked.rvepak is not a regular file")
            );
        }
    }

    #[test]
    fn catalog_and_registry_paths_must_match_both_ways_and_by_declaring_archive() {
        let temp = TempDirectory::new();
        write_archive(
            temp.path(),
            "base",
            &[
                ("/rve/assets.toml", b"registry"),
                ("/rve/project.toml", b"project"),
            ],
        );
        write_archive(
            temp.path(),
            "shared",
            &[
                ("/models/cube.fbx", b"mesh"),
                ("/models/catalog-only", b"x"),
            ],
        );
        let (catalog, archive_ids) = open_catalog(temp.path());
        let mut registry = valid_registry();
        let swapped_ids = BTreeMap::from([
            ("base".to_owned(), archive_ids["shared"]),
            ("shared".to_owned(), archive_ids["base"]),
        ]);
        assert!(
            registry
                .validate_against_catalog(&catalog, &swapped_ids, temp.path())
                .unwrap_err()
                .to_string()
                .contains("was mapped to archive name")
        );
        assert!(
            registry
                .validate_against_catalog(&catalog, &archive_ids, temp.path())
                .unwrap_err()
                .to_string()
                .contains("catalog path /models/catalog-only is missing from the registry")
        );

        let mut assets = registry.assets.clone();
        assets.insert(path("/models/catalog-only"), "shared".into());
        assets.insert(path("/models/registry-only"), "shared".into());
        registry = ArchiveSetRegistry::new(registry.archives.clone(), assets).unwrap();
        assert!(
            registry
                .validate_against_catalog(&catalog, &archive_ids, temp.path())
                .unwrap_err()
                .to_string()
                .contains(
                    "registry path /models/registry-only is missing from the archive catalog"
                )
        );

        let mut assets = registry.assets.clone();
        assets.remove(&path("/models/registry-only"));
        assets.insert(path("/models/catalog-only"), "base".into());
        registry = ArchiveSetRegistry::new(registry.archives.clone(), assets).unwrap();
        assert!(
            registry
                .validate_against_catalog(&catalog, &archive_ids, temp.path())
                .unwrap_err()
                .to_string()
                .contains("declared by shared.rvepak but registry assigns it to base.rvepak")
        );
    }

    #[test]
    fn catalog_validation_accepts_an_alias_declared_in_the_same_archive() {
        let temp = TempDirectory::new();
        let mut base_writer = ArchiveWriter::new(
            File::create(temp.path().join("base.rvepak")).unwrap(),
            Default::default(),
        )
        .unwrap();
        let registry_blob = base_writer
            .add_local(path("/rve/assets.toml"), &b"registry"[..], None)
            .unwrap();
        base_writer
            .add_local(path("/rve/project.toml"), &b"project"[..], None)
            .unwrap();
        base_writer
            .add_alias(path("/rve/assets-copy.toml"), registry_blob)
            .unwrap();
        base_writer.finish().unwrap();
        write_archive(temp.path(), "shared", &[("/models/cube.fbx", b"mesh")]);

        let registry = ArchiveSetRegistry::new(
            vec!["base".into(), "shared".into()],
            BTreeMap::from([
                (path("/rve/assets.toml"), "base".into()),
                (path("/rve/project.toml"), "base".into()),
                (path("/rve/assets-copy.toml"), "base".into()),
                (path("/models/cube.fbx"), "shared".into()),
            ]),
        )
        .unwrap();
        let (catalog, archive_ids) = open_catalog(temp.path());

        registry
            .validate_against_catalog(&catalog, &archive_ids, temp.path())
            .unwrap();
        assert!(catalog.stat(&path("/rve/assets-copy.toml")).is_ok());
    }

    #[test]
    fn deduplicated_external_path_is_owned_by_declaring_archive_and_base_opens_alone() {
        let temp = TempDirectory::new();
        let source = temp.path().join("source");
        let output = temp.path().join("out");
        fs::create_dir_all(&source).unwrap();
        let assets = BTreeMap::from([
            (path("/rve/assets.toml"), "base".to_owned()),
            (path("/rve/project.toml"), "base".to_owned()),
            (path("/models/cube.fbx"), "base".to_owned()),
            (path("/models/cube-copy.fbx"), "shared".to_owned()),
        ]);
        let registry =
            ArchiveSetRegistry::new(vec!["base".into(), "shared".into()], assets).unwrap();
        fs::write(source.join("registry.toml"), registry.to_toml().unwrap()).unwrap();
        fs::write(source.join("project.toml"), b"project").unwrap();
        fs::write(source.join("cube.bin"), b"same bytes").unwrap();
        fs::write(source.join("cube-copy.bin"), b"same bytes").unwrap();
        let report = build_archive_set(
            vec![
                ProgrammaticArchive {
                    name: "base".into(),
                    inputs: vec![
                        BuildFileInput::new(source.join("registry.toml"), path("/rve/assets.toml")),
                        BuildFileInput::new(source.join("project.toml"), path("/rve/project.toml")),
                        BuildFileInput::new(source.join("cube.bin"), path("/models/cube.fbx")),
                    ],
                },
                ProgrammaticArchive {
                    name: "shared".into(),
                    inputs: vec![BuildFileInput::new(
                        source.join("cube-copy.bin"),
                        path("/models/cube-copy.fbx"),
                    )],
                },
            ],
            &output,
            BuildCompression::default(),
            BuildOptions::default(),
        )
        .unwrap();
        assert_eq!(report.archives[0].name, "base.rvepak");

        let (catalog, archive_ids) = open_catalog(&output);
        let registry = ArchiveSetRegistry::parse(
            &String::from_utf8(catalog.read_all(&path("/rve/assets.toml")).unwrap()).unwrap(),
        )
        .unwrap();
        registry
            .validate_against_catalog(&catalog, &archive_ids, &output)
            .unwrap();

        let shared = catalog
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == path("/models/cube-copy.fbx"))
            .unwrap();
        assert_eq!(shared.archive_id, archive_ids["shared"]);
        assert_eq!(shared.owner_archive_id, archive_ids["base"]);

        let base = Archive::open_file(output.join("base.rvepak")).unwrap();
        assert_eq!(
            base.read_all(&path("/models/cube.fbx")).unwrap(),
            b"same bytes"
        );
        assert_eq!(
            base.read_all(&path("/rve/assets.toml")).unwrap(),
            fs::read(source.join("registry.toml")).unwrap()
        );
    }
}
