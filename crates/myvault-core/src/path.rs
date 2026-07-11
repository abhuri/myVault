use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::{CoreError, Result};

/// A validated, non-empty path relative to a vault root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VaultPath(PathBuf);

impl VaultPath {
    /// Validates and normalizes a vault-relative path.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidRelativePath`] for empty, absolute, parent,
    /// root, prefix, or leading-current-directory paths.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path.is_absolute() {
            return Err(CoreError::InvalidRelativePath(path.to_path_buf()));
        }

        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => normalized.push(value),
                Component::CurDir
                | Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_) => {
                    return Err(CoreError::InvalidRelativePath(path.to_path_buf()));
                }
            }
        }

        if normalized.as_os_str().is_empty() {
            return Err(CoreError::InvalidRelativePath(path.to_path_buf()));
        }
        Ok(Self(normalized))
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub fn is_obsidian_metadata(&self) -> bool {
        self.0
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == OsStr::new(".obsidian"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_unicode_and_spaces() {
        let path = VaultPath::new("บันทึก ประจำวัน/你好 world.md").expect("valid path");
        assert_eq!(path.as_path(), Path::new("บันทึก ประจำวัน/你好 world.md"));
    }

    #[test]
    fn rejects_absolute_parent_and_leading_current_components() {
        assert!(VaultPath::new("/etc/passwd").is_err());
        assert!(VaultPath::new("../outside.md").is_err());
        assert!(VaultPath::new("./ambiguous.md").is_err());
        assert!(VaultPath::new("").is_err());
    }

    #[test]
    fn normalizes_embedded_current_components() {
        let path = VaultPath::new("notes/./normalized.md").expect("normalized path");
        assert_eq!(path.as_path(), Path::new("notes/normalized.md"));
    }
}
