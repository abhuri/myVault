use std::fmt;
use std::path::{Component, Path};

use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::{CoreError, Result};

/// A validated, portable, non-empty path relative to a vault root.
///
/// The stored representation is UTF-8 and always uses `/` separators. This is
/// deliberately stricter than any one host filesystem so a path accepted on
/// macOS or Linux cannot become unaddressable after syncing to Windows.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VaultPath(String);

const MAX_COMPONENT_BYTES: usize = 255;
const MAX_COMPONENT_UTF16_UNITS: usize = 255;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_PATH_UTF16_UNITS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VaultPathClass {
    Content,
    ObsidianMetadata,
    Trash,
}

impl VaultPath {
    /// Validates and normalizes a vault-relative path.
    ///
    /// Empty segments and embedded `.` segments are removed. A leading `.` is
    /// rejected to avoid accepting an ambiguously relative API input.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidRelativePath`] for non-UTF-8, empty,
    /// absolute, parent-traversing, non-portable, or reserved paths.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_native(path)
    }

    /// Parses an API/storage path that must already use portable `/`
    /// separators.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidRelativePath`] when the input violates the
    /// portable vault path contract.
    pub fn from_portable(path: impl AsRef<str>) -> Result<Self> {
        let value = path.as_ref();
        let original = Path::new(value).to_path_buf();
        if value.is_empty()
            || value.starts_with('/')
            || value.starts_with("./")
            || value.contains('\\')
        {
            return Err(CoreError::InvalidRelativePath(original));
        }

        let mut components = Vec::new();
        for component in value.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." || !is_portable_component(component) {
                return Err(CoreError::InvalidRelativePath(original));
            }
            components.push(component);
        }
        Self::from_components(&components, original)
    }

    /// Converts a host-native relative path, including native separators, to
    /// the canonical portable representation used by storage and IPC.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidRelativePath`] for non-UTF-8, absolute,
    /// parent-traversing, non-portable, or reserved paths.
    pub fn from_native(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let original = path.to_path_buf();
        if path.as_os_str().is_empty() || path.is_absolute() {
            return Err(CoreError::InvalidRelativePath(original));
        }

        let mut components = Vec::new();
        for (index, component) in path.components().enumerate() {
            match component {
                Component::Normal(value) => {
                    let Some(value) = value.to_str() else {
                        return Err(CoreError::InvalidRelativePath(original));
                    };
                    if !is_portable_component(value) {
                        return Err(CoreError::InvalidRelativePath(original));
                    }
                    components.push(value);
                }
                Component::CurDir if index > 0 => {}
                Component::CurDir
                | Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_) => {
                    return Err(CoreError::InvalidRelativePath(original));
                }
            }
        }
        Self::from_components(&components, original)
    }

    fn from_components(components: &[&str], original: std::path::PathBuf) -> Result<Self> {
        if components.is_empty() {
            return Err(CoreError::InvalidRelativePath(original));
        }
        let canonical = components.join("/");
        if canonical.len() > MAX_PATH_BYTES
            || canonical.encode_utf16().count() > MAX_PATH_UTF16_UNITS
        {
            return Err(CoreError::InvalidRelativePath(original));
        }
        Ok(Self(canonical))
    }

    /// Returns the canonical UTF-8, slash-separated representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// Returns a normalization and case-insensitive key for collision checks.
    ///
    /// This key is not used as the display path. NFKC handles composed versus
    /// decomposed Unicode and compatibility forms, while the fold catches the
    /// important Windows/default-macOS case collisions before a mutation.
    #[must_use]
    pub fn collision_key(&self) -> String {
        self.0.nfkc().case_fold().nfkc().collect()
    }

    #[must_use]
    pub fn is_obsidian_metadata(&self) -> bool {
        self.classify() == VaultPathClass::ObsidianMetadata
    }

    #[must_use]
    pub(crate) fn classify(&self) -> VaultPathClass {
        match self.0.split('/').next() {
            Some(component) if component.eq_ignore_ascii_case(".obsidian") => {
                VaultPathClass::ObsidianMetadata
            }
            Some(component) if component.eq_ignore_ascii_case(".trash") => VaultPathClass::Trash,
            _ => VaultPathClass::Content,
        }
    }
}

impl fmt::Display for VaultPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<Path> for VaultPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl TryFrom<&Path> for VaultPath {
    type Error = CoreError;

    fn try_from(value: &Path) -> Result<Self> {
        Self::new(value)
    }
}

fn is_portable_component(component: &str) -> bool {
    if component.len() > MAX_COMPONENT_BYTES
        || component.encode_utf16().count() > MAX_COMPONENT_UTF16_UNITS
        || component.ends_with(['.', ' '])
        || component.chars().any(|character| {
            character.is_control()
                || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\')
        })
    {
        return false;
    }

    let stem = component.split('.').next().unwrap_or(component);
    !is_windows_reserved_name(stem)
}

