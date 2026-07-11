//! Safety-focused primitives for accessing a local Markdown vault.
//!
//! This crate deliberately contains no UI or platform integration. Callers must
//! keep authentication tokens and other secrets outside the derived index.

mod capability;
mod error;
mod index;
mod path;
mod vault;
mod watcher;

pub use error::{CoreError, Result};
pub use index::{DerivedIndex, NoteRecord, SCHEMA_VERSION, SQLITE_OPEN_RESIDUAL_RISK};
pub use path::VaultPath;
pub use vault::{Vault, WriteIntent};
pub use watcher::{
    BurstNormalizer, NormalizedEvent, RawEvent, SelfWriteSuppressor, WriteFingerprint,
};
