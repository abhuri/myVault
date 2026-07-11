//! Config-gated Google Drive REST fixture harness.
//!
//! This adapter is intentionally small: it proves the Phase 0 request ordering
//! and safety boundaries without becoming the production sync engine.

use crate::{
    resolve_unknown_upload, verify_fixture_cleanup, InitialSync, RemoteCandidate, SyncError,
    UnknownUploadResolution,
};
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, time::Duration};

const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
const FIXTURE_MARKER_PROPERTY: &str = "myVaultFixtureMarker";

pub const REQUIRED_FIXTURES: &[(&str, &str, &[u8])] = &[
    (
        "thai-markdown.md",
        "text/markdown; charset=utf-8",
        "# สวัสดี myVault\n\nภาษาไทย **ตัวหนา** และ [[ลิงก์]]\n".as_bytes(),
    ),
    (
        "rendering.md",
        "text/markdown; charset=utf-8",
        "```mermaid\ngraph TD\n  A --> B\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n".as_bytes(),
    ),
    (
        "attachment.txt",
        "text/plain; charset=utf-8",
        b"myVault Phase 0 attachment\n",
    ),
];

pub const RESUMABLE_THRESHOLD: usize = 5 * 1024 * 1024;
const RESUMABLE_CHUNK_SIZE: usize = 5 * 1024 * 1024;

/// Exact logical paths exercised by the live acceptance fixture builder.
pub const ACCEPTANCE_PATHS: &[&str] = &[
    "hello.md",
    "thai-สวัสดี.md",
    "duplicate.md",
    "duplicate.md",
    "Level one spaces/duplicate.md",
    "empty file.md",
    "small-binary.bin",
    "Level one spaces/ระดับ-สอง/level's-three/spaces Unicode's.md",
    ".obsidian/ignored.json",
    "large-attachment.bin",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedUpload {
    Uploaded(DriveFile),
    /// The upload response was lost or retryable. Drive was queried before any
    /// retry, so callers can safely follow the returned reconciliation result.
    Reconciled(UnknownUploadResolution),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceFixtureSet {
    pub files: Vec<DriveFile>,
    pub large_attachment: VerifiedUpload,
}

/// OAuth bearer token. The secret is never printable through `Debug`.
pub struct BearerToken(SecretString);

impl BearerToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(SecretString::from(token.into()))
    }

    fn authorization_value(&self) -> String {
        format!("Bearer {}", self.0.expose_secret())
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([REDACTED])")
    }
}

#[derive(Debug, Clone)]
pub struct DriveHarnessConfig {
    /// Must be explicitly enabled. Live tests should additionally require an env flag.
    enabled: bool,
    api_base: String,
    upload_base: String,
    fixture_name: String,
    allow_test_localhost: bool,
}

impl DriveHarnessConfig {
    pub fn google(fixture_name: impl Into<String>) -> Self {
        Self {
            enabled: false,
            api_base: "https://www.googleapis.com/drive/v3".into(),
            upload_base: "https://www.googleapis.com/upload/drive/v3".into(),
            fixture_name: fixture_name.into(),
            allow_test_localhost: false,
        }
    }

    pub fn enable_live(mut self) -> Self {
        self.enabled = true;
        self
    }

    #[cfg(test)]
    fn for_test(base: String, fixture_name: impl Into<String>) -> Self {
        Self {
            enabled: true,
            api_base: base.clone(),
            upload_base: base,
            fixture_name: fixture_name.into(),
            allow_test_localhost: true,
        }
    }

    fn require_enabled(&self) -> Result<(), SyncError> {
        if self.enabled {
            Ok(())
        } else {
            Err(SyncError::HarnessDisabled)
        }
    }
}

#[derive(Clone)]
struct FixtureIdentity {
    id: String,
    name: String,
    marker: String,
}

pub struct DriveHarness {
    client: Client,
    token: BearerToken,
    config: DriveHarnessConfig,
    fixture: Option<FixtureIdentity>,
    pending_marker: String,
}

