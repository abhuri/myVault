use std::collections::{BTreeMap, HashMap};

use crate::VaultPath;

/// Platform-neutral filesystem event before burst coalescing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawEvent {
    Create(VaultPath),
    Modify(VaultPath),
    Delete(VaultPath),
    Rename { from: VaultPath, to: VaultPath },
}

/// A minimal operation the index/sync layer can consume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NormalizedEvent {
    Upsert(VaultPath),
    Delete(VaultPath),
    Rename { from: VaultPath, to: VaultPath },
}

/// Coalesces a watcher burst deterministically.
///
/// Creates and modifies become one upsert. A delete followed by a create for
/// the same path becomes an upsert, matching common editor atomic-save output.
#[derive(Default)]
pub struct BurstNormalizer {
    by_path: BTreeMap<VaultPath, NormalizedEvent>,
    renames: Vec<NormalizedEvent>,
}

impl BurstNormalizer {
    pub fn push(&mut self, event: RawEvent) {
        match event {
            RawEvent::Create(path) | RawEvent::Modify(path) => {
                self.by_path
                    .insert(path.clone(), NormalizedEvent::Upsert(path));
            }
            RawEvent::Delete(path) => {
                self.by_path
                    .insert(path.clone(), NormalizedEvent::Delete(path));
            }
            RawEvent::Rename { from, to } => {
                self.by_path.remove(&from);
                self.by_path.remove(&to);
                self.renames.push(NormalizedEvent::Rename { from, to });
            }
        }
    }

    #[must_use]
    pub fn finish(mut self) -> Vec<NormalizedEvent> {
        self.renames.extend(self.by_path.into_values());
        self.renames
    }
}

/// Metadata supplied by the writer and later observed by the watcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteFingerprint {
    pub byte_len: u64,
    pub content_tag: u64,
}

#[derive(Clone, Copy, Debug)]
struct Suppression {
    fingerprint: WriteFingerprint,
    expires_at_tick: u64,
}

/// One-shot suppression tokens for watcher echoes of our own writes.
///
/// The caller owns the monotonic logical clock. Exact fingerprint matching
/// prevents an external edit at the same path from being hidden.
#[derive(Default)]
pub struct SelfWriteSuppressor {
    entries: HashMap<VaultPath, Suppression>,
}

impl SelfWriteSuppressor {
    pub fn record(&mut self, path: VaultPath, fingerprint: WriteFingerprint, expires_at_tick: u64) {
        self.entries.insert(
            path,
            Suppression {
                fingerprint,
                expires_at_tick,
            },
        );
    }

    pub fn should_suppress(
        &mut self,
        path: &VaultPath,
        observed: WriteFingerprint,
        now_tick: u64,
    ) -> bool {
        let Some(entry) = self.entries.get(path).copied() else {
            return false;
        };
        if now_tick > entry.expires_at_tick {
            self.entries.remove(path);
            return false;
        }
        if observed == entry.fingerprint {
            self.entries.remove(path);
            return true;
        }
        false
    }

    pub fn prune_expired(&mut self, now_tick: u64) {
        self.entries
            .retain(|_, entry| now_tick <= entry.expires_at_tick);
    }
}
