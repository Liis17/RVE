use crate::{ArchiveId, BlobId, ContentHash};
use engine_files::VirtualPath;
use std::fmt;
use std::io;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, PakError>;

#[derive(Debug)]
pub enum PakError {
    Path(engine_files::VirtualPathError),
    NotFound(VirtualPath),
    Io {
        path: Option<PathBuf>,
        source: io::Error,
    },
    Corrupt(String),
    InvalidRange {
        start: u64,
        end: u64,
        size: u64,
    },
    Unsupported(String),
    BudgetExceeded {
        requested: u64,
        limit: u64,
    },
    Cancelled,
    DuplicatePath(VirtualPath),
    DuplicateArchiveId(ArchiveId),
    MissingOwner(ArchiveId),
    MissingBlob {
        archive_id: ArchiveId,
        blob_id: BlobId,
    },
    UnknownLocalBlob(BlobId),
    SourceMismatch {
        expected: ContentHash,
        actual: ContentHash,
    },
    InvalidConfiguration(String),
    BackingChanged,
    PoisonedWriter,
}

impl PakError {
    pub(crate) fn io(source: io::Error) -> Self {
        Self::Io { path: None, source }
    }

    pub(crate) fn io_path(path: PathBuf, source: io::Error) -> Self {
        Self::Io {
            path: Some(path),
            source,
        }
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }
}

impl fmt::Display for PakError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(f),
            Self::NotFound(path) => write!(f, "resource {path} was not found"),
            Self::Io {
                path: Some(path),
                source,
            } => {
                write!(f, "I/O error for {}: {source}", path.display())
            }
            Self::Io { path: None, source } => write!(f, "I/O error: {source}"),
            Self::Corrupt(message) => write!(f, "corrupt archive: {message}"),
            Self::InvalidRange { start, end, size } => {
                write!(f, "range {start}..{end} is outside resource size {size}")
            }
            Self::Unsupported(message) => write!(f, "unsupported archive data: {message}"),
            Self::BudgetExceeded { requested, limit } => write!(
                f,
                "resource budget exceeded: requested {requested}, limit {limit}"
            ),
            Self::Cancelled => f.write_str("resource read was cancelled"),
            Self::DuplicatePath(path) => write!(f, "duplicate resource path {path}"),
            Self::DuplicateArchiveId(id) => write!(f, "duplicate archive id {id}"),
            Self::MissingOwner(id) => write!(f, "external resource owner {id} is missing"),
            Self::MissingBlob {
                archive_id,
                blob_id,
            } => {
                write!(f, "archive {archive_id} does not own blob {blob_id}")
            }
            Self::UnknownLocalBlob(blob_id) => {
                write!(f, "local blob {blob_id} has not been added to this archive")
            }
            Self::SourceMismatch { expected, actual } => write!(
                f,
                "streamed source hash changed: expected {expected}, got {actual}"
            ),
            Self::InvalidConfiguration(message) => f.write_str(message),
            Self::BackingChanged => {
                f.write_str("archive backing changed since its metadata was opened")
            }
            Self::PoisonedWriter => {
                f.write_str("archive writer cannot continue after a failed asset write")
            }
        }
    }
}

impl std::error::Error for PakError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<engine_files::VirtualPathError> for PakError {
    fn from(value: engine_files::VirtualPathError) -> Self {
        Self::Path(value)
    }
}

impl From<io::Error> for PakError {
    fn from(value: io::Error) -> Self {
        Self::io(value)
    }
}
