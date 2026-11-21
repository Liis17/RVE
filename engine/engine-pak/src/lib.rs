//! Reader, writer, and catalog for the RVEPAK archive format.

mod archive_name;
mod block_cache;
mod builder;
mod catalog;
mod error;
mod format;
mod reader;
mod registry;
mod writer;

pub use block_cache::{BlockCache, BlockCacheStats, DEFAULT_BLOCK_CACHE_MAX_BYTES};
pub use builder::{
    ArchiveBuildSpec, BuildArchive, BuildCompression, BuildDependency, BuildError, BuildErrorKind,
    BuildFileInput, BuildInput, BuildManifest, BuildOptions, BuildOwner, BuildPlan, BuildReport,
    BuildStats, IgnoredCompressionOverride, ProgrammaticArchive, build_archive_set,
    build_archive_set_with_cache, execute, load_manifest, parse_manifest, preflight,
};
pub use catalog::{
    ArchiveCatalog, CatalogAsset, CatalogLimits, DiscoveredArchive, discover_archives,
};
pub use error::{PakError, Result};
pub use format::{
    ArchiveId, BlobId, Codec, ContentHash, DEFAULT_CHUNK_SIZE, DEFAULT_ZSTD_LEVEL, FileReference,
    HEADER_SIZE, MAGIC, VERSION,
};
pub use reader::{
    Archive, ArchiveLimits, AssetStat, MemoryFootprint, NoopReadControl, ReadControl,
    ReadReservation, VerifyReport,
};
pub use registry::{ArchiveSetRegistry, ArchiveSetRegistryError};
pub use writer::{ArchiveWriter, BlobOptions, FinishedArchive, WriterOptions};

/// Version of the `engine-pak` crate; [`VERSION`] is the written RVEPAK format version.
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");