impl fmt::Debug for DriveHarness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DriveHarness")
            .field("config", &self.config)
            .field("token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default, rename = "appProperties")]
    pub app_properties: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StartToken {
    #[serde(rename = "startPageToken")]
    start_page_token: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Change {
    #[serde(rename = "fileId")]
    pub file_id: String,
    #[serde(default)]
    pub removed: bool,
}

#[derive(Debug, Deserialize)]
struct ChangeList {
    #[serde(default)]
    changes: Vec<Change>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "newStartPageToken")]
    new_start_page_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeDrain {
    pub changes: Vec<Change>,
    pub durable_cursor: String,
}

impl DriveHarness {
    pub fn new(config: DriveHarnessConfig, token: BearerToken) -> Result<Self, SyncError> {
        config.require_enabled()?;
        validate_origins(&config)?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| SyncError::Http(error.to_string()))?;
        Ok(Self {
            client,
            token,
            config,
            fixture: None,
            pending_marker: random_marker()?,
        })
    }

    #[cfg(test)]
    fn attach_test_fixture(&mut self, id: &str, marker: &str) {
        self.fixture = Some(FixtureIdentity {
            id: id.into(),
            name: self.config.fixture_name.clone(),
            marker: marker.into(),
        });
    }

    fn fixture(&self) -> Result<&FixtureIdentity, SyncError> {
        let fixture = self
            .fixture
            .as_ref()
            .ok_or(SyncError::FixtureNotAllowlisted)?;
        verify_fixture_cleanup(&fixture.id, &fixture.name)?;
        Ok(fixture)
    }

    fn authorized(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        request.header(AUTHORIZATION, self.token.authorization_value())
    }

    fn decode<T: for<'de> Deserialize<'de>>(response: Response) -> Result<T, SyncError> {
        let status = response.status();
        if !status.is_success() {
            return Err(SyncError::HttpStatus {
                status: status.as_u16(),
            });
        }
        response
            .json()
            .map_err(|error| SyncError::InvalidResponse(error.to_string()))
    }

    fn send(&self, request: reqwest::blocking::RequestBuilder) -> Result<Response, SyncError> {
        request
            .send()
            .map_err(|error| SyncError::Http(error.to_string()))
    }

    pub fn create_fixture_folder(&mut self) -> Result<DriveFile, SyncError> {
        // The name must pass the same allowlist before anything is uploaded into it.
        verify_fixture_cleanup("pending", &self.config.fixture_name)?;
        let url = format!("{}/files", self.config.api_base);
        let response = self.send(
            self.authorized(self.client.post(url))
                .query(&[("fields", "id,name,mimeType,appProperties")])
                .json(&serde_json::json!({
                    "name": self.config.fixture_name,
                    "mimeType": FOLDER_MIME_TYPE,
                    "appProperties": { (FIXTURE_MARKER_PROPERTY): self.pending_marker }
                })),
        )?;
        let file: DriveFile = Self::decode(response)?;
        verify_fixture_cleanup(&file.id, &file.name)?;
        if file.name != self.config.fixture_name
            || file.mime_type.as_deref() != Some(FOLDER_MIME_TYPE)
            || file.app_properties.get(FIXTURE_MARKER_PROPERTY) != Some(&self.pending_marker)
        {
            return Err(SyncError::FixtureNotAllowlisted);
        }
        self.fixture = Some(FixtureIdentity {
            id: file.id.clone(),
            name: file.name.clone(),
            marker: self.pending_marker.clone(),
        });
        Ok(file)
    }

    pub fn upload_required_fixtures(&self) -> Result<Vec<DriveFile>, SyncError> {
        REQUIRED_FIXTURES
            .iter()
            .map(|(name, mime, body)| self.upload_multipart(name, mime, body))
            .collect()
    }

    /// Builds the complete Phase 0 fixture tree below the allowlisted root.
    /// Returned child folder ids come directly from Drive and are validated
    /// before they can be used as upload parents.
    pub fn create_acceptance_fixture_set(&self) -> Result<AcceptanceFixtureSet, SyncError> {
        let root = self.fixture()?.id.as_str();
        let level_one = self.create_child_folder(root, "Level one spaces")?;
        let level_two = self.create_child_folder(&level_one.id, "ระดับ-สอง")?;
        let level_three = self.create_child_folder(&level_two.id, "level's-three")?;
        let obsidian = self.create_child_folder(root, ".obsidian")?;

        let mut files = vec![
            self.upload_multipart_at(root, "hello.md", "text/markdown", b"# Hello\n")?,
            self.upload_multipart_at(
                root,
                "thai-สวัสดี.md",
                "text/markdown; charset=utf-8",
                "# สวัสดี\n\nภาษาไทยและ [[hello]]\n".as_bytes(),
            )?,
            // Drive permits duplicate names; both must remain distinct by id.
            self.upload_multipart_at(root, "duplicate.md", "text/markdown", b"duplicate A\n")?,
            self.upload_multipart_at(root, "duplicate.md", "text/markdown", b"duplicate B\n")?,
            self.upload_multipart_at(
                &level_one.id,
                "duplicate.md",
                "text/markdown",
                b"nested duplicate\n",
            )?,
            self.upload_multipart_at(root, "empty file.md", "text/markdown", b"")?,
            self.upload_multipart_at(
                root,
                "small-binary.bin",
                "application/octet-stream",
                &[0, 1, 2, 0xff, 0x7f],
            )?,
            self.upload_multipart_at(
                &level_three.id,
                "spaces Unicode's.md",
                "text/markdown; charset=utf-8",
                "# ชั้นที่สาม\n".as_bytes(),
            )?,
            self.upload_multipart_at(
                &obsidian.id,
                "ignored.json",
                "application/json",
                br#"{"preserve":true,"purpose":"ignored-file probe"}"#,
            )?,
        ];
        let large_attachment = self.upload_large_fixture()?;
        // Keep the folder metadata out of `files`: this collection represents
        // byte-bearing verification targets only.
        files.shrink_to_fit();
        Ok(AcceptanceFixtureSet {
            files,
            large_attachment,
        })
    }

    fn create_child_folder(&self, parent_id: &str, name: &str) -> Result<DriveFile, SyncError> {
        require_safe_drive_id(parent_id)?;
        let url = format!("{}/files", self.config.api_base);
        let response = self.send(
            self.authorized(self.client.post(url))
                .query(&[("fields", "id,name")])
                .json(&serde_json::json!({
                    "name": name,
                    "parents": [parent_id],
                    "mimeType": "application/vnd.google-apps.folder"
                })),
        )?;
        let folder: DriveFile = Self::decode(response)?;
        require_safe_drive_id(&folder.id)?;
        Ok(folder)
    }

    /// Creates the required >5 MiB acceptance fixture using Drive's resumable
    /// protocol. The generated content is deterministic so its hash is stable.
    pub fn upload_large_fixture(&self) -> Result<VerifiedUpload, SyncError> {
        let bytes = vec![b'M'; RESUMABLE_THRESHOLD + 1];
        self.upload_resumable_with_verification(
            "large-attachment.bin",
            "application/octet-stream",
            &bytes,
        )
    }

    /// Minimal upload implementation for Phase 0. Files are deliberately tiny;
    /// production-sized attachments will use Drive's resumable protocol.
    pub fn upload_multipart(
        &self,
        name: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<DriveFile, SyncError> {
        let parent = self.fixture()?.id.as_str();
        self.upload_multipart_at(parent, name, mime_type, bytes)
    }

    fn upload_multipart_at(
        &self,
        parent: &str,
        name: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<DriveFile, SyncError> {
        require_safe_drive_id(parent)?;
        let hash = sha256(bytes);
        let metadata = serde_json::json!({
            "name": name,
            "parents": [parent],
            "appProperties": { "myVaultSha256": hash }
        });
        let boundary = "myvault-phase0-boundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{}\r\n--{boundary}\r\nContent-Type: {mime_type}\r\n\r\n",
            metadata
        )
        .into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let url = format!("{}/files", self.config.upload_base);
        let response = self.send(
            self.authorized(self.client.post(url))
                .query(&[
                    ("uploadType", "multipart"),
                    ("fields", "id,name,appProperties"),
                ])
                .header(
                    CONTENT_TYPE,
                    format!("multipart/related; boundary={boundary}"),
                )
                .body(body),
        )?;
        Self::decode(response)
    }

    /// Initiates a resumable session and uploads in 5 MiB chunks. Google Drive
    /// requires non-final chunks to be multiples of 256 KiB; 5 MiB satisfies it.
    pub fn upload_resumable(
        &self,
        name: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<DriveFile, SyncError> {
        if bytes.len() <= RESUMABLE_THRESHOLD {
            return Err(SyncError::InvalidResponse(format!(
                "resumable fixture must exceed {RESUMABLE_THRESHOLD} bytes"
            )));
        }
        let parent = self.fixture()?.id.as_str();
        let hash = sha256(bytes);
        let metadata = serde_json::json!({
            "name": name,
            "parents": [parent],
            "appProperties": { "myVaultSha256": hash }
        });
        let url = format!("{}/files", self.config.upload_base);
        let initiation = self.send(
            self.authorized(self.client.post(url))
                .query(&[
                    ("uploadType", "resumable"),
                    ("fields", "id,name,appProperties"),
                ])
                .header("X-Upload-Content-Type", mime_type)
                .header("X-Upload-Content-Length", bytes.len())
                .json(&metadata),
        )?;
        let initiation_status = initiation.status();
        if !initiation_status.is_success() {
            return Err(SyncError::HttpStatus {
                status: initiation_status.as_u16(),
            });
        }
        let session_url = initiation
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                SyncError::InvalidResponse("resumable initiation omitted Location".into())
            })?
            .to_owned();
        validate_resumable_location(&self.config.upload_base, &session_url)?;

        let total = bytes.len();
        let mut offset = 0;
        while offset < total {
            let end_exclusive = (offset + RESUMABLE_CHUNK_SIZE).min(total);
            let end_inclusive = end_exclusive - 1;
            let response = self.send(
                self.authorized(self.client.put(&session_url))
                    .header(CONTENT_TYPE, mime_type)
                    .header(CONTENT_LENGTH, end_exclusive - offset)
                    .header(
                        CONTENT_RANGE,
                        format!("bytes {offset}-{end_inclusive}/{total}"),
                    )
                    .body(bytes[offset..end_exclusive].to_vec()),
            )?;

            if response.status().as_u16() == 308 {
                offset = parse_resume_offset(&response, offset, end_exclusive, total)?;
                continue;
            }
            return Self::decode(response);
        }
        Err(SyncError::InvalidResponse(
            "resumable session completed without file metadata".into(),
        ))
    }

    /// A transport failure or retryable final response is an unknown outcome:
    /// query Drive by parent/name/hash before deciding whether to retry.
    pub fn upload_resumable_with_verification(
        &self,
        name: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<VerifiedUpload, SyncError> {
        match self.upload_resumable(name, mime_type, bytes) {
            Ok(file) => Ok(VerifiedUpload::Uploaded(file)),
            Err(error) if is_unknown_upload_error(&error) => Ok(VerifiedUpload::Reconciled(
                self.resolve_unknown_upload(name, &sha256(bytes))?,
            )),
            Err(error) => Err(error),
        }
    }

    pub fn list_all(&self) -> Result<Vec<DriveFile>, SyncError> {
        let parent = self.fixture()?.id.as_str();
        let mut page_token: Option<String> = None;
        let mut files = Vec::new();
        loop {
            let url = format!("{}/files", self.config.api_base);
            let query = format!("'{parent}' in parents and trashed = false");
            let mut request = self.authorized(self.client.get(url)).query(&[
                ("q", query.as_str()),
                ("fields", "nextPageToken,files(id,name,appProperties)"),
                ("pageSize", "1000"),
            ]);
            if let Some(token) = page_token.as_deref() {
                request = request.query(&[("pageToken", token)]);
            }
            let page: FileList = Self::decode(self.send(request)?)?;
            files.extend(page.files);
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => return Ok(files),
            }
        }
    }

    pub fn get_start_page_token(&self) -> Result<String, SyncError> {
        let url = format!("{}/changes/startPageToken", self.config.api_base);
        let value: StartToken = Self::decode(self.send(self.authorized(self.client.get(url)))?)?;
        Ok(value.start_page_token)
    }

    /// Enforces initial-sync ordering: capture token, scan, then drain changes.
    pub fn initial_scan_and_drain(&self) -> Result<(Vec<DriveFile>, ChangeDrain), SyncError> {
        let mut order = InitialSync::default();
        let token = self.get_start_page_token()?;
        order.capture_start_token(&token);
        let files = self.list_all()?;
        order.finish_scan()?;
        let drain = self.drain_changes(&token)?;
        order.finish_drain()?;
        Ok((files, drain))
    }

    pub fn drain_changes(&self, start_token: &str) -> Result<ChangeDrain, SyncError> {
        let mut token = start_token.to_owned();
        let mut changes = Vec::new();
        loop {
            let url = format!("{}/changes", self.config.api_base);
            let response = self.send(self.authorized(self.client.get(url)).query(&[
                ("pageToken", token.as_str()),
                (
                    "fields",
                    "nextPageToken,newStartPageToken,changes(fileId,removed)",
                ),
            ]))?;
            let page: ChangeList = Self::decode(response)?;
            changes.extend(page.changes);
            if let Some(next) = page.next_page_token {
                token = next;
                continue;
            }
            let durable_cursor = page.new_start_page_token.ok_or_else(|| {
                SyncError::InvalidResponse("final changes page omitted newStartPageToken".into())
            })?;
            return Ok(ChangeDrain {
                changes,
                durable_cursor,
            });
        }
    }

    pub fn download_and_verify(
        &self,
        file_id: &str,
        expected_hash: &str,
    ) -> Result<Vec<u8>, SyncError> {
        let url = format!("{}/files/{file_id}", self.config.api_base);
        let response = self.send(
            self.authorized(self.client.get(url))
                .query(&[("alt", "media")]),
        )?;
        let status = response.status();
        if !status.is_success() {
            return Err(SyncError::HttpStatus {
                status: status.as_u16(),
            });
        }
        let bytes = response
            .bytes()
            .map_err(|error| SyncError::Http(error.to_string()))?
            .to_vec();
        let actual = sha256(&bytes);
        if actual != expected_hash {
            return Err(SyncError::HashMismatch {
                expected: expected_hash.into(),
                actual,
            });
        }
        Ok(bytes)
    }

    pub fn resolve_unknown_upload(
        &self,
        name: &str,
        expected_hash: &str,
    ) -> Result<UnknownUploadResolution, SyncError> {
        let parent = self.fixture()?.id.as_str();
        let candidates = self
            .list_all()?
            .into_iter()
            .filter(|file| file.name == name)
            .map(|file| RemoteCandidate {
                file_id: file.id,
                parent_id: parent.into(),
                name: file.name,
                content_hash: file
                    .app_properties
                    .get("myVaultSha256")
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        Ok(resolve_unknown_upload(
            parent,
            name,
            expected_hash,
            &candidates,
        ))
    }

    /// Drive DELETE is intentionally absent. Cleanup can only mark the exact
    /// allowlisted fixture folder as trashed.
    pub fn trash_fixture_folder(&self) -> Result<DriveFile, SyncError> {
        let fixture = self.fixture()?;
        let url = format!("{}/files/{}", self.config.api_base, fixture.id);
        let remote: DriveFile = Self::decode(
            self.send(
                self.authorized(self.client.get(&url))
                    .query(&[("fields", "id,name,mimeType,appProperties")]),
            )?,
        )?;
        if remote.id != fixture.id
            || remote.name != fixture.name
            || remote.mime_type.as_deref() != Some(FOLDER_MIME_TYPE)
            || remote.app_properties.get(FIXTURE_MARKER_PROPERTY) != Some(&fixture.marker)
        {
            return Err(SyncError::FixtureNotAllowlisted);
        }
        let response = self.send(
            self.authorized(self.client.patch(url))
                .query(&[("fields", "id,name,mimeType,appProperties")])
                .json(&serde_json::json!({ "trashed": true })),
        )?;
        let trashed: DriveFile = Self::decode(response)?;
        if trashed.id != fixture.id || trashed.name != fixture.name {
            return Err(SyncError::FixtureNotAllowlisted);
        }
        Ok(trashed)
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn parse_resume_offset(
    response: &Response,
    previous_offset: usize,
    sent_end_exclusive: usize,
    total: usize,
) -> Result<usize, SyncError> {
    let Some(range) = response.headers().get(RANGE) else {
        // Drive may omit Range when it persisted zero bytes. A 308 following a
        // nonzero chunk without Range cannot be advanced safely.
        return Err(SyncError::InvalidResponse(format!(
            "308 response omitted Range after sending through byte {}",
            sent_end_exclusive - 1
        )));
    };
    let range = range
        .to_str()
        .map_err(|error| SyncError::InvalidResponse(error.to_string()))?;
    let last = range
        .strip_prefix("bytes=0-")
        .ok_or_else(|| SyncError::InvalidResponse(format!("invalid resumable Range: {range}")))?
        .parse::<usize>()
        .map_err(|error| SyncError::InvalidResponse(error.to_string()))?;
    let next = last
        .checked_add(1)
        .ok_or_else(|| SyncError::InvalidResponse("resumable Range overflow".into()))?;
    if next <= previous_offset || next > sent_end_exclusive || next > total {
        return Err(SyncError::InvalidResponse(
            "resumable Range was non-advancing or out of bounds".into(),
        ));
    }
    Ok(next)
}

fn is_unknown_upload_error(error: &SyncError) -> bool {
    matches!(
        error,
        SyncError::Http(_)
            | SyncError::HttpStatus {
                status: 408 | 429 | 500..=599,
            }
    )
}

fn require_safe_drive_id(id: &str) -> Result<(), SyncError> {
    let valid = !id.trim().is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(SyncError::FixtureNotAllowlisted)
    }
}

fn random_marker() -> Result<String, SyncError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| SyncError::InvalidResponse("secure random marker generation failed".into()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn validate_origins(config: &DriveHarnessConfig) -> Result<(), SyncError> {
    let api = reqwest::Url::parse(&config.api_base)
        .map_err(|_| SyncError::InvalidResponse("invalid Drive API origin".into()))?;
    let upload = reqwest::Url::parse(&config.upload_base)
        .map_err(|_| SyncError::InvalidResponse("invalid Drive upload origin".into()))?;
    if api.username() != ""
        || api.password().is_some()
        || upload.username() != ""
        || upload.password().is_some()
    {
        return Err(SyncError::InvalidResponse(
            "credentials are forbidden in Drive origins".into(),
        ));
    }
    if config.allow_test_localhost {
        let local = |url: &reqwest::Url| {
            matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                && matches!(url.scheme(), "http" | "https")
        };
        if local(&api) && local(&upload) && same_origin(&api, &upload) {
            return Ok(());
        }
    } else if api.scheme() == "https"
        && upload.scheme() == "https"
        && api.host_str() == Some("www.googleapis.com")
        && upload.host_str() == Some("www.googleapis.com")
        && api.port_or_known_default() == Some(443)
        && upload.port_or_known_default() == Some(443)
    {
        return Ok(());
    }
    Err(SyncError::InvalidResponse(
        "Drive origins are not allowlisted".into(),
    ))
}

fn validate_resumable_location(upload_base: &str, session_url: &str) -> Result<(), SyncError> {
    let expected = reqwest::Url::parse(upload_base)
        .map_err(|_| SyncError::InvalidResponse("invalid Drive upload origin".into()))?;
    let actual = reqwest::Url::parse(session_url)
        .map_err(|_| SyncError::InvalidResponse("invalid resumable Location".into()))?;
    if actual.username() != "" || actual.password().is_some() || !same_origin(&expected, &actual) {
        return Err(SyncError::InvalidResponse(
            "resumable Location changed origin".into(),
        ));
    }
    Ok(())
}

fn same_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

#[derive(Serialize)]
#[allow(dead_code)]
struct CompileTimeSerializationGuard<'a> {
    name: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    fn harness(server: &Server) -> DriveHarness {
        let mut harness = DriveHarness::new(
            DriveHarnessConfig::for_test(server.url(), "myVault-spike-20260711-a1b2c3"),
            BearerToken::new("top-secret"),
        )
        .unwrap();
        harness.attach_test_fixture("fixture_123", "test-marker");
        harness
    }

    #[test]
    fn disabled_by_default_and_debug_redacts_token() {
        let config = DriveHarnessConfig::google("myVault-spike-20260711-a1b2c3");
        assert!(matches!(
            DriveHarness::new(config, BearerToken::new("never-print-me")),
            Err(SyncError::HarnessDisabled)
        ));
        assert_eq!(
            format!("{:?}", BearerToken::new("never-print-me")),
            "BearerToken([REDACTED])"
        );
    }

    #[test]
    fn production_constructor_rejects_non_google_origins() {
        let config = DriveHarnessConfig {
            enabled: true,
            api_base: "https://evil.example/drive/v3".into(),
            upload_base: "https://evil.example/upload/drive/v3".into(),
            fixture_name: "myVault-spike-20260711-a1b2c3".into(),
            allow_test_localhost: false,
        };
        assert!(matches!(
            DriveHarness::new(config, BearerToken::new("secret")),
            Err(SyncError::InvalidResponse(_))
        ));
    }

    #[test]
    fn independent_harnesses_use_distinct_random_cleanup_markers() {
        let server = Server::new();
        let first = harness(&server);
        let second = DriveHarness::new(
            DriveHarnessConfig::for_test(server.url(), "myVault-spike-20260711-a1b2c3"),
            BearerToken::new("secret"),
        )
        .unwrap();
        assert_ne!(first.pending_marker, second.pending_marker);
        assert_eq!(first.pending_marker.len(), 64);
    }

    #[test]
    fn fixture_creation_sets_and_verifies_random_marker_and_folder_metadata() {
        let mut server = Server::new();
        let mut harness = DriveHarness::new(
            DriveHarnessConfig::for_test(server.url(), "myVault-spike-20260711-a1b2c3"),
            BearerToken::new("secret"),
        )
        .unwrap();
        harness.pending_marker = "creation-marker".into();
        let create = server
            .mock("POST", "/files")
            .match_query(Matcher::Any)
            .match_body(Matcher::Regex(
                "myVaultFixtureMarker.*creation-marker".into(),
            ))
            .with_body(format!(
                r#"{{"id":"created_123","name":"myVault-spike-20260711-a1b2c3","mimeType":"{FOLDER_MIME_TYPE}","appProperties":{{"{FIXTURE_MARKER_PROPERTY}":"creation-marker"}}}}"#
            ))
            .create();
        let folder = harness.create_fixture_folder().unwrap();
        assert_eq!(folder.id, "created_123");
        assert_eq!(harness.fixture().unwrap().marker, "creation-marker");
        create.assert();
    }

    #[test]
    fn fixture_creation_rejects_server_response_with_wrong_mime_or_marker() {
        let mut server = Server::new();
        let mut harness = DriveHarness::new(
            DriveHarnessConfig::for_test(server.url(), "myVault-spike-20260711-a1b2c3"),
            BearerToken::new("secret"),
        )
        .unwrap();
        harness.pending_marker = "expected-marker".into();
        let create = server
            .mock("POST", "/files")
            .match_query(Matcher::Any)
            .with_body(format!(
                r#"{{"id":"created_123","name":"myVault-spike-20260711-a1b2c3","mimeType":"text/plain","appProperties":{{"{FIXTURE_MARKER_PROPERTY}":"attacker-marker"}}}}"#
            ))
            .create();
        assert_eq!(
            harness.create_fixture_folder(),
            Err(SyncError::FixtureNotAllowlisted)
        );
        assert!(harness.fixture.is_none());
        create.assert();
    }

    #[test]
    fn response_bodies_are_never_embedded_in_http_errors() {
        let mut server = Server::new();
        let failed = server
            .mock("GET", "/changes/startPageToken")
            .with_status(500)
            .with_body("private note content must not reach logs")
            .create();
        let error = harness(&server).get_start_page_token().unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("private note content"));
        assert!(rendered.contains("500"));
        failed.assert();
    }

    #[test]
    fn acceptance_manifest_covers_path_and_content_edge_cases() {
        assert!(ACCEPTANCE_PATHS.contains(&"hello.md"));
        assert!(ACCEPTANCE_PATHS.contains(&"thai-สวัสดี.md"));
        assert!(ACCEPTANCE_PATHS.contains(&"empty file.md"));
        assert!(ACCEPTANCE_PATHS.contains(&"small-binary.bin"));
        assert!(ACCEPTANCE_PATHS.contains(&".obsidian/ignored.json"));
        assert!(ACCEPTANCE_PATHS.contains(&"large-attachment.bin"));
        assert!(ACCEPTANCE_PATHS
            .iter()
            .any(|path| path.matches('/').count() >= 3));
        assert!(ACCEPTANCE_PATHS
            .iter()
            .any(|path| path.contains(' ') && path.contains('\'')));
        assert_eq!(
            ACCEPTANCE_PATHS
                .iter()
                .filter(|path| **path == "duplicate.md")
                .count(),
            2
        );
    }

    #[test]
    fn paginates_list_and_sends_bearer_without_logging_it() {
        let mut server = Server::new();
        let first = server
            .mock("GET", "/files")
            .match_header("authorization", "Bearer top-secret")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "1000".into()),
                Matcher::UrlEncoded(
                    "q".into(),
                    "'fixture_123' in parents and trashed = false".into(),
                ),
                Matcher::UrlEncoded(
                    "fields".into(),
                    "nextPageToken,files(id,name,appProperties)".into(),
                ),
            ]))
            .with_body(r#"{"files":[{"id":"1","name":"a"}],"nextPageToken":"p2"}"#)
            .create();
        let second = server
            .mock("GET", "/files")
            .match_query(Matcher::UrlEncoded("pageToken".into(), "p2".into()))
            .with_body(r#"{"files":[{"id":"2","name":"b"}]}"#)
            .create();
        let files = harness(&server).list_all().unwrap();
        assert_eq!(files.len(), 2);
        first.assert();
        second.assert();
    }

    #[test]
    fn initial_sync_gets_token_before_scan_then_drains_all_change_pages() {
        let mut server = Server::new();
        let token = server
            .mock("GET", "/changes/startPageToken")
            .match_query(Matcher::Any)
            .with_body(r#"{"startPageToken":"s1"}"#)
            .create();
        let scan = server
            .mock("GET", "/files")
            .match_query(Matcher::Any)
            .with_body(r#"{"files":[]}"#)
            .create();
        let changes1 = server
            .mock("GET", "/changes")
            .match_query(Matcher::UrlEncoded("pageToken".into(), "s1".into()))
            .with_body(r#"{"changes":[{"fileId":"1"}],"nextPageToken":"s2"}"#)
            .create();
        let changes2 = server
            .mock("GET", "/changes")
            .match_query(Matcher::UrlEncoded("pageToken".into(), "s2".into()))
            .with_body(r#"{"changes":[{"fileId":"2","removed":true}],"newStartPageToken":"s3"}"#)
            .create();
        let (files, drain) = harness(&server).initial_scan_and_drain().unwrap();
        assert!(files.is_empty());
        assert_eq!(drain.changes.len(), 2);
        assert_eq!(drain.durable_cursor, "s3");
        token.assert();
        scan.assert();
        changes1.assert();
        changes2.assert();
    }

    #[test]
    fn multipart_upload_records_hash_and_download_verifies_it() {
        let mut server = Server::new();
        let upload = server
            .mock("POST", "/files")
            .match_query(Matcher::UrlEncoded("uploadType".into(), "multipart".into()))
            .match_header(
                "content-type",
                "multipart/related; boundary=myvault-phase0-boundary",
            )
            .match_body(Matcher::Regex("myVaultSha256.*thai.md".into()))
            .with_body(r#"{"id":"f1","name":"thai.md","appProperties":{}}"#)
            .create();
        let body = "สวัสดี".as_bytes();
        harness(&server)
            .upload_multipart("thai.md", "text/markdown", body)
            .unwrap();
        upload.assert();

        let download = server
            .mock("GET", "/files/f1")
            .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
            .with_body(body)
            .create();
        let expected = sha256(body);
        assert_eq!(
            harness(&server)
                .download_and_verify("f1", &expected)
                .unwrap(),
            body
        );
        download.assert();
    }

    #[test]
    fn resumable_upload_uses_location_handles_308_and_returns_final_metadata() {
        let mut server = Server::new();
        let session_url = format!("{}/upload-session/fixture", server.url());
        let initiate = server
            .mock("POST", "/files")
            .match_query(Matcher::UrlEncoded("uploadType".into(), "resumable".into()))
            .match_header("x-upload-content-type", "application/octet-stream")
            .match_header(
                "x-upload-content-length",
                Matcher::Exact((RESUMABLE_THRESHOLD + 1).to_string()),
            )
            .match_body(Matcher::Regex("myVaultSha256.*large.bin".into()))
            .with_status(200)
            .with_header("Location", &session_url)
            .create();
        let first_chunk = server
            .mock("PUT", "/upload-session/fixture")
            .match_header(
                "content-range",
                Matcher::Exact(format!(
                    "bytes 0-{}/{total}",
                    RESUMABLE_CHUNK_SIZE - 1,
                    total = RESUMABLE_THRESHOLD + 1
                )),
            )
            .with_status(308)
            .with_header("Range", &format!("bytes=0-{}", RESUMABLE_CHUNK_SIZE - 1))
            .create();
        let hash = sha256(&vec![b'L'; RESUMABLE_THRESHOLD + 1]);
        let final_chunk = server
            .mock("PUT", "/upload-session/fixture")
            .match_header(
                "content-range",
                Matcher::Exact(format!(
                    "bytes {start}-{end}/{total}",
                    start = RESUMABLE_CHUNK_SIZE,
                    end = RESUMABLE_THRESHOLD,
                    total = RESUMABLE_THRESHOLD + 1
                )),
            )
            .with_status(200)
            .with_body(format!(
                r#"{{"id":"large1","name":"large.bin","appProperties":{{"myVaultSha256":"{hash}"}}}}"#
            ))
            .create();

        let bytes = vec![b'L'; RESUMABLE_THRESHOLD + 1];
        let uploaded = harness(&server)
            .upload_resumable("large.bin", "application/octet-stream", &bytes)
            .unwrap();
        assert_eq!(uploaded.id, "large1");
        assert_eq!(uploaded.app_properties.get("myVaultSha256"), Some(&hash));
        initiate.assert();
        first_chunk.assert();
        final_chunk.assert();
    }

    #[test]
    fn resumable_location_cannot_redirect_bearer_to_another_origin() {
        let mut server = Server::new();
        let initiate = server
            .mock("POST", "/files")
            .match_query(Matcher::UrlEncoded("uploadType".into(), "resumable".into()))
            .with_status(200)
            .with_header("Location", "https://evil.example/steal-token")
            .create();
        let bytes = vec![b'X'; RESUMABLE_THRESHOLD + 1];
        assert!(matches!(
            harness(&server).upload_resumable("large.bin", "application/octet-stream", &bytes,),
            Err(SyncError::InvalidResponse(_))
        ));
        initiate.assert();
    }

    #[test]
    fn resumable_308_rejects_out_of_bounds_range() {
        let mut server = Server::new();
        let session_url = format!("{}/upload-session/bad-range", server.url());
        let initiate = server
            .mock("POST", "/files")
            .match_query(Matcher::UrlEncoded("uploadType".into(), "resumable".into()))
            .with_status(200)
            .with_header("Location", &session_url)
            .create();
        let chunk = server
            .mock("PUT", "/upload-session/bad-range")
            .with_status(308)
            .with_header("Range", &format!("bytes=0-{}", RESUMABLE_CHUNK_SIZE + 10))
            .create();
        let bytes = vec![b'X'; RESUMABLE_THRESHOLD + 1];
        assert!(matches!(
            harness(&server).upload_resumable("large.bin", "application/octet-stream", &bytes,),
            Err(SyncError::InvalidResponse(_))
        ));
        initiate.assert();
        chunk.assert();
    }

    #[test]
    fn resumable_308_rejects_non_advancing_range() {
        let mut server = Server::new();
        let session_url = format!("{}/upload-session/stalled", server.url());
        let initiate = server
            .mock("POST", "/files")
            .match_query(Matcher::UrlEncoded("uploadType".into(), "resumable".into()))
            .with_status(200)
            .with_header("Location", &session_url)
            .create();
        let first = server
            .mock("PUT", "/upload-session/stalled")
            .match_header(
                "content-range",
                Matcher::Regex(format!("bytes 0-{}", RESUMABLE_CHUNK_SIZE - 1)),
            )
            .with_status(308)
            .with_header("Range", &format!("bytes=0-{}", RESUMABLE_CHUNK_SIZE - 1))
            .create();
        let stalled = server
            .mock("PUT", "/upload-session/stalled")
            .match_header(
                "content-range",
                Matcher::Regex(format!("bytes {}-", RESUMABLE_CHUNK_SIZE)),
            )
            .with_status(308)
            .with_header("Range", &format!("bytes=0-{}", RESUMABLE_CHUNK_SIZE - 1))
            .create();
        let bytes = vec![b'X'; RESUMABLE_THRESHOLD + 1];
        assert!(matches!(
            harness(&server).upload_resumable("large.bin", "application/octet-stream", &bytes,),
            Err(SyncError::InvalidResponse(_))
        ));
        initiate.assert();
        first.assert();
        stalled.assert();
    }

    #[test]
    fn retryable_resumable_final_response_reconciles_before_retry() {
        let mut server = Server::new();
        let session_url = format!("{}/upload-session/unknown", server.url());
        let initiate = server
            .mock("POST", "/files")
            .match_query(Matcher::UrlEncoded("uploadType".into(), "resumable".into()))
            .with_status(200)
            .with_header("Location", &session_url)
            .create();
        let first_chunk = server
            .mock("PUT", "/upload-session/unknown")
            .with_status(308)
            .with_header("Range", &format!("bytes=0-{}", RESUMABLE_CHUNK_SIZE - 1))
            .create();
        let failed_final = server
            .mock("PUT", "/upload-session/unknown")
            .with_status(503)
            .with_body("outcome unknown")
            .create();
        let bytes = vec![b'U'; RESUMABLE_THRESHOLD + 1];
        let hash = sha256(&bytes);
        let list = server
            .mock("GET", "/files")
            .match_query(Matcher::Any)
            .with_body(format!(
                r#"{{"files":[{{"id":"already-there","name":"large.bin","appProperties":{{"myVaultSha256":"{hash}"}}}}]}}"#
            ))
            .create();

        assert_eq!(
            harness(&server)
                .upload_resumable_with_verification(
                    "large.bin",
                    "application/octet-stream",
                    &bytes,
                )
                .unwrap(),
            VerifiedUpload::Reconciled(UnknownUploadResolution::ConfirmExisting {
                file_id: "already-there".into()
            })
        );
        initiate.assert();
        first_chunk.assert();
        failed_final.assert();
        list.assert();
    }

    #[test]
    fn cleanup_only_trashes_exact_allowlisted_folder() {
        let mut server = Server::new();
        let metadata = server
            .mock("GET", "/files/fixture_123")
            .match_query(Matcher::Any)
            .with_body(format!(
                r#"{{"id":"fixture_123","name":"myVault-spike-20260711-a1b2c3","mimeType":"{FOLDER_MIME_TYPE}","appProperties":{{"{FIXTURE_MARKER_PROPERTY}":"test-marker"}}}}"#
            ))
            .create();
        let trash = server
            .mock("PATCH", "/files/fixture_123")
            .match_query(Matcher::Any)
            .match_body(Matcher::JsonString(r#"{"trashed":true}"#.into()))
            .with_body(r#"{"id":"fixture_123","name":"myVault-spike-20260711-a1b2c3"}"#)
            .create();
        harness(&server).trash_fixture_folder().unwrap();
        metadata.assert();
        trash.assert();
    }

    #[test]
    fn cleanup_refuses_remote_folder_when_random_marker_does_not_match() {
        let mut server = Server::new();
        let metadata = server
            .mock("GET", "/files/fixture_123")
            .match_query(Matcher::Any)
            .with_body(format!(
                r#"{{"id":"fixture_123","name":"myVault-spike-20260711-a1b2c3","mimeType":"{FOLDER_MIME_TYPE}","appProperties":{{"{FIXTURE_MARKER_PROPERTY}":"attacker-marker"}}}}"#
            ))
            .create();
        let no_trash = server
            .mock("PATCH", "/files/fixture_123")
            .expect(0)
            .create();
        assert_eq!(
            harness(&server).trash_fixture_folder(),
            Err(SyncError::FixtureNotAllowlisted)
        );
        metadata.assert();
        no_trash.assert();
    }

    #[test]
    fn unknown_upload_is_verified_by_parent_name_and_sha256_property() {
        let mut server = Server::new();
        let hash = sha256(b"payload");
        let body = format!(
            r#"{{"files":[{{"id":"f1","name":"note.md","appProperties":{{"myVaultSha256":"{hash}"}}}}]}}"#
        );
        let list = server
            .mock("GET", "/files")
            .match_query(Matcher::Any)
            .with_body(body)
            .create();
        assert_eq!(
            harness(&server)
                .resolve_unknown_upload("note.md", &hash)
                .unwrap(),
            UnknownUploadResolution::ConfirmExisting {
                file_id: "f1".into()
            }
        );
        list.assert();
    }

    #[test]
    #[ignore = "requires MYVAULT_DRIVE_LIVE=1 and a short-lived access token"]
    fn live_fixture_smoke_is_explicitly_env_gated() {
        if std::env::var("MYVAULT_DRIVE_LIVE").as_deref() != Ok("1") {
            return;
        }
        let token = std::env::var("MYVAULT_DRIVE_ACCESS_TOKEN").expect("access token required");
        let fixture_name =
            std::env::var("MYVAULT_DRIVE_FIXTURE_NAME").expect("allowlisted fixture name required");
        let config = DriveHarnessConfig::google(fixture_name).enable_live();
        let mut live = DriveHarness::new(config, BearerToken::new(token)).unwrap();
        let folder = live.create_fixture_folder().unwrap();
        assert!(!folder.id.is_empty());
        let fixture_set = live.create_acceptance_fixture_set().unwrap();
        assert_eq!(fixture_set.files.len(), 9);
        assert!(matches!(
            fixture_set.large_attachment,
            VerifiedUpload::Uploaded(_) | VerifiedUpload::Reconciled(_)
        ));
        // The root listing includes root files and folders; nested content is
        // represented by the successful fixture builder result above.
        assert!(!live.list_all().unwrap().is_empty());
        live.trash_fixture_folder().unwrap();
    }
}
