#![forbid(unsafe_code)]

use serde::Deserialize;
#[cfg(target_os = "android")]
use serde::Serialize;
#[cfg(target_os = "android")]
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};
use uuid::Uuid;

#[cfg(target_os = "android")]
mod mobile;
mod policy;

#[cfg(any(target_os = "android", test))]
const SAF_VAULT_NAMESPACE: Uuid = Uuid::from_u128(0xf3d7_7615_8097_5c85_9a33_6b3f_62ae_d477);

/// Native-only proof that one exact persisted SAF tree was active when the
/// capability was issued. It intentionally has no serialization surface and
/// redacts its stable root identity from debug output.
#[derive(Clone, Eq, PartialEq)]
pub struct SafVaultCapability {
    root_identity_hex: String,
    vault_id: Uuid,
}

impl SafVaultCapability {
    #[cfg(any(target_os = "android", test))]
    fn from_root_identity(root_identity_hex: String) -> Result<Self, SafError> {
        let identity = decode_root_identity(&root_identity_hex).ok_or(SafError::NativeBridge)?;
        Ok(Self {
            root_identity_hex,
            vault_id: Uuid::new_v5(&SAF_VAULT_NAMESPACE, &identity),
        })
    }

    #[cfg(target_os = "android")]
    fn expected_root_identity_hex(&self) -> &str {
        &self.root_identity_hex
    }

    /// Returns the stable per-tree UUID used only for native per-Vault state.
    #[must_use]
    pub const fn vault_id(&self) -> Uuid {
        self.vault_id
    }
}

