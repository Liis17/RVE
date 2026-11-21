//! Disk cache of compressed RVEPAK chunks for repeated builds.
//!
//! An entry stores the encoder output for one decoded chunk. The key covers the decoded
//! bytes, the codec and its parameters, the encoder library revision and the written format
//! version, so a hit returns exactly the bytes the encoder would produce again. Entries are
//! validated on every read; a damaged or foreign entry is treated as a miss and replaced.

use crate::format::{Codec, VERSION};
use crate::writer::{WRITER_ZSTD_WINDOW_LOG, encode_chunk};
use crate::{PakError, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Default size limit applied by [`BlockCache::prune`]: 2 GiB of entry files.
pub const DEFAULT_BLOCK_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

const ENTRY_MAGIC: [u8; 8] = *b"RVEBLKC\0";
/// Layout and key schema of cache entries. Bump when either changes.
const CACHE_SCHEMA: u32 = 1;
/// The `lz4_flex` release whose block output is cached; kept in sync with `Cargo.lock`
/// by a unit test, because the crate exposes no runtime version.
const LZ4_FLEX_VERSION: &str = "0.14.0";
const KEY_CONTEXT: &str = "RVE RVEPAK compressed block cache key";
/// magic + schema + key + decoded_len + stored_len + stored BLAKE3.
const ENTRY_HEADER_SIZE: usize = 8 + 4 + 32 + 8 + 8 + 32;
const ENTRY_EXTENSION: &str = "blk";
const TEMP_EXTENSION: &str = "tmp";
/// Temporary files older than this are leftovers of an interrupted writer.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Counters of one [`BlockCache`] since it was opened.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockCacheStats {
    /// Chunks whose stored bytes came from the cache.
    pub hits: u64,
    /// Chunks compressed by the encoder because no valid entry existed.
    pub misses: u64,
    /// Existing entries rejected as damaged or mismatched; each also counts as a miss.
    pub rejected: u64,
    /// Entries that could not be written; the build continues with the encoded bytes.
    pub write_failures: u64,
    /// Entries removed by [`BlockCache::prune`].
    pub evicted: u64,
}

/// Content-addressed store of compressed chunks inside one directory.
///
/// Only files named `<2 hex>/<64 hex>.blk` and this cache's own temporary files are
/// ever removed; other files in the directory are left untouched.
#[derive(Debug)]
pub struct BlockCache {
    root: PathBuf,
    max_bytes: u64,
    stats: BlockCacheStats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyParams<'a> {
    schema: u32,
    format_version: u32,
    codec: Codec,
    zstd_level: i32,
    encoder_revision: &'a str,
}

