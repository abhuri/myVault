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
    TrashAccessDenied(PathBuf),
    InvalidTrashPath(PathBuf),
    InvalidTrashManifest(&'static str),
    NonCanonicalTrashManifest,
    TrashManifestCollision(PathBuf),
    TrashManifestDigestMismatch,
    InvalidTrashTopology(&'static str),
    TrashItemIdentityChanged,
    TrashManifestOutcomeUnknown {
        path: PathBuf,
        cause: Box<CoreError>,
    },
    TrashPayloadOutcomeUnknown {
        source_path: PathBuf,
        destination_path: PathBuf,
        durability: crate::MoveDurability,
        cause: Box<CoreError>,
    },
    InvalidRevision,
    RevisionTargetNotFile(PathBuf),
    StaleRevision {
        path: PathBuf,
        expected: crate::FileRevision,
        actual: crate::FileRevision,
    },
    MoveDurabilitySyncFailed,
    StagePayloadPrepublicationSyncFailed {
        source_path: PathBuf,
        destination_path: PathBuf,
        destination_sync: crate::DirectorySyncStatus,
        source_sync: crate::DirectorySyncStatus,
        cause: Box<CoreError>,
    },
    MoveContentPrepublicationSyncFailed {
        source_path: PathBuf,
        destination_path: PathBuf,
        destination_sync: crate::DirectorySyncStatus,
        source_sync: crate::DirectorySyncStatus,
        cause: Box<CoreError>,
    },
    CaseRenamePrepublicationSyncFailed {
        source_path: PathBuf,
        destination_path: PathBuf,
        temporary_path: PathBuf,
        directory_sync: crate::DirectorySyncStatus,
        cause: Box<CoreError>,
    },
    CaseRenameOutcomeUnknown {
        source_path: PathBuf,
        destination_path: PathBuf,
        temporary_path: PathBuf,
        phase: crate::CaseRenamePhase,
        directory_sync: crate::DirectorySyncStatus,
        verification: Box<CoreError>,
    },
    ReplaceContentOutcomeUnknown {
        path: PathBuf,
        directory_sync: crate::DirectorySyncStatus,
        verification: Box<CoreError>,
    },
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
    InvalidMove {
        source_path: PathBuf,
        destination_path: PathBuf,
        reason: &'static str,
    },
    AtomicMoveOutcomeUnknown {
        source_path: PathBuf,
        destination_path: PathBuf,
        destination_sync: crate::DirectorySyncStatus,
        source_sync: crate::DirectorySyncStatus,
    },
    VerifiedMoveOutcomeUnknown {
        source_path: PathBuf,
        destination_path: PathBuf,
        destination_sync: crate::DirectorySyncStatus,
        source_sync: crate::DirectorySyncStatus,
        verification: Box<CoreError>,
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
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(result) = self.fmt_special(formatter) {
            return result;
        }
        match self {
            Self::AutomaticObsidianWriteDenied(path) => write!(
                formatter,
                "automatic writes under .obsidian are denied: {}",
                path.display()
            ),
            Self::InvalidRelativePath(_)
            | Self::PathEscapesVault(_)
            | Self::SymlinkRejected(_)
            | Self::TrashWriteDenied(_)
            | Self::TrashAccessDenied(_)
            | Self::InvalidTrashPath(_)
            | Self::InvalidTrashManifest(_)
            | Self::NonCanonicalTrashManifest
            | Self::TrashManifestCollision(_)
            | Self::TrashManifestDigestMismatch
            | Self::InvalidTrashTopology(_)
            | Self::TrashItemIdentityChanged
            | Self::TrashManifestOutcomeUnknown { .. }
            | Self::TrashPayloadOutcomeUnknown { .. }
            | Self::InvalidRevision
            | Self::RevisionTargetNotFile(_)
            | Self::StaleRevision { .. }
            | Self::MoveDurabilitySyncFailed
            | Self::StagePayloadPrepublicationSyncFailed { .. }
            | Self::MoveContentPrepublicationSyncFailed { .. }
            | Self::CaseRenamePrepublicationSyncFailed { .. }
            | Self::CaseRenameOutcomeUnknown { .. }
            | Self::ReplaceContentOutcomeUnknown { .. }
            | Self::VerifiedMoveOutcomeUnknown { .. } => {
                unreachable!("handled before main error formatting")
            }
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
            Self::InvalidMove {
                source_path,
                destination_path,
                reason,
            } => write!(
                formatter,
                "invalid move from {} to {}: {reason}",
                source_path.display(),
                destination_path.display()
            ),
            Self::AtomicMoveOutcomeUnknown {
                source_path,
                destination_path,
                destination_sync,
                source_sync,
            } => write!(
                formatter,
                "move from {} to {} was published but directory durability is unknown (destination: {destination_sync}; source: {source_sync})",
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

impl CoreError {
    #[allow(clippy::too_many_lines)]
    fn fmt_special(&self, formatter: &mut fmt::Formatter<'_>) -> Option<fmt::Result> {
        match self {
            Self::InvalidRelativePath(path) => Some(write!(
                formatter,
                "invalid vault-relative path: {}",
                path.display()
            )),
            Self::PathEscapesVault(path) => Some(write!(
                formatter,
                "path escapes vault root: {}",
                path.display()
            )),
            Self::SymlinkRejected(path) => Some(write!(
                formatter,
                "symlink components are not allowed: {}",
                path.display()
            )),
            Self::TrashAccessDenied(path) => Some(write!(
                formatter,
                "generic vault access under .trash is denied: {}",
                path.display()
            )),
            Self::TrashWriteDenied(path) => Some(write!(
                formatter,
                "generic vault writes under .trash are denied: {}",
                path.display()
            )),
            Self::InvalidTrashPath(path) => Some(write!(
                formatter,
                "invalid privileged trash path: {}",
                path.display()
            )),
            Self::InvalidTrashManifest(reason) => {
                Some(write!(formatter, "invalid trash manifest: {reason}"))
            }
            Self::NonCanonicalTrashManifest => {
                Some(formatter.write_str("trash manifest JSON is not byte-for-byte canonical"))
            }
            Self::TrashManifestCollision(path) => Some(write!(
                formatter,
                "trash manifest differs from the existing entry: {}",
                path.display()
            )),
            Self::TrashManifestDigestMismatch => {
                Some(formatter.write_str("trash manifest digest does not match"))
            }
            Self::InvalidTrashTopology(reason) => {
                Some(write!(formatter, "invalid trash topology: {reason}"))
            }
            Self::TrashItemIdentityChanged => {
                Some(formatter.write_str("trash item identity changed during inspection"))
            }
            Self::TrashManifestOutcomeUnknown { path, cause } => Some(write!(
                formatter,
                "trash manifest may be published at {}: {cause}",
                path.display()
            )),
            Self::TrashPayloadOutcomeUnknown {
                source_path,
                destination_path,
                durability,
                cause,
            } => Some(write!(
                formatter,
                "trash payload move from {} to {} is published with {durability:?}, but manifest revalidation failed: {cause}",
                source_path.display(),
                destination_path.display()
            )),
            Self::InvalidRevision => Some(formatter.write_str("invalid BLAKE3 file revision")),
            Self::RevisionTargetNotFile(path) => Some(write!(
                formatter,
                "revision target is not a regular file: {}",
                path.display()
            )),
            Self::StaleRevision {
                path,
                expected,
                actual,
            } => Some(write!(
                formatter,
                "stale revision for {}: expected {} bytes at {}, found {} bytes at {}",
                path.display(),
                expected.byte_len,
                expected.hex,
                actual.byte_len,
                actual.hex
            )),
            Self::MoveDurabilitySyncFailed => Some(formatter.write_str(
                "one or more directory sync attempts failed",
            )),
            Self::StagePayloadPrepublicationSyncFailed {
                source_path,
                destination_path,
                destination_sync,
                source_sync,
                cause,
            } => Some(write!(
                formatter,
                "trash payload was not published from {} to {} because prepublication sync failed (destination sync: {destination_sync}; source sync: {source_sync}): {cause}",
                source_path.display(),
                destination_path.display()
            )),
            Self::MoveContentPrepublicationSyncFailed {
                source_path,
                destination_path,
                destination_sync,
                source_sync,
                cause,
            } => Some(write!(
                formatter,
                "content was not moved from {} to {} because prepublication sync failed (destination sync: {destination_sync}; source sync: {source_sync}): {cause}",
                source_path.display(),
                destination_path.display()
            )),
            Self::CaseRenamePrepublicationSyncFailed {
                source_path,
                destination_path,
                temporary_path,
                directory_sync,
                cause,
            } => Some(write!(
                formatter,
                "case rename from {} to {} through {} was not published because the parent prepublication sync failed ({directory_sync}): {cause}",
                source_path.display(),
                destination_path.display(),
                temporary_path.display()
            )),
            Self::VerifiedMoveOutcomeUnknown {
                source_path,
                destination_path,
                destination_sync,
                source_sync,
                verification,
            } => Some(write!(
                formatter,
                "verified move from {} to {} may be published (destination sync: {destination_sync}; source sync: {source_sync}); topology verification failed: {verification}",
                source_path.display(),
                destination_path.display()
            )),
            Self::CaseRenameOutcomeUnknown {
                source_path,
                destination_path,
                temporary_path,
                phase,
                directory_sync,
                verification,
            } => Some(write!(
                formatter,
                "case rename from {} to {} through {} has an unknown outcome in phase {phase:?} (directory sync: {directory_sync}); verification failed: {verification}",
                source_path.display(),
                destination_path.display(),
                temporary_path.display()
            )),
            Self::ReplaceContentOutcomeUnknown {
                path,
                directory_sync,
                verification,
            } => Some(write!(
                formatter,
                "replacement of {} may be published (directory sync: {directory_sync}); verification failed: {verification}",
                path.display()
            )),
            _ => None,
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::AtomicNoReplaceUnsupported { source: error, .. } => Some(error),
            Self::CommitOutcomeUnknown { source, .. }
            | Self::PublishedCleanupPending { source, .. } => Some(source),
            Self::AtomicMoveOutcomeUnknown {
                destination_sync,
                source_sync,
                ..
            } => destination_sync.error().or_else(|| source_sync.error()),
            Self::VerifiedMoveOutcomeUnknown { verification, .. }
            | Self::CaseRenameOutcomeUnknown { verification, .. } => Some(verification.as_ref()),
            Self::ReplaceContentOutcomeUnknown {
                directory_sync,
                verification,
                ..
            } => directory_sync
                .error()
                .or_else(|| Some(verification.as_ref())),
            Self::StagePayloadPrepublicationSyncFailed { cause, .. }
            | Self::MoveContentPrepublicationSyncFailed { cause, .. }
            | Self::CaseRenamePrepublicationSyncFailed { cause, .. }
            | Self::TrashManifestOutcomeUnknown { cause, .. }
            | Self::TrashPayloadOutcomeUnknown { cause, .. } => Some(cause.as_ref()),
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
