use std::cmp::Ordering;

pub(crate) const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(target_os = "android", test))]
pub(crate) enum NativeErrorKind {
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

#[cfg(any(target_os = "android", test))]
pub(crate) fn classify_native_error_code(code: Option<&str>) -> NativeErrorKind {
    match code {
        Some("INVALID_PATH") => NativeErrorKind::InvalidPath,
        Some("NOTE_NOT_FOUND") => NativeErrorKind::NoteNotFound,
        Some("NOTE_NOT_UTF8") => NativeErrorKind::NoteNotUtf8,
        Some("RESOURCE_LIMIT") => NativeErrorKind::ResourceLimit,
        Some("VAULT_UNAVAILABLE") => NativeErrorKind::VaultUnavailable,
        Some("PICKER_BUSY") => NativeErrorKind::PickerBusy,
        Some("PICKER_UNAVAILABLE") => NativeErrorKind::PickerUnavailable,
        Some("PICKER_PERMISSION") => NativeErrorKind::PickerPermission,
        Some("PICKER_FAILED") => NativeErrorKind::PickerFailed,
        _ => NativeErrorKind::NativeBridge,
    }
}

pub(crate) fn is_valid_portable_path(value: &str) -> bool {
    if value.is_empty() || value.starts_with('/') || value.contains('\\') || value.contains('\0') {
        return false;
    }

    let mut parts = value.split('/');
    let Some(first) = parts.next() else {
        return false;
    };
    if matches!(first, ".trash" | ".obsidian") {
        return false;
    }

    let mut depth = 1;
    if matches!(first, "" | "." | "..") {
        return false;
    }
    for part in parts {
        depth += 1;
        if depth > MAX_DEPTH || matches!(part, "" | "." | "..") {
            return false;
        }
    }

    true
}

#[cfg(any(target_os = "android", test))]
pub(crate) fn is_valid_note_path(value: &str) -> bool {
    is_valid_portable_path(value)
        && value
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("md"))
}

/// Portable paths are ordered by unsigned UTF-8 bytes, which is also Rust's
/// `str` ordering. Native implementations must not rely on UTF-16 ordering.
pub(crate) fn portable_path_cmp(left: &str, right: &str) -> Ordering {
    left.as_bytes().cmp(right.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_path_policy_blocks_protected_and_noncanonical_paths() {
        for path in [
            ".trash/note.md",
            ".obsidian/workspace.md",
            "/absolute.md",
            "Notes/../secret.md",
            "Notes//empty.md",
            "Notes/not-markdown.txt",
        ] {
            assert!(!is_valid_note_path(path), "accepted {path}");
        }
        assert!(is_valid_note_path("Notes/ภาษาไทย.MD"));
        assert!(is_valid_portable_path("Attachments/ภาพ.png"));
        assert!(!is_valid_portable_path(".obsidian/config.json"));
    }

    #[test]
    fn native_error_codes_keep_stable_semantics() {
        for (code, expected) in [
            ("INVALID_PATH", NativeErrorKind::InvalidPath),
            ("NOTE_NOT_FOUND", NativeErrorKind::NoteNotFound),
            ("NOTE_NOT_UTF8", NativeErrorKind::NoteNotUtf8),
            ("RESOURCE_LIMIT", NativeErrorKind::ResourceLimit),
            ("VAULT_UNAVAILABLE", NativeErrorKind::VaultUnavailable),
            ("PICKER_BUSY", NativeErrorKind::PickerBusy),
            ("PICKER_UNAVAILABLE", NativeErrorKind::PickerUnavailable),
            ("PICKER_PERMISSION", NativeErrorKind::PickerPermission),
            ("PICKER_FAILED", NativeErrorKind::PickerFailed),
        ] {
            assert_eq!(classify_native_error_code(Some(code)), expected, "{code}");
        }
        assert_eq!(
            classify_native_error_code(Some("unexpected-native-code")),
            NativeErrorKind::NativeBridge
        );
        assert_eq!(
            classify_native_error_code(None),
            NativeErrorKind::NativeBridge
        );
    }

    #[test]
    fn portable_path_order_uses_utf8_for_unicode_pagination() {
        let mut paths = vec!["😀.md", "ภาษาไทย.md", "\u{e000}.md", "A.md"];
        paths.sort_by(|left, right| portable_path_cmp(left, right));
        assert_eq!(paths, ["A.md", "ภาษาไทย.md", "\u{e000}.md", "😀.md"]);
    }
}