impl BlockCache {
    /// Opens or creates the cache directory. `max_bytes` is enforced by [`Self::prune`].
    pub fn open(root: impl AsRef<Path>, max_bytes: u64) -> Result<Self> {
        let root = root.as_ref();
        fs::create_dir_all(root).map_err(|error| PakError::io_path(root.to_path_buf(), error))?;
        let metadata = fs::symlink_metadata(root)
            .map_err(|error| PakError::io_path(root.to_path_buf(), error))?;
        if !metadata.is_dir() {
            return Err(PakError::InvalidConfiguration(format!(
                "block cache root is not a real directory: {}",
                root.display()
            )));
        }
        Ok(Self {
            root: root.to_path_buf(),
            max_bytes,
            stats: BlockCacheStats::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn stats(&self) -> BlockCacheStats {
        self.stats
    }

    /// Returns stored bytes for one chunk, from the cache or by encoding and caching it.
    /// `Codec::None` is never cached: its stored bytes are the input.
    pub(crate) fn encode(
        &mut self,
        codec: Codec,
        decoded: &[u8],
        zstd_level: i32,
        chunk_hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        if codec == Codec::None {
            return encode_chunk(codec, decoded, zstd_level);
        }
        let encoder_revision = encoder_revision(codec);
        let key = block_key(
            KeyParams {
                schema: CACHE_SCHEMA,
                format_version: VERSION,
                codec,
                zstd_level,
                encoder_revision: &encoder_revision,
            },
            decoded.len() as u64,
            chunk_hash,
        );
        let path = self.entry_path(&key);
        match read_entry(&path, &key, decoded.len() as u64) {
            Ok(Some(stored)) => {
                self.stats.hits += 1;
                touch(&path);
                return Ok(stored);
            }
            Ok(None) => {}
            Err(_) => self.stats.rejected += 1,
        }
        self.stats.misses += 1;
        let stored = encode_chunk(codec, decoded, zstd_level)?;
        if write_entry(&path, &key, decoded.len() as u64, &stored).is_err() {
            self.stats.write_failures += 1;
        }
        Ok(stored)
    }

    /// Removes the least recently used entries until the cache fits `max_bytes`, and stale
    /// temporary files. Returns the number of removed entries.
    pub fn prune(&mut self) -> Result<u64> {
        let mut entries = Vec::new();
        let now = SystemTime::now();
        let shards = fs::read_dir(&self.root)
            .map_err(|error| PakError::io_path(self.root.clone(), error))?;
        for shard in shards {
            let shard = shard.map_err(|error| PakError::io_path(self.root.clone(), error))?;
            let shard_path = shard.path();
            if !is_shard_name(&shard.file_name())
                || !shard.file_type().is_ok_and(|kind| kind.is_dir())
            {
                continue;
            }
            let files = fs::read_dir(&shard_path)
                .map_err(|error| PakError::io_path(shard_path.clone(), error))?;
            for file in files {
                let file = file.map_err(|error| PakError::io_path(shard_path.clone(), error))?;
                let path = file.path();
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                match classify_file(&file.file_name()) {
                    Some(CacheFile::Entry) => entries.push((modified, path, metadata.len())),
                    Some(CacheFile::Temp)
                        if now
                            .duration_since(modified)
                            .is_ok_and(|age| age >= STALE_TEMP_AGE) =>
                    {
                        let _ = fs::remove_file(&path);
                    }
                    _ => {}
                }
            }
        }
        let mut total = entries
            .iter()
            .fold(0_u64, |total, (_, _, len)| total.saturating_add(*len));
        entries.sort();
        let mut evicted = 0;
        for (_, path, len) in entries {
            if total <= self.max_bytes {
                break;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    total = total.saturating_sub(len);
                    evicted += 1;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    total = total.saturating_sub(len);
                }
                Err(error) => return Err(PakError::io_path(path, error)),
            }
        }
        self.stats.evicted += evicted;
        Ok(evicted)
    }

    fn entry_path(&self, key: &[u8; 32]) -> PathBuf {
        let hex = to_hex(key);
        self.root
            .join(&hex[..2])
            .join(format!("{hex}.{ENTRY_EXTENSION}"))
    }
}

fn encoder_revision(codec: Codec) -> String {
    match codec {
        Codec::None => String::new(),
        Codec::Zstd => format!(
            "zstd {} window {WRITER_ZSTD_WINDOW_LOG}",
            zstd::zstd_safe::version_number()
        ),
        Codec::Lz4 => format!("lz4_flex {LZ4_FLEX_VERSION}"),
    }
}

fn block_key(params: KeyParams<'_>, decoded_len: u64, chunk_hash: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(KEY_CONTEXT);
    hasher.update(&params.schema.to_le_bytes());
    hasher.update(&params.format_version.to_le_bytes());
    hasher.update(&[params.codec.wire_tag()]);
    // The level does not affect LZ4 output, so it is not part of an LZ4 key.
    let level = if params.codec == Codec::Zstd {
        params.zstd_level
    } else {
        0
    };
    hasher.update(&level.to_le_bytes());
    hasher.update(&(params.encoder_revision.len() as u64).to_le_bytes());
    hasher.update(params.encoder_revision.as_bytes());
    hasher.update(&decoded_len.to_le_bytes());
    hasher.update(chunk_hash);
    *hasher.finalize().as_bytes()
}

/// Upper bound of stored bytes accepted from an entry; above every codec's worst case.
fn max_stored_len(decoded_len: u64) -> u64 {
    decoded_len
        .saturating_add(decoded_len / 64)
        .saturating_add(4096)
}

/// `Ok(None)` means no entry; `Err` means an entry exists but cannot be trusted.
fn read_entry(path: &Path, key: &[u8; 32], decoded_len: u64) -> io::Result<Option<Vec<u8>>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut header = [0_u8; ENTRY_HEADER_SIZE];
    file.read_exact(&mut header)?;
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid block cache entry");
    let field = |range: std::ops::Range<usize>| &header[range];
    let u64_at =
        |offset: usize| u64::from_le_bytes(header[offset..offset + 8].try_into().expect("8 bytes"));
    if field(0..8) != ENTRY_MAGIC
        || field(8..12) != CACHE_SCHEMA.to_le_bytes()
        || field(12..44) != key
        || u64_at(44) != decoded_len
    {
        return Err(invalid());
    }
    let stored_len = u64_at(52);
    if stored_len > max_stored_len(decoded_len)
        || file.metadata()?.len() != ENTRY_HEADER_SIZE as u64 + stored_len
    {
        return Err(invalid());
    }
    let mut stored = vec![0_u8; usize::try_from(stored_len).map_err(|_| invalid())?];
    file.read_exact(&mut stored)?;
    if blake3::hash(&stored).as_bytes() != field(60..92) {
        return Err(invalid());
    }
    Ok(Some(stored))
}

fn write_entry(path: &Path, key: &[u8; 32], decoded_len: u64, stored: &[u8]) -> io::Result<()> {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let directory = path.parent().expect("entry path has a shard directory");
    fs::create_dir_all(directory)?;
    let temp = directory.join(format!(
        "{}.{}-{}.{TEMP_EXTENSION}",
        to_hex(key),
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let mut header = Vec::with_capacity(ENTRY_HEADER_SIZE);
        header.extend_from_slice(&ENTRY_MAGIC);
        header.extend_from_slice(&CACHE_SCHEMA.to_le_bytes());
        header.extend_from_slice(key);
        header.extend_from_slice(&decoded_len.to_le_bytes());
        header.extend_from_slice(&(stored.len() as u64).to_le_bytes());
        header.extend_from_slice(blake3::hash(stored).as_bytes());
        file.write_all(&header)?;
        file.write_all(stored)?;
        drop(file);
        // Readers see either the previous entry or the complete new one.
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Marks an entry as recently used for [`BlockCache::prune`]; failures only affect eviction order.
fn touch(path: &Path) {
    if let Ok(file) = OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

enum CacheFile {
    Entry,
    Temp,
}

fn classify_file(name: &std::ffi::OsStr) -> Option<CacheFile> {
    let name = name.to_str()?;
    let (stem, extension) = name.rsplit_once('.')?;
    let hex = stem.get(..64)?;
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    match extension {
        ENTRY_EXTENSION if stem.len() == 64 => Some(CacheFile::Entry),
        TEMP_EXTENSION if stem.as_bytes().get(64) == Some(&b'.') => Some(CacheFile::Temp),
        _ => None,
    }
}

fn is_shard_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn to_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn temp_dir(label: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rvepak-block-cache-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        path
    }

    fn chunk(byte: u8) -> (Vec<u8>, [u8; 32]) {
        let data = vec![byte; 4096];
        let hash = *blake3::hash(&data).as_bytes();
        (data, hash)
    }

    fn entry_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for shard in fs::read_dir(root).unwrap() {
            let shard = shard.unwrap().path();
            if shard.is_dir() {
                for file in fs::read_dir(shard).unwrap() {
                    let file = file.unwrap().path();
                    if file.extension().is_some_and(|ext| ext == ENTRY_EXTENSION) {
                        files.push(file);
                    }
                }
            }
        }
        files.sort();
        files
    }

    #[test]
    fn hit_returns_encoder_output_without_encoding_again() {
        let root = temp_dir("hit");
        let (data, hash) = chunk(7);
        for codec in [Codec::Zstd, Codec::Lz4] {
            let expected = encode_chunk(codec, &data, 3).unwrap();
            let mut cache = BlockCache::open(&root, u64::MAX).unwrap();
            assert_eq!(cache.encode(codec, &data, 3, &hash).unwrap(), expected);
            let mut cache = BlockCache::open(&root, u64::MAX).unwrap();
            assert_eq!(cache.encode(codec, &data, 3, &hash).unwrap(), expected);
            assert_eq!(
                cache.stats(),
                BlockCacheStats {
                    hits: 1,
                    ..BlockCacheStats::default()
                }
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uncompressed_chunks_bypass_the_cache() {
        let root = temp_dir("none");
        let (data, hash) = chunk(1);
        let mut cache = BlockCache::open(&root, u64::MAX).unwrap();
        assert_eq!(cache.encode(Codec::None, &data, 3, &hash).unwrap(), data);
        assert_eq!(cache.stats(), BlockCacheStats::default());
        assert!(entry_files(&root).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn key_changes_with_codec_parameters_and_versions() {
        let (data, hash) = chunk(3);
        let base = KeyParams {
            schema: CACHE_SCHEMA,
            format_version: VERSION,
            codec: Codec::Zstd,
            zstd_level: 3,
            encoder_revision: "zstd 10507 window 23",
        };
        let key = |params| block_key(params, data.len() as u64, &hash);
        let variants = [
            KeyParams {
                schema: CACHE_SCHEMA + 1,
                ..base
            },
            KeyParams {
                format_version: VERSION + 1,
                ..base
            },
            KeyParams {
                codec: Codec::Lz4,
                ..base
            },
            KeyParams {
                zstd_level: 4,
                ..base
            },
            KeyParams {
                encoder_revision: "zstd 10508 window 23",
                ..base
            },
        ];
        for variant in variants {
            assert_ne!(key(variant), key(base), "{variant:?}");
        }
        assert_ne!(
            block_key(base, data.len() as u64 + 1, &hash),
            key(base),
            "decoded length is part of the key"
        );
        let lz4 = KeyParams {
            codec: Codec::Lz4,
            ..base
        };
        assert_eq!(
            key(KeyParams {
                zstd_level: 9,
                ..lz4
            }),
            key(lz4)
        );
    }

    #[test]
    fn damaged_or_foreign_entries_are_recompressed_and_replaced() {
        let root = temp_dir("damage");
        let (data, hash) = chunk(9);
        let expected = encode_chunk(Codec::Zstd, &data, 3).unwrap();
        let damage: [fn(&mut Vec<u8>); 4] = [
            |bytes| *bytes.last_mut().unwrap() ^= 0xff,
            |bytes| bytes.truncate(bytes.len() - 1),
            |bytes| bytes[8] ^= 1, // schema version
            |bytes| bytes.truncate(10),
        ];
        BlockCache::open(&root, u64::MAX)
            .unwrap()
            .encode(Codec::Zstd, &data, 3, &hash)
            .unwrap();
        let entry = entry_files(&root).remove(0);
        let pristine = fs::read(&entry).unwrap();
        for corrupt in damage {
            let mut bytes = pristine.clone();
            corrupt(&mut bytes);
            fs::write(&entry, bytes).unwrap();
            let mut cache = BlockCache::open(&root, u64::MAX).unwrap();
            assert_eq!(
                cache.encode(Codec::Zstd, &data, 3, &hash).unwrap(),
                expected
            );
            assert_eq!(cache.stats().rejected, 1);
            assert_eq!(cache.stats().misses, 1);
            assert_eq!(fs::read(&entry).unwrap(), pristine, "entry was rewritten");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prune_evicts_least_recently_used_entries_and_keeps_foreign_files() {
        let root = temp_dir("prune");
        let mut cache = BlockCache::open(&root, u64::MAX).unwrap();
        for byte in 0..3 {
            let (data, hash) = chunk(byte);
            cache.encode(Codec::Lz4, &data, 3, &hash).unwrap();
        }
        let entries = entry_files(&root);
        let entry_len = fs::metadata(&entries[0]).unwrap().len();
        let base = SystemTime::now() - Duration::from_secs(1000);
        for (age, entry) in entries.iter().enumerate() {
            let file = OpenOptions::new().write(true).open(entry).unwrap();
            file.set_modified(base + Duration::from_secs(age as u64 * 10))
                .unwrap();
        }
        let foreign = root.join("notes.txt");
        fs::write(&foreign, vec![0_u8; 100_000]).unwrap();
        let stale_temp = entries[0].with_file_name(format!("{}.1-1.tmp", "a".repeat(64)));
        fs::write(&stale_temp, b"partial").unwrap();
        OpenOptions::new()
            .write(true)
            .open(&stale_temp)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_TEMP_AGE * 2)
            .unwrap();
        let fresh_temp = entries[0].with_file_name(format!("{}.1-2.tmp", "b".repeat(64)));
        fs::write(&fresh_temp, b"in progress").unwrap();

        let mut cache = BlockCache::open(&root, entry_len * 2).unwrap();
        assert_eq!(cache.prune().unwrap(), 1);
        assert_eq!(cache.stats().evicted, 1);
        assert_eq!(entry_files(&root), entries[1..].to_vec());
        assert!(foreign.exists());
        assert!(!stale_temp.exists());
        assert!(fresh_temp.exists(), "a concurrent writer may still own it");
        assert_eq!(cache.prune().unwrap(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn open_rejects_a_file_as_root() {
        let root = temp_dir("file-root");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("cache");
        fs::write(&file, b"x").unwrap();
        assert!(BlockCache::open(&file, u64::MAX).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lz4_revision_matches_the_locked_dependency() {
        let lock = include_str!("../../../Cargo.lock");
        let mut lines = lock.lines().map(str::trim);
        let version = lines
            .by_ref()
            .find(|line| *line == "name = \"lz4_flex\"")
            .and_then(|_| lines.next());
        assert_eq!(
            version,
            Some(format!("version = \"{LZ4_FLEX_VERSION}\"").as_str()),
            "update LZ4_FLEX_VERSION after changing lz4_flex"
        );
    }
}