fn is_windows_reserved_name(stem: &str) -> bool {
    let uppercase = stem.to_uppercase();
    matches!(
        uppercase.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || reserved_numbered_name(&uppercase, "COM")
        || reserved_numbered_name(&uppercase, "LPT")
}

fn reserved_numbered_name(value: &str, prefix: &str) -> bool {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return false;
    };
    matches!(
        suffix,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_unicode_spaces_and_canonical_slashes() {
        let path = VaultPath::from_portable("บันทึก ประจำวัน//你好 world.md").expect("valid path");
        assert_eq!(path.as_str(), "บันทึก ประจำวัน/你好 world.md");
        assert_eq!(path.as_path(), Path::new("บันทึก ประจำวัน/你好 world.md"));
    }

    #[test]
    fn rejects_absolute_parent_and_leading_current_components() {
        for path in [
            "/etc/passwd",
            "../outside.md",
            "notes/../outside.md",
            "./ambiguous.md",
            "",
            ".",
        ] {
            assert!(VaultPath::new(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn normalizes_embedded_current_and_empty_components() {
        let path = VaultPath::new("notes/./drafts//normalized.md/").expect("normalized path");
        assert_eq!(path.as_str(), "notes/drafts/normalized.md");
    }

    #[test]
    fn rejects_non_slash_separators_and_windows_forbidden_characters() {
        for path in [
            r"notes\escape.md",
            "notes/stream:secret.md",
            "notes/a<b.md",
            "notes/a>b.md",
            "notes/a\"b.md",
            "notes/a|b.md",
            "notes/a?b.md",
            "notes/a*b.md",
            "notes/control\u{001f}.md",
        ] {
            assert!(VaultPath::new(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn rejects_windows_reserved_names_with_extensions_and_superscripts() {
        for path in [
            "CON",
            "notes/con.md",
            "PRN.txt",
            "aux.anything.md",
            "NUL.md",
            "COM1.md",
            "com9",
            "LPT1.md",
            "lpt9.txt",
            "COM¹.md",
            "LPT³.md",
            "CONIN$",
            "conout$.md",
            "CLOCK$.txt",
        ] {
            assert!(VaultPath::new(path).is_err(), "accepted {path:?}");
        }
        for path in ["console.md", "com0.md", "com10.md", "lpt0.md", "lpt10.md"] {
            assert!(VaultPath::new(path).is_ok(), "rejected {path:?}");
        }
    }

    #[test]
    fn rejects_trailing_dots_and_spaces_per_component() {
        for path in [
            "note. ",
            "notes./file.md",
            "notes/folder /file.md",
            "note.md.",
        ] {
            assert!(VaultPath::new(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn collision_key_normalizes_case_compatibility_and_unicode_forms() {
        let composed = VaultPath::new("โน้ต/Café/STRASSE.md").expect("composed");
        let decomposed = VaultPath::new("โน้ต/cafe\u{301}/Straße.md").expect("decomposed");
        assert_eq!(composed.collision_key(), decomposed.collision_key());

        let fullwidth = VaultPath::new("ＡＢＣ.md").expect("fullwidth");
        let ascii = VaultPath::new("abc.md").expect("ascii");
        assert_eq!(fullwidth.collision_key(), ascii.collision_key());
    }

    #[test]
    fn collision_key_uses_full_casefold_and_normalizes_fold_output() {
        let precomposed_caron = VaultPath::new("ǰ.md").expect("precomposed caron");
        let decomposed_caron = VaultPath::new("j\u{30c}.md").expect("decomposed caron");
        assert_eq!(
            precomposed_caron.collision_key(),
            decomposed_caron.collision_key()
        );

        let ypogegrammeni = VaultPath::new("α\u{345}.md").expect("ypogegrammeni");
        let iota = VaultPath::new("αι.md").expect("iota");
        assert_eq!(ypogegrammeni.collision_key(), iota.collision_key());

        let cherokee_upper = VaultPath::new("Ꭰ.md").expect("Cherokee uppercase");
        let cherokee_lower = VaultPath::new("ꭰ.md").expect("Cherokee lowercase");
        assert_eq!(
            cherokee_upper.collision_key(),
            cherokee_lower.collision_key()
        );
    }

    #[test]
    fn enforces_component_and_total_portable_limits() {
        let max_component = "a".repeat(MAX_COMPONENT_BYTES);
        assert!(VaultPath::from_portable(&max_component).is_ok());
        assert!(VaultPath::from_portable("a".repeat(MAX_COMPONENT_BYTES + 1)).is_err());
        assert!(VaultPath::from_portable("é".repeat(128)).is_err());

        let maximum = std::iter::repeat_n(max_component.as_str(), 16)
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(maximum.len(), 4_095);
        assert!(VaultPath::from_portable(&maximum).is_ok());

        let too_long = format!("{maximum}/a");
        assert_eq!(too_long.len(), 4_097);
        assert!(VaultPath::from_portable(&too_long).is_err());
    }

    #[test]
    fn classifies_internal_roots_without_matching_similar_names() {
        assert_eq!(
            VaultPath::new(".obsidian/app.json")
                .expect("metadata")
                .classify(),
            VaultPathClass::ObsidianMetadata
        );
        assert_eq!(
            VaultPath::new(".trash/note.md").expect("trash").classify(),
            VaultPathClass::Trash
        );
        assert_eq!(
            VaultPath::new(".TRASH/note.md")
                .expect("trash collision")
                .classify(),
            VaultPathClass::Trash
        );
        assert_eq!(
            VaultPath::new(".Obsidian/app.json")
                .expect("metadata collision")
                .classify(),
            VaultPathClass::ObsidianMetadata
        );
        assert_eq!(
            VaultPath::new(".trashcan/note.md")
                .expect("content")
                .classify(),
            VaultPathClass::Content
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_separators_become_portable_slashes() {
        let path = VaultPath::from_native(Path::new(r"notes\ไทย\file.md")).expect("native");
        assert_eq!(path.as_str(), "notes/ไทย/file.md");
        assert!(VaultPath::from_portable(r"notes\ไทย\file.md").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn native_backslash_filename_is_not_portable() {
        assert!(VaultPath::from_native(Path::new(r"notes\ไทย\file.md")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let invalid = Path::new(OsStr::from_bytes(b"notes/\xff.md"));
        assert!(VaultPath::new(invalid).is_err());
    }
}
