use crate::{Error, ErrorCode, Result};
use myvault_sync_engine::{RemoteContentHash, RemoteEntry, RemoteEntryKind, RemoteHashAlgorithm};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::fmt;

pub const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

/// OAuth access token retained exclusively in native memory.
///
/// This type intentionally does not implement `Clone`, `Serialize`, `Display`,
/// or expose its plaintext through its public API.
pub struct AccessToken(SecretString);

impl AccessToken {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(SecretString::from(value.into()))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken([REDACTED])")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct AccountIdentity {
    #[serde(rename = "permissionId")]
    pub permission_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AboutResponse {
    pub user: AccountIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct RemoteFile {
    pub id: String,
    pub name: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    pub trashed: bool,
    pub version: Option<String>,
    #[serde(rename = "md5Checksum")]
    pub md5_checksum: Option<String>,
    #[serde(rename = "sha1Checksum")]
    pub sha1_checksum: Option<String>,
    #[serde(rename = "sha256Checksum")]
    pub sha256_checksum: Option<String>,
}

impl RemoteFile {
    #[must_use]
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME_TYPE
    }

    /// Converts provider metadata after integration has resolved a canonical
    /// path from its durable folder frontier.
    ///
    /// # Errors
    /// Rejects trashed entries, missing/ambiguous parents or revisions, and
    /// values rejected by the sync engine's portable metadata validation.
    pub fn to_remote_entry(&self, canonical_path: impl Into<String>) -> Result<RemoteEntry> {
        if self.trashed || self.parents.len() != 1 {
            return Err(Error::new(ErrorCode::MalformedResponse));
        }
        let version = self
            .version
            .as_ref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?;
        let version = version
            .parse::<u64>()
            .map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
        let kind = if self.is_folder() {
            RemoteEntryKind::Folder
        } else {
            RemoteEntryKind::File
        };
        let content_hash = if kind == RemoteEntryKind::Folder {
            None
        } else if let Some(hash) = &self.sha256_checksum {
            Some(hash_value(RemoteHashAlgorithm::Sha256, hash)?)
        } else if let Some(hash) = &self.sha1_checksum {
            Some(hash_value(RemoteHashAlgorithm::Sha1, hash)?)
        } else if let Some(hash) = &self.md5_checksum {
            Some(hash_value(RemoteHashAlgorithm::Md5, hash)?)
        } else {
            None
        };
        let entry = RemoteEntry {
            file_id: self.id.clone(),
            parent_id: self.parents[0].clone(),
            path: canonical_path.into(),
            kind,
            content_hash,
            // The engine requires a canonical lowercase hexadecimal revision.
            // Drive's monotonically increasing `version` is an int64 string;
            // fixed-width hex preserves exact equality and ordering uniqueness.
            remote_revision: format!("{version:064x}"),
        };
        entry
            .validate()
            .map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
        Ok(entry)
    }
}

fn hash_value(algorithm: RemoteHashAlgorithm, hex: &str) -> Result<RemoteContentHash> {
    RemoteContentHash::new(algorithm, hex.to_owned())
        .map_err(|_| Error::new(ErrorCode::MalformedResponse))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedRoot {
    pub id: String,
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct FilePage {
    #[serde(default)]
    pub files: Vec<RemoteFile>,
    #[serde(rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    #[serde(default, rename = "incompleteSearch")]
    pub incomplete_search: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Change {
    #[serde(rename = "fileId")]
    pub file_id: String,
    #[serde(default)]
    pub removed: bool,
    pub file: Option<RemoteFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ChangesPage {
    #[serde(default)]
    pub changes: Vec<Change>,
    #[serde(rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    #[serde(rename = "newStartPageToken")]
    pub new_start_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StartTokenResponse {
    #[serde(rename = "startPageToken")]
    pub start_page_token: String,
}
