#![forbid(unsafe_code)]

//! GET-only Google Drive metadata adapter.
//!
//! In addition to narrow provider operations, [`ReadOnlyDrive`] implements
//! [`myvault_sync_engine::DriveClient`]. The engine owns the durable recursive
//! folder frontier; this adapter fetches one direct-child page and derives paths
//! from the supplied durable folder path. Incremental upserts that cannot be
//! mapped to a canonical path without store context fail closed with
//! `cursor_ambiguous`, causing a fresh metadata scan instead of stale paths.
//!
//! The production constructor pins requests to Google's Drive v3 origin,
//! refuses redirects, bounds response bodies, and accepts only an opaque bearer
//! token that cannot be serialized or printed.

mod client;
mod error;
mod model;

pub use client::ReadOnlyDrive;
pub use error::{Error, ErrorCode, Result};
pub use model::{
    AccessToken, AccountIdentity, Change, ChangesPage, FilePage, RemoteFile, VerifiedRoot,
    FOLDER_MIME_TYPE,
};
