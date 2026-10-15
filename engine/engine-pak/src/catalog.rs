use crate::format::{ArchiveId, BlobId, Codec, ContentHash, FileReference};
use crate::reader::{Archive, AssetStat, ReadControl, VerifyReport};
use crate::{NoopReadControl, PakError, Result};
use engine_files::VirtualPath;
use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct CatalogLimits {
    pub max_total_index_bytes: u64,
    pub max_archives: u32,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            max_total_index_bytes: 256 * 1024 * 1024,
            max_archives: 4096,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredArchive {
    pub path: PathBuf,
}

/// One catalog path and the local archive/blob that ultimately owns its bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogAsset {
    pub path: VirtualPath,
    /// Archive whose file table contains this path.
    pub archive_id: ArchiveId,
    /// Archive containing the local blob, which may differ for an external reference.
    pub owner_archive_id: ArchiveId,
    pub blob_id: BlobId,
    pub size: u64,
    pub content_hash: ContentHash,
    pub codec: Codec,
    pub chunk_count: u32,
}

/// Returns top-level regular files whose extension is `.rvepak`, sorted by filename.
pub fn discover_archives(root: impl AsRef<Path>) -> Result<Vec<DiscoveredArchive>> {
    let root = root.as_ref();
    let entries =
        fs::read_dir(root).map_err(|error| PakError::io_path(root.to_path_buf(), error))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| PakError::io_path(root.to_path_buf(), error))?;
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|error| PakError::io_path(path.clone(), error))?
            .is_file()
        {
            continue;
        }
        let Some(extension) = path.extension() else {
            continue;
        };
        if extension.to_string_lossy().eq_ignore_ascii_case("rvepak") {
            paths.push(DiscoveredArchive { path });
        }
    }
    paths.sort_by(|left, right| {
        left.path
            .file_name()
            .cmp(&right.path.file_name())
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(paths)
}

/// Validated namespace for one or more archives and their local payload owners.
pub struct ArchiveCatalog {
    archives: BTreeMap<ArchiveId, Arc<Archive>>,
    paths: BTreeMap<VirtualPath, (ArchiveId, FileReference)>,
    total_index_bytes: u64,
    limits: CatalogLimits,
}

impl Default for ArchiveCatalog {
    fn default() -> Self {
        Self::new(CatalogLimits::default())
    }
}

impl ArchiveCatalog {
    pub fn new(limits: CatalogLimits) -> Self {
        Self {
            archives: BTreeMap::new(),
            paths: BTreeMap::new(),
            total_index_bytes: 0,
            limits,
        }
    }

    pub fn open_directory(
        root: impl AsRef<Path>,
        archive_limits: crate::ArchiveLimits,
        catalog_limits: CatalogLimits,
    ) -> Result<Self> {
        let discovered = discover_archives(root)?;
        if discovered.len() > catalog_limits.max_archives as usize {
            return Err(PakError::BudgetExceeded {
                requested: discovered.len() as u64,
                limit: u64::from(catalog_limits.max_archives),
            });
        }
        let mut catalog = Self::new(catalog_limits);
        for item in discovered {
            let remaining_index_budget = catalog
                .limits
                .max_total_index_bytes
                .saturating_sub(catalog.total_index_bytes);
            let mut per_archive_limits = archive_limits.clone();
            per_archive_limits.max_index_bytes = per_archive_limits
                .max_index_bytes
                .min(remaining_index_budget);
            let archive = Archive::open_file_with_limits(&item.path, per_archive_limits)?;
            catalog.insert(archive)?;
        }
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn insert(&mut self, archive: Archive) -> Result<ArchiveId> {
        let id = archive.archive_id();
        if self.archives.contains_key(&id) {
            return Err(PakError::DuplicateArchiveId(id));
        }
        if self.archives.len() >= self.limits.max_archives as usize {
            return Err(PakError::BudgetExceeded {
                requested: self.archives.len() as u64 + 1,
                limit: u64::from(self.limits.max_archives),
            });
        }
        let total_index_bytes = self
            .total_index_bytes
            .checked_add(archive.index_len())
            .ok_or_else(|| PakError::corrupt("catalog index byte total overflow"))?;
        if total_index_bytes > self.limits.max_total_index_bytes {
            return Err(PakError::BudgetExceeded {
                requested: total_index_bytes,
                limit: self.limits.max_total_index_bytes,
            });
        }

        for path in archive.list() {
            if self.paths.contains_key(path) {
                return Err(PakError::DuplicatePath(path.clone()));
            }
        }
        for path in archive.list() {
            let reference = archive.file_reference(path)?;
            self.paths.insert(path.clone(), (id, reference));
        }
        self.total_index_bytes = total_index_bytes;
        self.archives.insert(id, Arc::new(archive));
        Ok(id)
    }

    /// Confirms that every external path resolves to a local blob in its declared owner.
    pub fn validate(&self) -> Result<()> {
        for archive in self.archives.values() {
            for path in archive.list() {
                if let FileReference::External {
                    archive_id: owner_id,
                    blob_id,
                } = archive.file_reference(path)?
                {
                    let owner = self
                        .archives
                        .get(&owner_id)
                        .ok_or(PakError::MissingOwner(owner_id))?;
                    if !owner.has_local_blob(blob_id) {
                        return Err(PakError::MissingBlob {
                            archive_id: owner_id,
                            blob_id,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    pub fn archive_ids(&self) -> impl Iterator<Item = ArchiveId> + '_ {
        self.archives.keys().copied()
    }

    pub fn list(&self) -> impl Iterator<Item = &VirtualPath> {
        self.paths.keys()
    }

    /// Returns every path with its declaration archive and resolved local blob owner.
    pub fn entries(&self) -> Result<Vec<CatalogAsset>> {
        self.paths
            .iter()
            .map(|(path, (archive_id, _))| {
                let stat = self.stat(path)?;
                Ok(CatalogAsset {
                    path: path.clone(),
                    archive_id: *archive_id,
                    owner_archive_id: stat.owner_archive_id,
                    blob_id: stat.blob_id,
                    size: stat.size,
                    content_hash: stat.content_hash,
                    codec: stat.codec,
                    chunk_count: stat.chunk_count,
                })
            })
            .collect()
    }

    pub fn stat(&self, path: &VirtualPath) -> Result<AssetStat> {
        let (owner, blob_id) = self.resolve(path)?;
        owner.stat_blob(path.clone(), blob_id)
    }

    pub fn read_all(&self, path: &VirtualPath) -> Result<Vec<u8>> {
        let size = self.stat(path)?.size;
        self.read_range(path, 0..size)
    }

    pub fn read_range(&self, path: &VirtualPath, range: Range<u64>) -> Result<Vec<u8>> {
        self.read_range_with_control(path, range, &NoopReadControl)
    }

    pub fn read_range_with_control(
        &self,
        path: &VirtualPath,
        range: Range<u64>,
        control: &dyn ReadControl,
    ) -> Result<Vec<u8>> {
        let (owner, blob_id) = self.resolve(path)?;
        owner.read_blob_range_with_control(blob_id, range, control)
    }

    pub fn verify(&self) -> Result<VerifyReport> {
        self.verify_with_control(&NoopReadControl)
    }

    pub fn verify_with_control(&self, control: &dyn ReadControl) -> Result<VerifyReport> {
        self.validate()?;
        let mut total = VerifyReport::default();
        for archive in self.archives.values() {
            let report = archive.verify_with_control_report(control)?;
            total.local_blobs_verified = total
                .local_blobs_verified
                .checked_add(report.local_blobs_verified)
                .ok_or_else(|| PakError::corrupt("verified blob count overflow"))?;
            total.decoded_bytes_verified = total
                .decoded_bytes_verified
                .checked_add(report.decoded_bytes_verified)
                .ok_or_else(|| PakError::corrupt("verified byte count overflow"))?;
        }
        Ok(total)
    }

    pub fn total_index_bytes(&self) -> u64 {
        self.total_index_bytes
    }

    fn resolve(&self, path: &VirtualPath) -> Result<(&Archive, BlobId)> {
        let (archive_id, reference) = self
            .paths
            .get(path)
            .copied()
            .ok_or_else(|| PakError::NotFound(path.clone()))?;
        match reference {
            FileReference::Local(blob_id) => {
                let archive = self
                    .archives
                    .get(&archive_id)
                    .ok_or(PakError::MissingOwner(archive_id))?;
                Ok((archive.as_ref(), blob_id))
            }
            FileReference::External {
                archive_id: owner_id,
                blob_id,
            } => {
                let owner = self
                    .archives
                    .get(&owner_id)
                    .ok_or(PakError::MissingOwner(owner_id))?;
                if !owner.has_local_blob(blob_id) {
                    return Err(PakError::MissingBlob {
                        archive_id: owner_id,
                        blob_id,
                    });
                }
                Ok((owner.as_ref(), blob_id))
            }
        }
    }
}
