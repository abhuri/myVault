#![forbid(unsafe_code)]

//! Guarded Google Drive provider capabilities.
//!
//! [`ReadOnlyDrive`] remains a GET-only metadata adapter and implements
//! [`myvault_sync_engine::DriveClient`]. The engine owns the durable recursive
//! folder frontier; this adapter fetches one direct-child page and derives paths
//! from the supplied durable folder path. Incremental upserts that cannot be
//! mapped to a canonical path without store context fail closed with
//! `cursor_ambiguous`, causing a fresh metadata scan instead of stale paths.
//!
//! The production constructor pins requests to Google's Drive v3 origin,
//! refuses redirects, bounds response bodies, and accepts only an opaque bearer
//! token that cannot be serialized or printed. [`TransferDrive`] is a separate
//! create-only/blob-download capability: callers cannot turn the read-only
//! adapter into a generic mutation client or update existing content.

mod client;
mod error;
mod model;
mod transfer;

pub use client::{ReadOnlyDrive, ResolvedDriveChange};
pub use error::{Error, ErrorCode, Result};
pub use model::{
    AccessToken, AccountIdentity, Change, ChangesPage, FilePage, RemoteFile, VerifiedRoot,
    FOLDER_MIME_TYPE,
};
pub use transfer::{
    plan_resumable_upload_chunk, CreateIntent, CreatePermit, CreateReconciliation, DownloadIntent,
    ReconcileReason, RemoteObject, TransferDrive, UploadChunkPlan, UploadProgress, UploadSession,
    VerifiedDownload, RESUMABLE_UPLOAD_CHUNK_BYTES,
};
