#![forbid(unsafe_code)]

use serde::Deserialize;
#[cfg(target_os = "android")]
use serde::Serialize;
#[cfg(target_os = "android")]
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

#[cfg(target_os = "android")]
mod mobile;
mod policy;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafEntry {
    pub path: String,
    pub kind: String,
    pub byte_len: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafInventory {
    pub entries: Vec<SafEntry>,
    pub scanned_entries: usize,
}

impl SafInventory {
    /// Normalizes native inventory ordering to the UTF-8 ordering used by
    /// Rust cursors before a caller performs partition-based pagination.
    pub fn normalize_portable_order(&mut self) {
        self.entries
            .sort_by(|left, right| policy::portable_path_cmp(&left.path, &right.path));
    }
}

#[must_use]
pub fn is_valid_explorer_cursor(value: &str) -> bool {
    policy::is_valid_portable_path(value)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafNote {
    pub text: String,
    pub revision_hex: String,
    pub byte_len: u64,
}

#[derive(Clone, Debug)]
pub struct SafSave {
    pub revision_hex: String,
    pub byte_len: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct NativeSave {
    outcome: String,
    revision_hex: Option<String>,
    byte_len: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafSaveError {
    StaleRevision,
    NoteNotFound,
    InvalidPath,
    InvalidRequest,
    WriteOutcomeUnknown,
    NativeBridge,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct NativeStatus {
    active: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct NativeChoice {
    outcome: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct PathRequest<'a> {
    path: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct SaveRequest<'a> {
    path: &'a str,
    text: &'a str,
    expected_revision_hex: &'a str,
    expected_byte_len: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafError {
    InvalidPath,
    NoteNotFound,
    NoteNotUtf8,
    ResourceLimit,
    VaultUnavailable,
    PickerBusy,
    PickerUnavailable,
    PickerPermission,
    PickerFailed,
    NativeBridge,
}

impl std::fmt::Display for SafError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "the Android note path is invalid",
            Self::NoteNotFound => "the Android note was not found",
            Self::NoteNotUtf8 => "the Android note is not valid UTF-8",
            Self::ResourceLimit => "the Android document-tree safety limit was exceeded",
            Self::VaultUnavailable => "the Android document-tree capability is unavailable",
            Self::PickerBusy => "another Android folder selection is active",
            Self::PickerUnavailable => "the Android folder picker is unavailable",
            Self::PickerPermission => "the Android folder permission is insufficient",
            Self::PickerFailed => "the Android folder selection failed",
            Self::NativeBridge => "the Android document-tree bridge failed",
        })
    }
}

impl std::error::Error for SafError {}

#[cfg(target_os = "android")]
fn saf_error_from_kind(kind: policy::NativeErrorKind) -> SafError {
    match kind {
        policy::NativeErrorKind::InvalidPath => SafError::InvalidPath,
        policy::NativeErrorKind::NoteNotFound => SafError::NoteNotFound,
        policy::NativeErrorKind::NoteNotUtf8 => SafError::NoteNotUtf8,
        policy::NativeErrorKind::ResourceLimit => SafError::ResourceLimit,
        policy::NativeErrorKind::VaultUnavailable => SafError::VaultUnavailable,
        policy::NativeErrorKind::PickerBusy => SafError::PickerBusy,
        policy::NativeErrorKind::PickerUnavailable => SafError::PickerUnavailable,
        policy::NativeErrorKind::PickerPermission => SafError::PickerPermission,
        policy::NativeErrorKind::PickerFailed => SafError::PickerFailed,
        policy::NativeErrorKind::NativeBridge => SafError::NativeBridge,
    }
}

#[cfg(target_os = "android")]
fn map_plugin_error(error: tauri::plugin::mobile::PluginInvokeError) -> SafError {
    let code = match &error {
        tauri::plugin::mobile::PluginInvokeError::InvokeRejected(response) => {
            response.code.as_deref()
        }
        _ => None,
    };
    saf_error_from_kind(policy::classify_native_error_code(code))
}

#[cfg(target_os = "android")]
pub struct VaultSaf<R: Runtime>(tauri::plugin::PluginHandle<R>);

#[cfg(target_os = "android")]
impl<R: Runtime> VaultSaf<R> {
    pub fn has_root(&self) -> Result<bool, SafError> {
        let value: NativeStatus = self
            .0
            .run_mobile_plugin("status", ())
            .map_err(map_plugin_error)?;
        Ok(value.active)
    }

    pub fn choose_root(&self) -> Result<bool, SafError> {
        let value: NativeChoice = self
            .0
            .run_mobile_plugin("chooseRoot", ())
            .map_err(map_plugin_error)?;
        Ok(value.outcome == "activated")
    }

    pub fn inventory(&self) -> Result<SafInventory, SafError> {
        self.0
            .run_mobile_plugin("inventory", ())
            .map_err(map_plugin_error)
    }

    pub fn read_note(&self, path: &str) -> Result<SafNote, SafError> {
        if !policy::is_valid_note_path(path) {
            return Err(SafError::InvalidPath);
        }
        self.0
            .run_mobile_plugin("readNote", PathRequest { path })
            .map_err(map_plugin_error)
    }

    /// Performs a revision-guarded, best-effort SAF write.
    ///
    /// Android document providers do not expose an atomic compare-and-swap or
    /// portable atomic replacement primitive. The native adapter therefore
    /// verifies the expected bytes, writes in place, syncs when supported, and
    /// reads back the result. External writers can still race this operation,
    /// and any ambiguous failure is reported as `WriteOutcomeUnknown`.
    pub fn save_note(
        &self,
        path: &str,
        text: &str,
        expected_revision_hex: &str,
        expected_byte_len: u64,
    ) -> Result<SafSave, SafSaveError> {
        if !policy::is_valid_note_path(path) {
            return Err(SafSaveError::InvalidPath);
        }
        let response: NativeSave = self
            .0
            .run_mobile_plugin(
                "saveNote",
                SaveRequest {
                    path,
                    text,
                    expected_revision_hex,
                    expected_byte_len,
                },
            )
            .map_err(|_| SafSaveError::NativeBridge)?;
        match response.outcome.as_str() {
            "saved" => Ok(SafSave {
                revision_hex: response.revision_hex.ok_or(SafSaveError::NativeBridge)?,
                byte_len: response.byte_len.ok_or(SafSaveError::NativeBridge)?,
            }),
            "staleRevision" => Err(SafSaveError::StaleRevision),
            "noteNotFound" => Err(SafSaveError::NoteNotFound),
            "invalidRequest" => Err(SafSaveError::InvalidRequest),
            "writeOutcomeUnknown" => Err(SafSaveError::WriteOutcomeUnknown),
            _ => Err(SafSaveError::NativeBridge),
        }
    }
}

#[cfg(target_os = "android")]
pub trait VaultSafExt<R: Runtime> {
    fn vault_saf(&self) -> &VaultSaf<R>;
}

#[cfg(target_os = "android")]
impl<R: Runtime, T: Manager<R>> VaultSafExt<R> for T {
    fn vault_saf(&self) -> &VaultSaf<R> {
        self.state::<VaultSaf<R>>().inner()
    }
}

#[cfg(target_os = "android")]
#[must_use]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("vault-saf")
        .setup(|app, api| {
            let handle = api
                .register_android_plugin("com.abhuri.myvault.vaultsaf", "VaultSafPlugin")
                .map_err(|_| SafError::NativeBridge)?;
            app.manage(VaultSaf(handle));
            Ok(())
        })
        .build()
}
