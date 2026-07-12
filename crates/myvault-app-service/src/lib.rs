//! Tauri-free, frontend-safe read service for one active local vault.

use myvault_core::{CoreError, TrashId, TrashListEvidence, Vault, VaultPath, MAX_TRASH_PAGE_SIZE};
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

pub const EXPLORER_MAX_DEPTH: usize = 64;
pub const EXPLORER_MAX_SCAN: usize = 5_000;
pub const EXPLORER_DEFAULT_PAGE_SIZE: usize = 100;
pub const EXPLORER_MAX_PAGE_SIZE: usize = 200;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct VaultSessionId(Uuid);

impl VaultSessionId {
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
pub enum AppErrorCode {
    NoActiveSession,
    StaleSession,
    InvalidSessionId,
    InvalidPath,
    InvalidCursor,
    InvalidLimit,
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
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for AppError {}

struct VaultSession {
    id: VaultSessionId,
    vault: Vault,
}

#[derive(Default)]
pub struct AppService {
    session: RwLock<Option<Arc<VaultSession>>>,
}

impl AppService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Activates an already-open capability supplied by a trusted native picker adapter.
    ///
    /// # Errors
    /// Returns a safe internal error if the session lock is unavailable.
    pub fn activate_trusted_vault(&self, vault: Vault) -> Result<VaultStatusDto, AppError> {
        let id = VaultSessionId(Uuid::new_v4());
        let session = Arc::new(VaultSession { id, vault });
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
        | CoreError::TrashAccessDenied(_)
        | CoreError::RevisionTargetNotFile(_) => invalid_path_error(),
        CoreError::ResourceLimitExceeded { .. } => AppError::new(
            AppErrorCode::ResourceLimit,
            "the requested resource exceeds its safe limit",
        ),
        CoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            AppError::new(AppErrorCode::NoteNotFound, "the note was not found")
        }
        _ => map_core_error(error),
    }
}

// Consume the path-bearing source at the frontend boundary instead of letting
// callers retain or accidentally serialize it.
#[allow(clippy::needless_pass_by_value)]
fn map_core_error(error: CoreError) -> AppError {
    if matches!(error, CoreError::ResourceLimitExceeded { .. }) {
        AppError::new(
            AppErrorCode::ResourceLimit,
            "the requested resource exceeds its safe limit",
        )
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
}
