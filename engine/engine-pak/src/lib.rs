//! Reader, writer, and catalog for the version 1 RVEPAK archive format.

mod builder;
mod catalog;
mod error;
mod format;
mod reader;
mod writer;

pub use builder::{
    ArchiveBuildSpec, BuildArchive, BuildCompression, BuildDependency, BuildError, BuildErrorKind,
    BuildInput, BuildManifest, BuildOptions, BuildOwner, BuildPlan, BuildReport, BuildStats,
    IgnoredCompressionOverride, execute, load_manifest, parse_manifest, preflight,
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
pub use writer::{ArchiveWriter, BlobOptions, FinishedArchive, WriterOptions};
