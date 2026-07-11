use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsExt;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

use crate::capability::{open_absolute_dir_nofollow, open_child_dir_nofollow};
use crate::{CoreError, Result, VaultPath};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
pub const DEFAULT_READ_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InventoryLimits {
    pub max_depth: usize,
    pub max_entries: usize,
}

impl Default for InventoryLimits {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_entries: 100_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryKind {
    Markdown,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryEntry {
    pub path: VaultPath,
    pub kind: InventoryKind,
    pub size: u64,
}

/// Identifies whether a write was explicitly initiated by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteIntent {
    Automatic,
    UserInitiated,
}

/// A vault whose filesystem authority is held by an open directory handle.
#[derive(Debug)]
pub struct Vault {
    root_path: PathBuf,
    root_dir: Dir,
}

impl Vault {
    /// Opens an existing vault without following any symlink component.
    ///
    /// # Errors
    ///
    /// Returns an error when the root is not an absolute accessible directory
    /// or any component is a symlink.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let supplied = root.as_ref();
        let root_dir = open_absolute_dir_nofollow(supplied)?;
        let root_path = std::fs::canonicalize(supplied)?;
        Ok(Self {
            root_path,
            root_dir,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root_path
    }

    /// Reads a file relative to the held vault capability without following links.
    ///
    /// # Errors
    ///
    /// Returns an error when a parent or destination is a symlink, is missing,
    /// or cannot be read.
    pub fn read(&self, relative: &VaultPath) -> Result<Vec<u8>> {
        self.read_bounded(relative, DEFAULT_READ_LIMIT)
    }

    /// Reads at most `limit` bytes, rejecting a larger file rather than
    /// allocating in proportion to untrusted filesystem content.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, filesystem failures, or when the
    /// content exceeds `limit`.
    pub fn read_bounded(&self, relative: &VaultPath, limit: usize) -> Result<Vec<u8>> {
        let (parent, name) = self.open_parent(relative)?;
        self.reject_final_symlink(&parent, &name, relative)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = parent.open_with(&name, &options)?;
        let metadata = file.metadata()?;
        if metadata.len() > limit as u64 {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "file size",
                limit,
            });
        }
        let capacity = usize::try_from(metadata.len()).unwrap_or(limit).min(limit);
        let mut bytes = Vec::with_capacity(capacity);
        Read::by_ref(&mut file)
            .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "file size",
                limit,
            });
        }
        Ok(bytes)
    }

    /// Inventories regular vault files without following symbolic links.
    /// Internal `.obsidian` and `.trash` trees are excluded entirely.
    ///
    /// # Errors
    ///
    /// Returns an error for symlinks, invalid portable names, filesystem
    /// failures, or an exceeded traversal limit.
    pub fn inventory(&self, limits: InventoryLimits) -> Result<Vec<InventoryEntry>> {
        let mut output = Vec::new();
        let mut visited = 0_usize;
        self.inventory_dir(&self.root_dir, &[], 0, limits, &mut visited, &mut output)?;
        output.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        Ok(output)
    }

    /// Creates every missing directory component without following links.
    ///
    /// # Errors
    ///
    /// Returns an error for a symlink/non-directory component or filesystem
    /// failure.
    pub fn create_directories(&self, relative: &VaultPath) -> Result<()> {
        let mut current = self.root_dir.try_clone()?;
        let mut display = self.root_path.clone();
        for component in relative.as_path().components() {
            let name = component.as_os_str();
            display.push(name);
            match current.create_dir(name) {
                Ok(()) => sync_directory(&current)?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            current = open_child_dir_nofollow(&current, name, &display)?;
        }
        Ok(())
    }

    /// Crash-safely creates a file and fails if the destination already exists.
    /// The final hard-link publication has create-new/no-replace semantics.
    ///
    /// # Errors
    ///
    /// Returns an error when the destination exists, a path is unsafe, hard
    /// links are unsupported by the filesystem, or another I/O operation fails.
    pub fn create_new(&self, relative: &VaultPath, contents: &[u8]) -> Result<()> {
        let components: Vec<_> = relative.as_path().components().collect();
        if components.len() > 1 {
            let parent_path = components[..components.len() - 1]
                .iter()
                .map(|component| component.as_os_str())
                .collect::<PathBuf>();
            self.create_directories(&VaultPath::new(parent_path)?)?;
        }
        let (parent, destination_name) = self.open_parent(relative)?;
        self.reject_final_symlink(&parent, &destination_name, relative)?;
        let (temp_name, mut file) = Self::create_temp(&parent)?;
        let result = (|| {
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            parent.hard_link(&temp_name, &parent, &destination_name)?;
            sync_directory(&parent)?;
            parent.remove_file(&temp_name)?;
            sync_directory(&parent)
        })();
        if result.is_err() {
            let _ = parent.remove_file(&temp_name);
        }
        result
    }

    /// Atomically replaces a file through an already-open parent capability.
    ///
    /// The destination parent must already exist. The temporary file is
    /// flushed and renamed relative to the same directory handle, so replacing
    /// the path to that directory with a symlink cannot redirect the commit.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem failures, symlink components, or
    /// automatic writes beneath `.obsidian`.
    pub fn atomic_write(
        &self,
        relative: &VaultPath,
        contents: &[u8],
        intent: WriteIntent,
    ) -> Result<()> {
        self.atomic_write_inner(relative, contents, intent, || {})
    }

    fn atomic_write_inner<F>(
        &self,
        relative: &VaultPath,
        contents: &[u8],
        intent: WriteIntent,
        after_parent_open: F,
    ) -> Result<()>
    where
        F: FnOnce(),
    {
        if intent == WriteIntent::Automatic && relative.is_obsidian_metadata() {
            return Err(CoreError::AutomaticObsidianWriteDenied(
                relative.as_path().to_path_buf(),
            ));
        }

        let (parent, destination_name) = self.open_parent(relative)?;
        self.reject_final_symlink(&parent, &destination_name, relative)?;
        after_parent_open();

        let (temp_name, mut file) = Self::create_temp(&parent)?;
        let result = (|| {
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);

            parent.rename(&temp_name, &parent, &destination_name)?;
            sync_directory(&parent)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = parent.remove_file(&temp_name);
        }
        result
    }

    fn open_parent(&self, relative: &VaultPath) -> Result<(Dir, OsString)> {
        let components: Vec<_> = relative.as_path().components().collect();
        let (name, parents) = components
            .split_last()
            .ok_or_else(|| CoreError::InvalidRelativePath(relative.as_path().to_path_buf()))?;
        let mut current = self.root_dir.try_clone()?;
        let mut display = self.root_path.clone();
        for component in parents {
            display.push(component.as_os_str());
            current = open_child_dir_nofollow(&current, component.as_os_str(), &display)?;
        }
        Ok((current, name.as_os_str().to_owned()))
    }

    fn inventory_dir(
        &self,
        directory: &Dir,
        prefix: &[String],
        depth: usize,
        limits: InventoryLimits,
        visited: &mut usize,
        output: &mut Vec<InventoryEntry>,
    ) -> Result<()> {
        let mut entries = directory
            .read_dir(".")?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_unstable_by_key(cap_std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let Some(name_utf8) = name.to_str() else {
                return Err(CoreError::InvalidRelativePath(self.root_path.join(&name)));
            };
            if prefix.is_empty()
                && (name_utf8.eq_ignore_ascii_case(".obsidian")
                    || name_utf8.eq_ignore_ascii_case(".trash"))
            {
                continue;
            }
            *visited = visited.saturating_add(1);
            if *visited > limits.max_entries {
                return Err(CoreError::ResourceLimitExceeded {
                    resource: "inventory entries",
                    limit: limits.max_entries,
                });
            }
            let mut components = prefix.to_owned();
            components.push(name_utf8.to_owned());
            let path = VaultPath::from_portable(components.join("/"))?;
            let metadata = directory.symlink_metadata(&name)?;
            if metadata.file_type().is_symlink() {
                return Err(CoreError::SymlinkRejected(
                    self.root_path.join(path.as_path()),
                ));
            }
            if metadata.is_dir() {
                let next_depth = depth.saturating_add(1);
                if next_depth > limits.max_depth {
                    return Err(CoreError::ResourceLimitExceeded {
                        resource: "inventory depth",
                        limit: limits.max_depth,
                    });
                }
                let display = self.root_path.join(path.as_path());
                let child = open_child_dir_nofollow(directory, &name, &display)?;
                self.inventory_dir(&child, &components, next_depth, limits, visited, output)?;
            } else if metadata.is_file() {
                let markdown = Path::new(name_utf8)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("md")
                            || extension.eq_ignore_ascii_case("markdown")
                    });
                output.push(InventoryEntry {
                    path,
                    kind: if markdown {
                        InventoryKind::Markdown
                    } else {
                        InventoryKind::File
                    },
                    size: metadata.len(),
                });
            }
        }
        Ok(())
    }

    fn reject_final_symlink(
        &self,
        parent: &Dir,
        name: &OsString,
        relative: &VaultPath,
    ) -> Result<()> {
        if parent
            .symlink_metadata(name)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(CoreError::SymlinkRejected(
                self.root_path.join(relative.as_path()),
            ));
        }
        Ok(())
    }

    fn create_temp(parent: &Dir) -> Result<(OsString, cap_std::fs::File)> {
        for _ in 0..64 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let candidate =
                OsString::from(format!(".myvault-write-{}-{id}.tmp", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            options.follow(FollowSymlinks::No);
            match parent.open_with(&candidate, &options) {
                Ok(file) => return Ok((candidate, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "unable to allocate atomic-write temporary name",
        )
        .into())
    }
}

fn sync_directory(directory: &Dir) -> Result<()> {
    let file = directory.try_clone()?.into_std_file();
    match file.sync_all() {
        Ok(()) => Ok(()),
        // Some Windows/filesystem combinations do not support flushing a
        // directory handle. The file itself was flushed before atomic rename.
        #[cfg(windows)]
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn parent_symlink_swap_cannot_redirect_atomic_commit() {
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let base = temp_root.join(format!(
            "myvault-adversarial-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let vault_path = base.join("vault");
        let outside = base.join("outside");
        fs::create_dir_all(vault_path.join("notes")).expect("vault fixture");
        fs::create_dir_all(&outside).expect("outside fixture");
        let vault = Vault::open(&vault_path).expect("open vault");
        let relative = VaultPath::new("notes/attack.md").expect("path");

        vault
            .atomic_write_inner(&relative, b"safe", WriteIntent::Automatic, || {
                fs::rename(vault_path.join("notes"), vault_path.join("original-notes"))
                    .expect("swap original parent");
                symlink(&outside, vault_path.join("notes")).expect("install attack symlink");
            })
            .expect("descriptor-relative commit");

        assert_eq!(
            fs::read(vault_path.join("original-notes/attack.md")).expect("safe destination"),
            b"safe"
        );
        assert!(!outside.join("attack.md").exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }
}