impl std::fmt::Debug for SafVaultCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SafVaultCapability")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafEntry {
    pub path: String,
    pub kind: String,
    pub byte_len: u64,
    pub byte_len_known: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafInventory {
    pub entries: Vec<SafEntry>,
    pub scanned_entries: usize,
    /// Opaque native change generation consumed by this successful bounded scan.
    pub change_generation: u64,
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

/// Opaque, coalesced signal that the held SAF tree may have changed.
///
/// The signal intentionally contains no URI, document ID, path, or content.
/// Callers must treat it only as a reason to run the existing bounded inventory.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SafChangeHint {
    pub dirty: bool,
    pub generation: u64,
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
    VaultUnavailable,
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
    VaultUnavailable,
    UnsupportedReplace,
    WriteOutcomeUnknown,
    NativeBridge,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct NativeStatus {
    active: bool,
    root_identity_hex: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct NativeChoice {
    outcome: String,
    root_identity_hex: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct PathRequest<'a> {
    expected_root_identity_hex: &'a str,
    path: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct RootRequest<'a> {
    expected_root_identity_hex: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct SaveRequest<'a> {
    expected_root_identity_hex: &'a str,
    path: &'a str,
    text: &'a str,
    expected_revision_hex: &'a str,
    expected_byte_len: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(target_os = "android")]
struct BinaryWriteRequest<'a> {
    expected_root_identity_hex: &'a str,
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
fn map_transfer_plugin_error(error: tauri::plugin::mobile::PluginInvokeError) -> SafTransferError {
    match map_plugin_error(error) {
        SafError::InvalidPath => SafTransferError::InvalidPath,
        SafError::NoteNotFound => SafTransferError::NotFound,
        SafError::ResourceLimit => SafTransferError::ResourceLimit,
        SafError::VaultUnavailable => SafTransferError::VaultUnavailable,
        SafError::NoteNotUtf8
        | SafError::PickerBusy
        | SafError::PickerUnavailable
        | SafError::PickerPermission
        | SafError::PickerFailed
        | SafError::NativeBridge => SafTransferError::NativeBridge,
    }
}

#[cfg(target_os = "android")]
pub struct VaultSaf<R: Runtime>(tauri::plugin::PluginHandle<R>);

#[cfg(target_os = "android")]
impl<R: Runtime> VaultSaf<R> {
    /// Returns a native-only capability for the exact currently persisted SAF
    /// root, or `None` when permission/root validation is unavailable.
    pub fn active_root(&self) -> Result<Option<SafVaultCapability>, SafError> {
        let value: NativeStatus = self
            .0
            .run_mobile_plugin("status", ())
            .map_err(map_plugin_error)?;
        native_capability(value.active, value.root_identity_hex)
    }

    /// Runs the native picker and returns the exact activated SAF capability.
    pub fn choose_root(&self) -> Result<Option<SafVaultCapability>, SafError> {
        let value: NativeChoice = self
            .0
            .run_mobile_plugin("chooseRoot", ())
            .map_err(map_plugin_error)?;
        match value.outcome.as_str() {
            "cancelled" if value.root_identity_hex.is_none() => Ok(None),
            "activated" => value
                .root_identity_hex
                .ok_or(SafError::NativeBridge)
                .and_then(SafVaultCapability::from_root_identity)
                .map(Some),
            _ => Err(SafError::NativeBridge),
        }
    }

    pub fn inventory(&self, vault: &SafVaultCapability) -> Result<SafInventory, SafError> {
        let inventory: SafInventory = self
            .0
            .run_mobile_plugin(
                "inventory",
                RootRequest {
                    expected_root_identity_hex: vault.expected_root_identity_hex(),
                },
            )
            .map_err(map_plugin_error)?;
        validate_change_generation(inventory.change_generation)?;
        Ok(inventory)
    }

    /// Reads one coalesced native dirty hint without enumerating the SAF tree.
    /// A successful `inventory` call consumes the exact generation it observed.
    pub fn change_hint(&self, vault: &SafVaultCapability) -> Result<SafChangeHint, SafError> {
        let hint: SafChangeHint = self
            .0
            .run_mobile_plugin(
                "changeHint",
                RootRequest {
                    expected_root_identity_hex: vault.expected_root_identity_hex(),
                },
            )
            .map_err(map_plugin_error)?;
        validate_change_generation(hint.generation)?;
        Ok(hint)
    }

    pub fn read_note(&self, vault: &SafVaultCapability, path: &str) -> Result<SafNote, SafError> {
        if !policy::is_valid_note_path(path) {
            return Err(SafError::InvalidPath);
        }
        self.0
            .run_mobile_plugin(
                "readNote",
                PathRequest {
                    expected_root_identity_hex: vault.expected_root_identity_hex(),
                    path,
                },
            )
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
        vault: &SafVaultCapability,
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
                    expected_root_identity_hex: vault.expected_root_identity_hex(),
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
            "vaultUnavailable" => Err(SafSaveError::VaultUnavailable),
            "writeOutcomeUnknown" => Err(SafSaveError::WriteOutcomeUnknown),
            _ => Err(SafSaveError::NativeBridge),
        }
    }

    /// Reads one bounded binary document through the held SAF tree. The body
    /// remains native-only and is decoded only after exact length validation.
    pub fn read_binary(
        &self,
        vault: &SafVaultCapability,
        path: &str,
        max_bytes: usize,
    ) -> Result<SafBinary, SafTransferError> {
        if !policy::is_valid_portable_path(path) {
            return Err(SafTransferError::InvalidPath);
        }
        let response: NativeBinary = self
            .0
            .run_mobile_plugin(
                "readBinary",
                PathRequest {
                    expected_root_identity_hex: vault.expected_root_identity_hex(),
                    path,
                },
            )
            .map_err(map_transfer_plugin_error)?;
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
        vault: &SafVaultCapability,
        path: &str,
        bytes: &[u8],
        sha256_hex: &str,
    ) -> Result<SafSave, SafTransferError> {
        self.write_binary(vault, path, bytes, sha256_hex)
    }

    /// Validates replacement arguments, then fails closed without invoking the
    /// native writer. SAF cannot provide a trustworthy atomic compare-and-swap,
    /// so R2 does not replace existing transfer targets.
    pub fn replace_binary_if_revision(
        &self,
        _vault: &SafVaultCapability,
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
        vault: &SafVaultCapability,
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
                    expected_root_identity_hex: vault.expected_root_identity_hex(),
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
            "vaultUnavailable" => Err(SafTransferError::VaultUnavailable),
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
const MAX_SAFE_CHANGE_GENERATION: u64 = 9_007_199_254_740_991;

#[cfg(any(target_os = "android", test))]
fn validate_change_generation(generation: u64) -> Result<(), SafError> {
    if generation <= MAX_SAFE_CHANGE_GENERATION {
        Ok(())
    } else {
        Err(SafError::NativeBridge)
    }
}

#[cfg(any(target_os = "android", test))]
fn native_capability(
    active: bool,
    root_identity_hex: Option<String>,
) -> Result<Option<SafVaultCapability>, SafError> {
    match (active, root_identity_hex) {
        (false, None) => Ok(None),
        (true, Some(identity)) => SafVaultCapability::from_root_identity(identity).map(Some),
        _ => Err(SafError::NativeBridge),
    }
}

#[cfg(any(target_os = "android", test))]
fn decode_root_identity(value: &str) -> Option<[u8; 32]> {
    if !is_canonical_digest(value) {
        return None;
    }
    let mut output = [0_u8; 32];
    for (target, pair) in output.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let high = base16_value(pair[0])?;
        let low = base16_value(pair[1])?;
        *target = (high << 4) | low;
    }
    Some(output)
}

#[cfg(any(target_os = "android", test))]
fn base16_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

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
    fn root_capability_is_stable_distinct_and_debug_redacted() {
        let first_identity = "01".repeat(32);
        let second_identity = "02".repeat(32);
        let first = SafVaultCapability::from_root_identity(first_identity.clone()).unwrap();
        let restarted = SafVaultCapability::from_root_identity(first_identity.clone()).unwrap();
        let second = SafVaultCapability::from_root_identity(second_identity.clone()).unwrap();

        assert_eq!(first, restarted);
        assert_eq!(first.vault_id(), restarted.vault_id());
        assert_ne!(first, second);
        assert_ne!(first.vault_id(), second.vault_id());
        let debug = format!("{first:?}");
        assert_eq!(debug, "SafVaultCapability { .. }");
        assert!(!debug.contains(&first_identity));
        assert!(!debug.contains(&first.vault_id().to_string()));
    }

    #[test]
    fn native_root_evidence_is_exact_and_canonical() {
        let identity = "ab".repeat(32);
        assert!(native_capability(false, None).unwrap().is_none());
        assert!(native_capability(true, Some(identity)).unwrap().is_some());
        for malformed in [
            None,
            Some(String::new()),
            Some("AB".repeat(32)),
            Some("ab".repeat(31)),
            Some("gg".repeat(32)),
        ] {
            assert!(native_capability(true, malformed).is_err());
        }
        assert!(native_capability(false, Some("ab".repeat(32))).is_err());
    }

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

    #[test]
    fn native_change_hint_is_opaque_and_json_safe() {
        let clean = SafChangeHint {
            dirty: false,
            generation: 0,
        };
        let dirty = SafChangeHint {
            dirty: true,
            generation: MAX_SAFE_CHANGE_GENERATION,
        };
        assert_eq!(
            clean,
            SafChangeHint {
                dirty: false,
                generation: 0
            }
        );
        assert!(dirty.dirty);
        validate_change_generation(clean.generation).unwrap();
        validate_change_generation(dirty.generation).unwrap();
        assert_eq!(
            validate_change_generation(MAX_SAFE_CHANGE_GENERATION + 1),
            Err(SafError::NativeBridge)
        );
        let debug = format!("{dirty:?}");
        assert!(!debug.contains("content://"));
        assert!(!debug.contains('/'));
    }
}
