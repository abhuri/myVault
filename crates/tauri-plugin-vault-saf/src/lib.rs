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

/// Binary-safe native SAF read result. This type is not serializable to the
/// frontend; the byte body remains inside native Rust integration code.
#[derive(Clone, Debug)]
pub struct SafBinary {
    pub bytes: Vec<u8>,
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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct NativeBinary {
    bytes_base64: String,
    revision_hex: String,
    byte_len: u64,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafTransferError {
    StaleRevision,
    AlreadyExists,
    NotFound,
    InvalidPath,
    InvalidRequest,
    DigestMismatch,
    ResourceLimit,
    UnsupportedReplace,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct BinaryWriteRequest<'a> {
    path: &'a str,
    bytes_base64: String,
    sha256_hex: &'a str,
    byte_len: u64,
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

    /// Reads one bounded binary document through the held SAF tree. The body
    /// remains native-only and is decoded only after exact length validation.
    pub fn read_binary(&self, path: &str, max_bytes: usize) -> Result<SafBinary, SafTransferError> {
        if !policy::is_valid_portable_path(path) {
            return Err(SafTransferError::InvalidPath);
        }
        let response: NativeBinary = self
            .0
            .run_mobile_plugin("readBinary", PathRequest { path })
            .map_err(|_| SafTransferError::NativeBridge)?;
        let bytes = decode_base64_bounded(&response.bytes_base64, max_bytes)?;
        if bytes.len() as u64 != response.byte_len {
            return Err(SafTransferError::NativeBridge);
        }
        Ok(SafBinary {
            bytes,
            revision_hex: response.revision_hex,
            byte_len: response.byte_len,
        })
    }

    /// Creates a new SAF document without intentionally replacing an existing
    /// portable path and verifies the exact bytes by native readback.
    pub fn create_binary(
        &self,
        path: &str,
        bytes: &[u8],
        sha256_hex: &str,
    ) -> Result<SafSave, SafTransferError> {
        self.write_binary(path, bytes, sha256_hex)
    }

    /// Validates replacement arguments, then fails closed without invoking the
    /// native writer. SAF cannot provide a trustworthy atomic compare-and-swap,
    /// so R2 does not replace existing transfer targets.
    pub fn replace_binary_if_revision(
        &self,
        path: &str,
        bytes: &[u8],
        sha256_hex: &str,
        expected_revision_hex: &str,
        expected_byte_len: u64,
    ) -> Result<SafSave, SafTransferError> {
        if !policy::is_valid_portable_path(path)
            || bytes.len() > MAX_NATIVE_TRANSFER_BYTES
            || !is_canonical_digest(sha256_hex)
            || !is_canonical_digest(expected_revision_hex)
            || expected_byte_len > MAX_NATIVE_TRANSFER_BYTES as u64
        {
            return Err(SafTransferError::InvalidRequest);
        }
        Err(SafTransferError::UnsupportedReplace)
    }

    fn write_binary(
        &self,
        path: &str,
        bytes: &[u8],
        sha256_hex: &str,
    ) -> Result<SafSave, SafTransferError> {
        if !policy::is_valid_portable_path(path)
            || bytes.len() > MAX_NATIVE_TRANSFER_BYTES
            || !is_canonical_digest(sha256_hex)
        {
            return Err(SafTransferError::InvalidRequest);
        }
        let response: NativeSave = self
            .0
            .run_mobile_plugin(
                "createBinary",
                BinaryWriteRequest {
                    path,
                    bytes_base64: encode_base64(bytes),
                    sha256_hex,
                    byte_len: bytes.len() as u64,
                },
            )
            .map_err(|_| SafTransferError::NativeBridge)?;
        match response.outcome.as_str() {
            "saved" => Ok(SafSave {
                revision_hex: response
                    .revision_hex
                    .ok_or(SafTransferError::NativeBridge)?,
                byte_len: response.byte_len.ok_or(SafTransferError::NativeBridge)?,
            }),
            "staleRevision" => Err(SafTransferError::StaleRevision),
            "alreadyExists" => Err(SafTransferError::AlreadyExists),
            "notFound" => Err(SafTransferError::NotFound),
            "digestMismatch" => Err(SafTransferError::DigestMismatch),
            "resourceLimit" => Err(SafTransferError::ResourceLimit),
            "unsupportedReplace" => Err(SafTransferError::UnsupportedReplace),
            "invalidRequest" => Err(SafTransferError::InvalidRequest),
            "writeOutcomeUnknown" => Err(SafTransferError::WriteOutcomeUnknown),
            _ => Err(SafTransferError::NativeBridge),
        }
    }
}

#[cfg(target_os = "android")]
const MAX_NATIVE_TRANSFER_BYTES: usize = 16 * 1024 * 1024;

#[cfg(any(target_os = "android", test))]
fn is_canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(any(target_os = "android", test))]
fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(char::from(ALPHABET[usize::from(first >> 2)]));
        output.push(char::from(
            ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        output.push(if chunk.len() > 1 {
            char::from(ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))])
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            char::from(ALPHABET[usize::from(third & 0x3f)])
        } else {
            '='
        });
    }
    output
}

#[cfg(any(target_os = "android", test))]
fn decode_base64_bounded(value: &str, max_bytes: usize) -> Result<Vec<u8>, SafTransferError> {
    if value.len() % 4 != 0 || value.len() / 4 * 3 > max_bytes.saturating_add(2) {
        return Err(SafTransferError::ResourceLimit);
    }
    let mut output = Vec::with_capacity((value.len() / 4) * 3);
    for (index, chunk) in value.as_bytes().chunks_exact(4).enumerate() {
        let last = index + 1 == value.len() / 4;
        let a = base64_value(chunk[0]).ok_or(SafTransferError::NativeBridge)?;
        let b = base64_value(chunk[1]).ok_or(SafTransferError::NativeBridge)?;
        let c = if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' {
                return Err(SafTransferError::NativeBridge);
            }
            0
        } else {
            base64_value(chunk[2]).ok_or(SafTransferError::NativeBridge)?
        };
        let d = if chunk[3] == b'=' {
            if !last {
                return Err(SafTransferError::NativeBridge);
            }
            0
        } else {
            base64_value(chunk[3]).ok_or(SafTransferError::NativeBridge)?
        };
        output.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            output.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            output.push((c << 6) | d);
        }
        if output.len() > max_bytes {
            return Err(SafTransferError::ResourceLimit);
        }
    }
    Ok(output)
}

#[cfg(any(target_os = "android", test))]
fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
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

#[cfg(test)]
mod transfer_tests {
    use super::*;

    #[test]
    fn native_base64_round_trips_zero_unicode_and_large_binary() {
        for bytes in [
            Vec::new(),
            "ไทย 🧪".as_bytes().to_vec(),
            (0..5 * 1024 * 1024 + 13)
                .map(|index| u8::try_from((index * 211 + 7) % 256).unwrap())
                .collect(),
        ] {
            let encoded = encode_base64(&bytes);
            assert_eq!(decode_base64_bounded(&encoded, bytes.len()).unwrap(), bytes);
        }
    }

    #[test]
    fn native_base64_and_digest_validation_are_strict_and_bounded() {
        assert!(is_canonical_digest(&"a".repeat(64)));
        assert!(!is_canonical_digest(&"A".repeat(64)));
        assert!(matches!(
            decode_base64_bounded("AA==", 0),
            Err(SafTransferError::ResourceLimit)
        ));
        for malformed in ["A", "AA=A", "AA==AAAA", "****"] {
            assert!(decode_base64_bounded(malformed, 16).is_err(), "{malformed}");
        }
    }
}
