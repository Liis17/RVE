use engine_files::VirtualPathError;
use engine_pak::PakError;
use std::fmt;
use std::io;
use std::path::PathBuf;

/// Broad, stable classification for resource failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceErrorKind {
    InvalidPath,
    NotFound,
    Io,
    Corrupt,
    InvalidRange,
    Unsupported,
    BudgetExceeded,
    Cancelled,
    QueueFull,
    Stopped,
    WorkerPanicked,
    RequestCompleted,
    PathOutsideRoot,
    NonRegularFile,
    DuplicatePath,
    DuplicateArchiveId,
    MissingOwner,
    MissingBlob,
    InvalidConfiguration,
    BackingChanged,
}

/// Error returned by startup, request submission, and resource requests.
#[derive(Debug)]
pub enum ResourceError {
    InvalidPath(VirtualPathError),
    NotFound(String),
    Io {
        path: Option<PathBuf>,
        source: io::Error,
    },
    Archive(PakError),
    InvalidRange {
        start: u64,
        end: u64,
        size: u64,
    },
    BudgetExceeded {
        requested: u64,
        limit: u64,
    },
    InvalidConfiguration(String),
    PathOutsideRoot(String),
    NonRegularFile(String),
    QueueFull,
    Cancelled,
    Stopped,
    WorkerPanicked,
    RequestCompleted,
}

impl ResourceError {
    /// Returns the error class without discarding the detailed source error.
    pub fn kind(&self) -> ResourceErrorKind {
        match self {
            Self::InvalidPath(_) => ResourceErrorKind::InvalidPath,
            Self::NotFound(_) => ResourceErrorKind::NotFound,
            Self::Io { .. } => ResourceErrorKind::Io,
            Self::Archive(error) => match error {
                PakError::Path(_) => ResourceErrorKind::InvalidPath,
                PakError::NotFound(_) => ResourceErrorKind::NotFound,
                PakError::Io { .. } => ResourceErrorKind::Io,
                PakError::Corrupt(_) => ResourceErrorKind::Corrupt,
                PakError::InvalidRange { .. } => ResourceErrorKind::InvalidRange,
                PakError::Unsupported(_) => ResourceErrorKind::Unsupported,
                PakError::BudgetExceeded { .. } => ResourceErrorKind::BudgetExceeded,
                PakError::Cancelled => ResourceErrorKind::Cancelled,
                PakError::DuplicatePath(_) => ResourceErrorKind::DuplicatePath,
                PakError::DuplicateArchiveId(_) => ResourceErrorKind::DuplicateArchiveId,
                PakError::MissingOwner(_) => ResourceErrorKind::MissingOwner,
                PakError::MissingBlob { .. } => ResourceErrorKind::MissingBlob,
                PakError::UnknownLocalBlob(_) => ResourceErrorKind::Corrupt,
                PakError::SourceMismatch { .. } => ResourceErrorKind::Corrupt,
                PakError::InvalidConfiguration(_) => ResourceErrorKind::InvalidConfiguration,
                PakError::BackingChanged => ResourceErrorKind::BackingChanged,
                PakError::PoisonedWriter => ResourceErrorKind::Corrupt,
            },
            Self::InvalidRange { .. } => ResourceErrorKind::InvalidRange,
            Self::BudgetExceeded { .. } => ResourceErrorKind::BudgetExceeded,
            Self::InvalidConfiguration(_) => ResourceErrorKind::InvalidConfiguration,
            Self::PathOutsideRoot(_) => ResourceErrorKind::PathOutsideRoot,
            Self::NonRegularFile(_) => ResourceErrorKind::NonRegularFile,
            Self::QueueFull => ResourceErrorKind::QueueFull,
            Self::Cancelled => ResourceErrorKind::Cancelled,
            Self::Stopped => ResourceErrorKind::Stopped,
            Self::WorkerPanicked => ResourceErrorKind::WorkerPanicked,
            Self::RequestCompleted => ResourceErrorKind::RequestCompleted,
        }
    }
}

impl From<PakError> for ResourceError {
    fn from(error: PakError) -> Self {
        match error {
            PakError::Path(error) => Self::InvalidPath(error),
            PakError::NotFound(path) => Self::NotFound(path.to_string()),
            PakError::Io { path, source } => Self::Io { path, source },
            PakError::InvalidRange { start, end, size } => Self::InvalidRange { start, end, size },
            PakError::BudgetExceeded { requested, limit } => {
                Self::BudgetExceeded { requested, limit }
            }
            PakError::InvalidConfiguration(message) => Self::InvalidConfiguration(message),
            PakError::Cancelled => Self::Cancelled,
            other => Self::Archive(other),
        }
    }
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(error) => error.fmt(f),
            Self::NotFound(path) => write!(f, "resource {path} was not found"),
            Self::Io {
                path: Some(path),
                source,
            } => write!(f, "I/O error for {}: {source}", path.display()),
            Self::Io { path: None, source } => write!(f, "I/O error: {source}"),
            Self::Archive(error) => error.fmt(f),
            Self::InvalidRange { start, end, size } => {
                write!(f, "range {start}..{end} is outside resource size {size}")
            }
            Self::BudgetExceeded { requested, limit } => {
                write!(
                    f,
                    "resource budget exceeded: requested {requested}, limit {limit}"
                )
            }
            Self::InvalidConfiguration(message) => f.write_str(message),
            Self::PathOutsideRoot(path) => write!(f, "resource path {path} escapes its root"),
            Self::NonRegularFile(path) => write!(f, "resource path {path} is not a regular file"),
            Self::QueueFull => f.write_str("resource request queue is full"),
            Self::Cancelled => f.write_str("resource request was cancelled"),
            Self::Stopped => f.write_str("resource system is stopped"),
            Self::WorkerPanicked => f.write_str("resource worker panicked"),
            Self::RequestCompleted => f.write_str("resource request was already completed"),
        }
    }
}

impl std::error::Error for ResourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPath(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Archive(error) => Some(error),
            _ => None,
        }
    }
}
