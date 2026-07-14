//! Tauri-free, frontend-safe read service for one active local vault.

use myvault_core::{CoreError, TrashId, TrashListEvidence, Vault, VaultPath, MAX_TRASH_PAGE_SIZE};
use myvault_snapshots::{SnapshotManifest, SnapshotRevision, SnapshotStore};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const EXPLORER_MAX_DEPTH: usize = 64;
pub const EXPLORER_MAX_SCAN: usize = 5_000;
pub const EXPLORER_DEFAULT_PAGE_SIZE: usize = 100;
pub const EXPLORER_MAX_PAGE_SIZE: usize = 200;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct VaultSessionId(Uuid);

impl VaultSessionId {
    /// Creates a fresh opaque session identifier for another trusted native
    /// vault capability implementation.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parses one canonical lowercase, hyphenated, nonnil session UUID.
    ///
    /// # Errors
    /// Rejects UUID aliases, uppercase representations, and the nil UUID.
    pub fn parse(value: &str) -> Result<Self, AppError> {
        let id = Uuid::parse_str(value).map_err(|_| invalid_session_id_error())?;
        if id.is_nil() || id.to_string() != value {
            return Err(invalid_session_id_error());
        }
        Ok(Self(id))
    }
}

impl Default for VaultSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for VaultSessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for VaultSessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultStatusDto {
    pub active: bool,
    pub session_id: Option<VaultSessionId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteDto {
    pub session_id: VaultSessionId,
    pub path: String,
    pub text: String,
    pub revision_hex: String,
    pub byte_len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TrashEvidenceDto {
    Supported {
        original_path: String,
        trashed_at_unix_ms: i64,
        revision_hex: String,
        byte_len: u64,
        manifest_digest: String,
    },
    Opaque,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashItemDto {
    pub trash_id: String,
    pub evidence: TrashEvidenceDto,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashPageDto {
    pub session_id: VaultSessionId,
    pub entries: Vec<TrashItemDto>,
    pub invalid_name_count: usize,
    pub next_after: Option<String>,
    pub has_more: bool,
    pub scanned_entries: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExplorerKindDto {
    Markdown,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerEntryDto {
    pub path: String,
    pub kind: ExplorerKindDto,
    pub byte_len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerPageDto {
    pub session_id: VaultSessionId,
    pub entries: Vec<ExplorerEntryDto>,
    pub next_after: Option<String>,
    pub has_more: bool,
    pub scanned_entries: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SaveDurabilityDto {
    FullySynced,
    DirectorySyncUnsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveNoteDto {
    pub session_id: VaultSessionId,
    pub path: String,
    pub revision_hex: String,
    pub byte_len: u64,
    pub durability: SaveDurabilityDto,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AppErrorCode {
    NoActiveSession,
    StaleSession,
    InvalidSessionId,
    InvalidPath,
    InvalidCursor,
    InvalidLimit,
    InvalidRevision,
    StaleRevision,
    WriteOutcomeUnknown,
    RecoveryUnavailable,
    NoteNotFound,
    NoteNotUtf8,
    VaultUnavailable,
    ResourceLimit,
    VaultSelectionFailed,
    UnsupportedPlatform,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: AppErrorCode,
    pub message: &'static str,
}

impl AppError {
    const fn new(code: AppErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    #[must_use]
    pub const fn internal() -> Self {
        Self::new(
            AppErrorCode::Internal,
            "the application service is unavailable",
        )
    }

    #[must_use]
    pub const fn vault_selection_failed() -> Self {
        Self::new(
            AppErrorCode::VaultSelectionFailed,
            "the selected vault could not be activated",
        )
    }

    #[must_use]
    pub const fn unsupported_platform() -> Self {
        Self::new(
            AppErrorCode::UnsupportedPlatform,
            "this operation is unsupported on the current platform",
        )
    }

    #[must_use]
    pub const fn write_outcome_unknown() -> Self {
        Self::new(
            AppErrorCode::WriteOutcomeUnknown,
            "the note write outcome is unknown",
        )
    }

    /// Recovery snapshots are a fail-closed prerequisite for configured
    /// desktop saves. The safe error deliberately omits private filesystem
    /// details while still distinguishing this policy stop from an unknown
    /// Vault write outcome.
    #[must_use]
    pub const fn recovery_unavailable() -> Self {
        Self::new(
            AppErrorCode::RecoveryUnavailable,
            "the recovery snapshot could not be secured, so the note was not saved",
        )
    }

    #[must_use]
    pub const fn no_active_session() -> Self {
        Self::new(AppErrorCode::NoActiveSession, "no vault session is active")
    }

    #[must_use]
    pub const fn stale_session() -> Self {
        Self::new(
            AppErrorCode::StaleSession,
            "the vault session is no longer active",
        )
    }

    #[must_use]
    pub const fn vault_unavailable() -> Self {
        Self::new(
            AppErrorCode::VaultUnavailable,
            "vault evidence is unavailable",
        )
    }

    #[must_use]
    pub const fn invalid_cursor_or_limit() -> Self {
        Self::new(AppErrorCode::InvalidLimit, "the requested page is invalid")
    }

    #[must_use]
    pub const fn stale_revision() -> Self {
        Self::new(
            AppErrorCode::StaleRevision,
            "the note changed after it was opened",
        )
    }

    #[must_use]
    pub const fn note_not_found() -> Self {
        Self::new(AppErrorCode::NoteNotFound, "the note was not found")
    }

    #[must_use]
    pub const fn invalid_revision_or_path() -> Self {
        Self::new(
            AppErrorCode::InvalidRevision,
            "the note revision or path is invalid",
        )
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for AppError {}

struct VaultSession {
    id: VaultSessionId,
    vault_id: Uuid,
    vault: Vault,
    snapshots: Option<SnapshotRuntime>,
}

/// Native-only capability context for opening private per-Vault services.
///
/// This type deliberately does not implement `Serialize` or `Debug` because it
/// contains ambient filesystem paths that must stay behind the Tauri boundary.
pub struct NativeVaultContext {
    session_id: VaultSessionId,
    vault_id: Uuid,
    vault_root: PathBuf,
    app_data_root: Option<PathBuf>,
}

impl NativeVaultContext {
    #[must_use]
    pub const fn session_id(&self) -> VaultSessionId {
        self.session_id
    }

    #[must_use]
    pub const fn vault_id(&self) -> Uuid {
        self.vault_id
    }

    #[must_use]
    pub fn vault_root(&self) -> &Path {
        &self.vault_root
    }

    #[must_use]
    pub fn app_data_root(&self) -> Option<&Path> {
        self.app_data_root.as_deref()
    }
}

struct SnapshotRuntime {
    vault_id: Uuid,
    store: SnapshotStore,
}

#[derive(Default)]
pub struct AppService {
    session: RwLock<Option<Arc<VaultSession>>>,
    app_data_root: Option<PathBuf>,
}

impl AppService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables private recovery snapshots for activated desktop vaults.
    #[must_use]
    pub fn with_app_data_root(app_data_root: impl Into<PathBuf>) -> Self {
        Self {
            session: RwLock::new(None),
            app_data_root: Some(app_data_root.into()),
        }
    }

    /// Activates an already-open capability supplied by a trusted native picker adapter.
    ///
    /// # Errors
    /// Returns a safe internal error if the session lock is unavailable.
    pub fn activate_trusted_vault(&self, vault: Vault) -> Result<VaultStatusDto, AppError> {
        let id = VaultSessionId(Uuid::new_v4());
        let vault_id = vault_id_for(&vault);
        let snapshots = self
            .app_data_root
            .as_ref()
            .map(|root| open_snapshot_runtime(root, &vault, vault_id))
            .transpose()?;
        let session = Arc::new(VaultSession {
            id,
            vault_id,
            vault,
            snapshots,
        });
        *self.session.write().map_err(|_| internal_error())? = Some(session);
        Ok(VaultStatusDto {
            active: true,
            session_id: Some(id),
        })
    }

    /// Closes exactly the requested active session.
    ///
    /// # Errors
    /// Rejects absent or stale session identifiers.
    pub fn close(&self, session_id: VaultSessionId) -> Result<VaultStatusDto, AppError> {
        let mut current = self.session.write().map_err(|_| internal_error())?;
        let active = current.as_ref().ok_or_else(no_session_error)?;
        if active.id != session_id {
            return Err(stale_session_error());
        }
        *current = None;
        Ok(VaultStatusDto {
            active: false,
            session_id: None,
        })
    }

    /// Returns only opaque session state and never the ambient root path.
    ///
    /// # Errors
    /// Returns a safe internal error if the session lock is unavailable.
    pub fn status(&self) -> Result<VaultStatusDto, AppError> {
        let current = self.session.read().map_err(|_| internal_error())?;
        Ok(VaultStatusDto {
            active: current.is_some(),
            session_id: current.as_ref().map(|session| session.id),
        })
    }

    /// Returns an owned native-only capability snapshot for the exact active session.
    ///
    /// The context is intentionally not frontend-serializable. Native callers
    /// must still validate the session again before publishing a result after a
    /// long-running operation.
    ///
    /// # Errors
    /// Rejects absent or stale session identifiers and unavailable session state.
    pub fn native_vault_context(
        &self,
        session_id: VaultSessionId,
    ) -> Result<NativeVaultContext, AppError> {
        self.with_session(session_id, |session| {
            Ok(NativeVaultContext {
                session_id,
                vault_id: session.vault_id,
                vault_root: session.vault.root().to_path_buf(),
                app_data_root: self.app_data_root.clone(),
            })
        })
    }

    /// Confirms that an opaque session remains the active native capability.
    ///
    /// # Errors
    /// Rejects absent or stale session identifiers.
    pub fn confirm_active_session(&self, session_id: VaultSessionId) -> Result<(), AppError> {
        self.with_session(session_id, |_| Ok(()))
    }

    /// Reads one Markdown note as strict UTF-8 with its exact byte revision.
    ///
    /// # Errors
    /// Rejects invalid paths, non-UTF-8 content, unavailable files, and stale sessions.
    pub fn read_note(
        &self,
        session_id: VaultSessionId,
        portable_path: &str,
    ) -> Result<NoteDto, AppError> {
        let path = VaultPath::from_portable(portable_path).map_err(|_| invalid_path_error())?;
        let canonical = path.as_str().to_owned();
        self.with_session(session_id, |session| {
            let note = session
                .vault
                .read_note_with_revision(&path)
                .map_err(map_note_error)?;
            let text = String::from_utf8(note.bytes).map_err(|_| {
                AppError::new(AppErrorCode::NoteNotUtf8, "the note is not valid UTF-8")
            })?;
            Ok(NoteDto {
                session_id,
                path: canonical,
                text,
                revision_hex: note.revision.hex,
                byte_len: note.revision.byte_len,
            })
        })
    }

    /// Lists one deterministic bounded page of Trash evidence.
    ///
    /// # Errors
    /// Rejects invalid cursors/limits, unavailable evidence, and stale sessions.
    pub fn list_trash(
        &self,
        session_id: VaultSessionId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<TrashPageDto, AppError> {
        if !(1..=MAX_TRASH_PAGE_SIZE).contains(&limit) {
            return Err(AppError::new(
                AppErrorCode::InvalidLimit,
                "the requested page size is invalid",
            ));
        }
        let after = after
            .map(|value| {
                TrashId::parse(value).map_err(|_| {
                    AppError::new(AppErrorCode::InvalidCursor, "the Trash cursor is invalid")
                })
            })
            .transpose()?;
        self.with_session(session_id, |session| {
            let page = session
                .vault
                .trash_store()
                .list_items_page(after, limit)
                .map_err(map_core_error)?;
            Ok(TrashPageDto {
                session_id,
                entries: page.entries.into_iter().map(map_trash_entry).collect(),
                invalid_name_count: page.invalid_name_count,
                next_after: page.next_after.map(|id| id.to_string()),
                has_more: page.has_more,
                scanned_entries: page.scanned_entries,
            })
        })
    }

    /// Lists a deterministic, bounded page of portable explorer entries.
    ///
    /// # Errors
    /// Rejects noncanonical cursors/page sizes, unavailable evidence, and stale sessions.
    pub fn list_explorer(
        &self,
        session_id: VaultSessionId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<ExplorerPageDto, AppError> {
        if !(1..=EXPLORER_MAX_PAGE_SIZE).contains(&limit) {
            return Err(AppError::new(
                AppErrorCode::InvalidLimit,
                "the requested page size is invalid",
            ));
        }
        let after = after
            .map(|value| {
                let path = VaultPath::from_portable(value).map_err(|_| {
                    AppError::new(
                        AppErrorCode::InvalidCursor,
                        "the explorer cursor is invalid",
                    )
                })?;
                if path.as_str() != value {
                    return Err(AppError::new(
                        AppErrorCode::InvalidCursor,
                        "the explorer cursor is invalid",
                    ));
                }
                Ok(path)
            })
            .transpose()?;
        self.with_session(session_id, |session| {
            let inventory = session
                .vault
                .inventory(myvault_core::InventoryLimits {
                    max_depth: EXPLORER_MAX_DEPTH,
                    max_entries: EXPLORER_MAX_SCAN,
                })
                .map_err(map_core_error)?;
            let start = after.as_ref().map_or(0, |cursor| {
                inventory.partition_point(|entry| entry.path.as_str() <= cursor.as_str())
            });
            let end = inventory.len().min(start.saturating_add(limit));
            let entries = inventory[start..end]
                .iter()
                .map(|entry| ExplorerEntryDto {
                    path: entry.path.as_str().to_owned(),
                    kind: match entry.kind {
                        myvault_core::InventoryKind::Markdown => ExplorerKindDto::Markdown,
                        myvault_core::InventoryKind::File => ExplorerKindDto::File,
                    },
                    byte_len: entry.size,
                })
                .collect::<Vec<_>>();
            Ok(ExplorerPageDto {
                session_id,
                next_after: entries.last().map(|entry| entry.path.clone()),
                has_more: end < inventory.len(),
                scanned_entries: inventory.len(),
                entries,
            })
        })
    }

    /// Replaces one Markdown note only when its exact current revision matches.
    ///
    /// # Errors
    /// Rejects invalid paths/revisions, oversized text, stale revisions, unknown
    /// publication outcomes, unavailable evidence, and stale sessions.
    pub fn save_note(
        &self,
        session_id: VaultSessionId,
        portable_path: &str,
        text: &str,
        expected_revision_hex: &str,
        expected_byte_len: u64,
    ) -> Result<SaveNoteDto, AppError> {
        self.save_note_with_hook(
            session_id,
            portable_path,
            text,
            expected_revision_hex,
            expected_byte_len,
            || {},
        )
    }

    fn save_note_with_hook(
        &self,
        session_id: VaultSessionId,
        portable_path: &str,
        text: &str,
        expected_revision_hex: &str,
        expected_byte_len: u64,
        after_mutating_session_lock: impl FnOnce(),
    ) -> Result<SaveNoteDto, AppError> {
        let path = VaultPath::from_portable(portable_path).map_err(|_| invalid_path_error())?;
        let supported_extension = path
            .as_path()
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| matches!(extension, "md" | "MD"));
        if !supported_extension {
            return Err(invalid_path_error());
        }
        let bytes = text.as_bytes();
        if bytes.len() > myvault_core::MAX_NOTE_BYTES {
            return Err(resource_limit_error());
        }
        let expected =
            myvault_core::FileRevision::new(expected_revision_hex.to_owned(), expected_byte_len)
                .map_err(map_save_error)?;
        let replacement = myvault_core::FileRevision::from_bytes(bytes);
        let canonical = path.as_str().to_owned();
        // Mutations take the session read lock before the core vault mutation
        // lock. Activation/close need the session write lock and never hold a
        // core mutation lock, so there is no reverse lock order. Holding this
        // guard linearizes publication before a later switch/close; the hook is
        // private and exists only for deterministic lock-order tests.
        let current = self.session.read().map_err(|_| internal_error())?;
        let session = current.as_ref().ok_or_else(no_session_error)?;
        if session.id != session_id {
            return Err(stale_session_error());
        }
        after_mutating_session_lock();
        if let Some(runtime) = &session.snapshots {
            let current_note = session
                .vault
                .read_note_with_revision(&path)
                .map_err(map_save_error)?;
            if current_note.revision != expected {
                return Err(AppError::stale_revision());
            }
            let created_at_unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| AppError::recovery_unavailable())?
                .as_millis()
                .try_into()
                .map_err(|_| AppError::recovery_unavailable())?;
            let manifest = SnapshotManifest::new(
                Uuid::new_v4(),
                runtime.vault_id,
                path.as_str(),
                created_at_unix_ms,
                SnapshotRevision::from_bytes(&current_note.bytes),
            )
            .map_err(|_| AppError::recovery_unavailable())?;
            runtime
                .store
                .publish(&manifest, &current_note.bytes)
                .map_err(|_| AppError::recovery_unavailable())?;
        }
        let outcome = session
            .vault
            .replace_content_file_if_revision(&path, &expected, bytes, myvault_core::MAX_NOTE_BYTES)
            .map_err(map_save_error)?;
        let myvault_core::ReplaceContentOutcome::Replaced(durability) = outcome;
        Ok(SaveNoteDto {
            session_id,
            path: canonical,
            revision_hex: replacement.hex,
            byte_len: replacement.byte_len,
            durability: match durability {
                myvault_core::MoveDurability::FullySynced => SaveDurabilityDto::FullySynced,
                myvault_core::MoveDurability::DirectorySyncUnsupported => {
                    SaveDurabilityDto::DirectorySyncUnsupported
                }
            },
        })
    }

    fn with_session<T>(
        &self,
        requested: VaultSessionId,
        operation: impl FnOnce(&VaultSession) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let snapshot = {
            let current = self.session.read().map_err(|_| internal_error())?;
            let active = current.as_ref().ok_or_else(no_session_error)?;
            if active.id != requested {
                return Err(stale_session_error());
            }
            Arc::clone(active)
        };
        let result = operation(&snapshot)?;
        let current = self.session.read().map_err(|_| internal_error())?;
        if current.as_ref().is_none_or(|active| active.id != requested) {
            return Err(stale_session_error());
        }
        Ok(result)
    }
}

fn open_snapshot_runtime(
    app_data_root: &Path,
    vault: &Vault,
    vault_id: Uuid,
) -> Result<SnapshotRuntime, AppError> {
    let store = SnapshotStore::open(app_data_root, vault.root(), vault_id)
        .map_err(|_| AppError::recovery_unavailable())?;
    Ok(SnapshotRuntime { vault_id, store })
}

fn vault_id_for(vault: &Vault) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, &vault_identity_bytes(vault.root()))
}

fn vault_identity_bytes(root: &Path) -> Vec<u8> {
    let mut identity = b"myvault:local-vault:v1\0".to_vec();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        identity.extend_from_slice(root.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in root.as_os_str().encode_wide() {
            identity.extend_from_slice(&unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    identity.extend_from_slice(root.to_string_lossy().as_bytes());
    identity
}

fn map_trash_entry(entry: TrashListEvidence) -> TrashItemDto {
    match entry {
        TrashListEvidence::Supported {
            trash_id,
            manifest,
            manifest_digest,
        } => TrashItemDto {
            trash_id: trash_id.to_string(),
            evidence: TrashEvidenceDto::Supported {
                original_path: manifest.original_path,
                trashed_at_unix_ms: manifest.trashed_at_unix_ms,
                revision_hex: manifest.revision.hex,
                byte_len: manifest.revision.byte_len,
                manifest_digest: manifest_digest.as_str().to_owned(),
            },
        },
        TrashListEvidence::Opaque { trash_id } => TrashItemDto {
            trash_id: trash_id.to_string(),
            evidence: TrashEvidenceDto::Opaque,
        },
    }
}

fn map_note_error(error: CoreError) -> AppError {
    match error {
        CoreError::InvalidRelativePath(_)
        | CoreError::PathEscapesVault(_)
        | CoreError::AutomaticObsidianWriteDenied(_)
        | CoreError::TrashWriteDenied(_)
        | CoreError::TrashAccessDenied(_)
        | CoreError::InvalidMove { .. }
        | CoreError::RevisionTargetNotFile(_) => invalid_path_error(),
        CoreError::ResourceLimitExceeded { .. } => resource_limit_error(),
        CoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            AppError::new(AppErrorCode::NoteNotFound, "the note was not found")
        }
        _ => map_core_error(error),
    }
}

fn map_save_error(error: CoreError) -> AppError {
    match error {
        CoreError::InvalidRevision => AppError::new(
            AppErrorCode::InvalidRevision,
            "the expected note revision is invalid",
        ),
        CoreError::StaleRevision { .. } => AppError::new(
            AppErrorCode::StaleRevision,
            "the note changed before it could be saved",
        ),
        CoreError::ReplaceContentOutcomeUnknown { .. } => AppError::write_outcome_unknown(),
        CoreError::InvalidRelativePath(_)
        | CoreError::PathEscapesVault(_)
        | CoreError::AutomaticObsidianWriteDenied(_)
        | CoreError::TrashWriteDenied(_)
        | CoreError::TrashAccessDenied(_)
        | CoreError::InvalidMove { .. }
        | CoreError::RevisionTargetNotFile(_) => invalid_path_error(),
        CoreError::ResourceLimitExceeded { .. } => resource_limit_error(),
        _ => map_core_error(error),
    }
}

// Consume the path-bearing source at the frontend boundary instead of letting
// callers retain or accidentally serialize it.
#[allow(clippy::needless_pass_by_value)]
fn map_core_error(error: CoreError) -> AppError {
    if matches!(error, CoreError::ResourceLimitExceeded { .. }) {
        resource_limit_error()
    } else {
        AppError::new(
            AppErrorCode::VaultUnavailable,
            "vault evidence is unavailable",
        )
    }
}

const fn no_session_error() -> AppError {
    AppError::new(AppErrorCode::NoActiveSession, "no vault session is active")
}

const fn stale_session_error() -> AppError {
    AppError::new(
        AppErrorCode::StaleSession,
        "the vault session is no longer active",
    )
}

const fn invalid_path_error() -> AppError {
    AppError::new(AppErrorCode::InvalidPath, "the note path is invalid")
}

const fn resource_limit_error() -> AppError {
    AppError::new(
        AppErrorCode::ResourceLimit,
        "the requested resource exceeds its safe limit",
    )
}

const fn invalid_session_id_error() -> AppError {
    AppError::new(
        AppErrorCode::InvalidSessionId,
        "the vault session identifier is invalid",
    )
}

const fn internal_error() -> AppError {
    AppError::internal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Arc, Barrier};

    #[test]
    fn native_context_is_stable_per_vault_and_rejects_stale_sessions() {
        let temporary = tempfile::tempdir().expect("temporary");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir(&first).expect("first");
        fs::create_dir(&second).expect("second");
        let first = first.canonicalize().expect("canonical first");
        let second = second.canonicalize().expect("canonical second");
        let service = AppService::new();

        let first_session = service
            .activate_trusted_vault(Vault::open(&first).expect("first vault"))
            .expect("activate first")
            .session_id
            .expect("first session");
        let first_context = service
            .native_vault_context(first_session)
            .expect("first native context");
        assert_eq!(first_context.session_id(), first_session);
        assert_eq!(first_context.vault_root(), first);
        assert_eq!(first_context.app_data_root(), None);
        assert!(!first_context.vault_id().is_nil());

        let replacement_session = service
            .activate_trusted_vault(Vault::open(&first).expect("same first vault"))
            .expect("reactivate first")
            .session_id
            .expect("replacement session");
        let replacement_context = service
            .native_vault_context(replacement_session)
            .expect("replacement native context");
        assert_eq!(replacement_context.vault_id(), first_context.vault_id());
        let Err(stale_error) = service.native_vault_context(first_session) else {
            panic!("old session should be rejected");
        };
        assert_eq!(stale_error.code, AppErrorCode::StaleSession);

        let second_session = service
            .activate_trusted_vault(Vault::open(&second).expect("second vault"))
            .expect("activate second")
            .session_id
            .expect("second session");
        let second_context = service
            .native_vault_context(second_session)
            .expect("second native context");
        assert_ne!(second_context.vault_id(), first_context.vault_id());
        service
            .confirm_active_session(second_session)
            .expect("second remains active");
    }

    #[test]
    fn in_flight_old_session_result_is_suppressed_after_switch_or_close() {
        for switch in [false, true] {
            let temporary = tempfile::tempdir().expect("temporary");
            let first = temporary.path().join("first");
            let second = temporary.path().join("second");
            fs::create_dir(&first).expect("first");
            fs::create_dir(&second).expect("second");
            let first = first.canonicalize().expect("canonical first");
            let second = second.canonicalize().expect("canonical second");
            let service = AppService::new();
            let first_id = service
                .activate_trusted_vault(Vault::open(&first).expect("first vault"))
                .expect("activate first")
                .session_id
                .expect("first id");
            let snapped = Arc::new(Barrier::new(2));
            let resume = Arc::new(Barrier::new(2));
            std::thread::scope(|scope| {
                let snapped_worker = Arc::clone(&snapped);
                let resume_worker = Arc::clone(&resume);
                let service_ref = &service;
                let handle = scope.spawn(move || {
                    service_ref.with_session(first_id, |_| {
                        snapped_worker.wait();
                        resume_worker.wait();
                        Ok(NoteDto {
                            session_id: first_id,
                            path: "old.md".to_owned(),
                            text: "old vault result".to_owned(),
                            revision_hex: "0".repeat(64),
                            byte_len: 16,
                        })
                    })
                });
                snapped.wait();
                if switch {
                    service
                        .activate_trusted_vault(Vault::open(&second).expect("second vault"))
                        .expect("switch session");
                } else {
                    service.close(first_id).expect("close session");
                }
                resume.wait();
                let error = handle
                    .join()
                    .expect("worker")
                    .expect_err("old DTO suppressed");
                assert_eq!(error.code, AppErrorCode::StaleSession);
            });
        }
    }

    #[test]
    fn save_linearizes_before_switch_or_close_and_returns_success_not_stale() {
        for switch in [false, true] {
            let temporary = tempfile::tempdir().expect("temporary");
            let first = temporary.path().join("first");
            let second = temporary.path().join("second");
            fs::create_dir(&first).expect("first");
            fs::create_dir(&second).expect("second");
            fs::write(first.join("note.md"), b"old").expect("old note");
            let first = first.canonicalize().expect("canonical first");
            let second = second.canonicalize().expect("canonical second");
            let service = AppService::new();
            let first_id = service
                .activate_trusted_vault(Vault::open(&first).expect("first vault"))
                .expect("activate first")
                .session_id
                .expect("first id");
            let expected = myvault_core::FileRevision::from_bytes(b"old");
            let locked = Arc::new(Barrier::new(2));
            let resume = Arc::new(Barrier::new(2));
            std::thread::scope(|scope| {
                let locked_worker = Arc::clone(&locked);
                let resume_worker = Arc::clone(&resume);
                let service_ref = &service;
                let save = scope.spawn(move || {
                    service_ref.save_note_with_hook(
                        first_id,
                        "note.md",
                        "ใหม่",
                        &expected.hex,
                        expected.byte_len,
                        || {
                            locked_worker.wait();
                            resume_worker.wait();
                        },
                    )
                });
                locked.wait();
                assert!(matches!(
                    service.session.try_write(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                let (started_tx, started_rx) = std::sync::mpsc::channel();
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let service_ref = &service;
                let transition = scope.spawn(move || {
                    started_tx.send(()).expect("started");
                    let result = if switch {
                        service_ref
                            .activate_trusted_vault(Vault::open(&second).expect("second vault"))
                            .map(|_| ())
                    } else {
                        service_ref.close(first_id).map(|_| ())
                    };
                    done_tx.send(()).expect("done");
                    result
                });
                started_rx.recv().expect("transition started");
                assert!(matches!(
                    done_rx.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                resume.wait();
                let saved = save.join().expect("save worker").expect("save succeeds");
                assert_eq!(saved.session_id, first_id);
                assert_eq!(
                    saved.revision_hex,
                    myvault_core::FileRevision::from_bytes("ใหม่".as_bytes()).hex
                );
                transition
                    .join()
                    .expect("transition worker")
                    .expect("transition succeeds after save");
                done_rx.recv().expect("transition done");
            });
            assert_eq!(
                fs::read(first.join("note.md")).expect("published note"),
                "ใหม่".as_bytes()
            );
        }
    }
}
