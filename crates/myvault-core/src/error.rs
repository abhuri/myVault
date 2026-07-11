use std::fmt;
use std::path::PathBuf;

/// Errors emitted by the core safety boundary.
#[derive(Debug)]
pub enum CoreError {
    InvalidRelativePath(PathBuf),
    PathEscapesVault(PathBuf),
    SymlinkRejected(PathBuf),
    AutomaticObsidianWriteDenied(PathBuf),
    TrashWriteDenied(PathBuf),
    AppDataInsideVault {
        app_data: PathBuf,
        vault: PathBuf,
    },
    UnsafeDatabasePath(PathBuf),
    InvalidRecord(&'static str),
    ResourceLimitExceeded {
        resource: &'static str,
        limit: usize,
    },
    PortablePathCollision {
        existing: String,
        incoming: String,
    },
    AlreadyExists(PathBuf),
    AtomicNoReplaceUnsupported {
        source_path: PathBuf,
        destination_path: PathBuf,
        source: std::io::Error,
    },
    CommitOutcomeUnknown {
        path: PathBuf,
        source: std::io::Error,
    },
    PublishedCleanupPending {
        path: PathBuf,
        temp_name: String,
        source: std::io::Error,
    },
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRelativePath(path) => {
                write!(formatter, "invalid vault-relative path: {}", path.display())
            }
            Self::PathEscapesVault(path) => {
                write!(formatter, "path escapes vault root: {}", path.display())
            }
            Self::SymlinkRejected(path) => {
                write!(
                    formatter,
                    "symlink components are not allowed: {}",
                    path.display()
                )
            }
            Self::AutomaticObsidianWriteDenied(path) => write!(
                formatter,
                "automatic writes under .obsidian are denied: {}",
                path.display()
            ),
            Self::TrashWriteDenied(path) => write!(
                formatter,
                "generic vault writes under .trash are denied: {}",
                path.display()
            ),
            Self::AppDataInsideVault { app_data, vault } => write!(
                formatter,
                "app-data directory {} must be outside synced vault {}",
                app_data.display(),
                vault.display()
            ),
            Self::UnsafeDatabasePath(path) => write!(
                formatter,
                "derived-index path is not a private regular file: {}",
                path.display()
            ),
            Self::InvalidRecord(reason) => write!(formatter, "invalid index record: {reason}"),
            Self::ResourceLimitExceeded { resource, limit } => {
                write!(formatter, "{resource} exceeds configured limit of {limit}")
            }
            Self::PortablePathCollision { existing, incoming } => write!(
                formatter,
                "portable vault paths collide across filesystems: {incoming} conflicts with {existing}"
            ),
            Self::AlreadyExists(path) => {
                write!(formatter, "destination already exists: {}", path.display())
            }
            Self::AtomicNoReplaceUnsupported {
                source_path,
                destination_path,
                source,
            } => write!(
                formatter,
                "atomic no-replace move from {} to {} is unsupported: {source}",
                source_path.display(),
                destination_path.display()
            ),
            Self::CommitOutcomeUnknown { path, source } => write!(
                formatter,
                "publication outcome for {} is unknown: {source}",
                path.display()
            ),
            Self::PublishedCleanupPending {
                path,
                temp_name,
                source,
            } => write!(
                formatter,
                "{} was published but cleanup of {temp_name} may be pending: {source}",
                path.display()
            ),
            Self::Io(error) => write!(formatter, "filesystem error: {error}"),
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::AtomicNoReplaceUnsupported { source: error, .. } => Some(error),
            Self::CommitOutcomeUnknown { source, .. }
            | Self::PublishedCleanupPending { source, .. } => Some(source),
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<rusqlite::Error> for CoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;
