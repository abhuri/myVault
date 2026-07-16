use crate::{
    client::validate_identifier, AccessToken, Error, ErrorCode, ReadOnlyDrive, Result,
    FOLDER_MIME_TYPE,
};
use reqwest::{
    blocking::Response,
    header::{CONTENT_LENGTH, LOCATION, RANGE, RETRY_AFTER},
    StatusCode, Url,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fmt,
    fmt::Write as _,
    io::{Read, Write},
    time::Duration,
};

const GOOGLE_API_BASE: &str = "https://www.googleapis.com/drive/v3/";
const GOOGLE_UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3/";
const DEFAULT_MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_BLOB_BYTES: u64 = 512 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CHUNK_ALIGNMENT: usize = 256 * 1024;
pub const RESUMABLE_UPLOAD_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const MAX_ANCESTRY_DEPTH: usize = 128;
const MAX_RETRY_AFTER_SECONDS: u64 = 60 * 60;
const TRANSFER_FILE_FIELDS: &str =
    "id,name,mimeType,parents,trashed,version,size,sha256Checksum,appProperties";

/// One exact request body range for a guarded resumable upload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadChunkPlan {
    offset: u64,
    byte_len: usize,
    end_offset: u64,
}

impl UploadChunkPlan {
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }

    #[must_use]
    pub const fn end_offset(self) -> u64 {
        self.end_offset
    }
}

/// Plans the next request using the same 8 MiB boundary enforced by the
/// protocol adapter. A zero-byte object emits one empty final request.
///
/// # Errors
/// Rejects offsets beyond the object or a completed non-empty session.
pub fn plan_resumable_upload_chunk(total_size: u64, next_offset: u64) -> Result<UploadChunkPlan> {
    if next_offset > total_size || (total_size != 0 && next_offset == total_size) {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    let remaining = total_size - next_offset;
    let byte_len = usize::try_from(remaining.min(RESUMABLE_UPLOAD_CHUNK_BYTES as u64))
        .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
    let end_offset = next_offset
        .checked_add(u64::try_from(byte_len).map_err(|_| Error::new(ErrorCode::InvalidInput))?)
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
    Ok(UploadChunkPlan {
        offset: next_offset,
        byte_len,
        end_offset,
    })
}

#[derive(Deserialize)]
struct TransferFile {
    id: String,
    name: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
    #[serde(default)]
    parents: Vec<String>,
    trashed: bool,
    version: Option<String>,
    size: Option<String>,
    #[serde(rename = "sha256Checksum")]
    sha256_checksum: Option<String>,
    #[serde(rename = "appProperties")]
    app_properties: Option<TransferProperties>,
}

#[derive(Deserialize)]
struct TransferProperties {
    #[serde(rename = "myvaultOperation")]
    operation: Option<String>,
    #[serde(rename = "myvaultSha256")]
    sha256: Option<String>,
    #[serde(rename = "myvaultSize")]
    size: Option<String>,
}

#[derive(Deserialize)]
struct TransferFilePage {
    #[serde(default)]
    files: Vec<TransferFile>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(default, rename = "incompleteSearch")]
    incomplete_search: bool,
}

/// A create-only transfer description. Debug output deliberately redacts
/// provider identifiers, the display name, and operation marker.
#[derive(Clone, Eq, PartialEq)]
pub struct CreateIntent {
    parent_id: String,
    name: String,
    mime_type: String,
    operation_marker: String,
    sha256: String,
    size: u64,
}

impl CreateIntent {
    /// Builds a bounded create-only intent.
    ///
    /// # Errors
    /// Rejects malformed provider ids, names, MIME types, hashes, or markers.
    pub fn new(
        parent_id: impl Into<String>,
        name: impl Into<String>,
        mime_type: impl Into<String>,
        operation_marker: impl Into<String>,
        sha256: impl Into<String>,
        size: u64,
    ) -> Result<Self> {
        let value = Self {
            parent_id: parent_id.into(),
            name: name.into(),
            mime_type: mime_type.into(),
            operation_marker: operation_marker.into(),
            sha256: sha256.into(),
            size,
        };
        validate_identifier(&value.parent_id)?;
        validate_name(&value.name)?;
        validate_mime_type(&value.mime_type)?;
        validate_identifier(&value.operation_marker)?;
        validate_sha256(&value.sha256)?;
        Ok(value)
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub fn operation_marker(&self) -> &str {
        &self.operation_marker
    }
}

impl fmt::Debug for CreateIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreateIntent")
            .field("parent_id", &"[REDACTED]")
            .field("name", &"[REDACTED]")
            .field("mime_type", &self.mime_type)
            .field("operation_marker", &"[REDACTED]")
            .field("sha256", &self.sha256)
            .field("size", &self.size)
            .finish()
    }
}

/// An exact expected remote blob revision for guarded download.
#[derive(Clone, Eq, PartialEq)]
pub struct DownloadIntent {
    file_id: String,
    parent_id: String,
    provider_version: String,
    sync_revision: String,
    sha256: String,
    size: u64,
}

impl DownloadIntent {
    /// # Errors
    /// Rejects malformed identifiers, revisions, or SHA-256 values.
    pub fn from_sync_revision(
        file_id: impl Into<String>,
        parent_id: impl Into<String>,
        sync_revision: impl Into<String>,
        sha256: impl Into<String>,
        size: u64,
    ) -> Result<Self> {
        let sync_revision = sync_revision.into();
        let provider_version = provider_version_from_sync_revision(&sync_revision)?;
        let value = Self {
            file_id: file_id.into(),
            parent_id: parent_id.into(),
            provider_version,
            sync_revision,
            sha256: sha256.into(),
            size,
        };
        validate_identifier(&value.file_id)?;
        validate_identifier(&value.parent_id)?;
        validate_sha256(&value.sha256)?;
        Ok(value)
    }

    /// Backwards-compatible constructor with the same canonical sync-revision
    /// contract as [`Self::from_sync_revision`].
    ///
    /// # Errors
    /// Rejects malformed identifiers, revisions, or SHA-256 values.
    pub fn new(
        file_id: impl Into<String>,
        parent_id: impl Into<String>,
        sync_revision: impl Into<String>,
        sha256: impl Into<String>,
        size: u64,
    ) -> Result<Self> {
        Self::from_sync_revision(file_id, parent_id, sync_revision, sha256, size)
    }
}

impl fmt::Debug for DownloadIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DownloadIntent")
            .field("file_id", &"[REDACTED]")
            .field("parent_id", &"[REDACTED]")
            .field("sync_revision", &self.sync_revision)
            .field("sha256", &self.sha256)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Redacted, provider-verified metadata safe for native orchestration.
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteObject {
    file_id: String,
    provider_version: String,
    sync_revision: String,
    sha256: String,
    size: u64,
}

impl RemoteObject {
    #[must_use]
    pub fn file_id(&self) -> &str {
        &self.file_id
    }

    #[must_use]
    pub fn sync_revision(&self) -> &str {
        &self.sync_revision
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

impl fmt::Debug for RemoteObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteObject")
            .field("file_id", &"[REDACTED]")
            .field("sync_revision", &self.sync_revision)
            .field("sha256", &self.sha256)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileReason {
    ExistingDifferentContent,
    OperationMarkerMismatch,
    DuplicateName,
    DuplicateOperationMarker,
}

/// A create permit cannot be constructed by callers and is consumed by
/// resumable initiation. This makes reconciliation an API-enforced precursor.
pub struct CreatePermit {
    intent: CreateIntent,
}

impl fmt::Debug for CreatePermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreatePermit")
            .field("intent", &self.intent)
            .finish()
    }
}

#[derive(Debug)]
pub enum CreateReconciliation {
    Absent(CreatePermit),
    VerifiedExisting(RemoteObject),
    NeedsReconcile(ReconcileReason),
}

/// Bearer-like session URI retained only in native memory. This type does not
/// implement `Clone`, `Display`, or serialization and its Debug output is
/// always redacted.
pub struct UploadSession {
    uri: SecretString,
    intent: CreateIntent,
    next_offset: u64,
}

impl UploadSession {
    #[must_use]
    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.intent.size
    }
}

impl fmt::Debug for UploadSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadSession")
            .field("uri", &"[REDACTED]")
            .field("intent", &self.intent)
            .field("next_offset", &self.next_offset)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UploadProgress {
    InProgress { next_offset: u64 },
    Complete(RemoteObject),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDownload {
    size: u64,
    sha256: String,
    sync_revision: String,
}

impl VerifiedDownload {
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub fn sync_revision(&self) -> &str {
        &self.sync_revision
    }
}

/// Narrow create-only and exact-blob transfer capability. It contains no
/// generic request method and cannot update an existing remote object.
pub struct TransferDrive {
    read_only: ReadOnlyDrive,
    upload_base: Url,
    account_id: String,
    root_id: String,
    max_blob_bytes: u64,
}

impl fmt::Debug for TransferDrive {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferDrive")
            .field("api_origin", &self.read_only.api_origin())
            .field(
                "upload_origin",
                &self.upload_base.origin().ascii_serialization(),
            )
            .field("account_id", &"[REDACTED]")
            .field("root_id", &"[REDACTED]")
            .field("token", &"[REDACTED]")
            .field("max_blob_bytes", &self.max_blob_bytes)
            .finish_non_exhaustive()
    }
}

impl TransferDrive {
    /// Constructs a production capability pinned to Google's HTTPS origins.
    ///
    /// # Errors
    /// Rejects malformed binding identifiers or native HTTP initialization.
    pub fn google(
        token: AccessToken,
        account_id: impl Into<String>,
        root_id: impl Into<String>,
    ) -> Result<Self> {
        Self::build(
            token,
            Url::parse(GOOGLE_API_BASE).map_err(|_| Error::new(ErrorCode::UnexpectedOrigin))?,
            Url::parse(GOOGLE_UPLOAD_BASE).map_err(|_| Error::new(ErrorCode::UnexpectedOrigin))?,
            account_id.into(),
            root_id.into(),
            DEFAULT_MAX_BLOB_BYTES,
            REQUEST_TIMEOUT,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        token: AccessToken,
        api_base: Url,
        upload_base: Url,
        account_id: String,
        root_id: String,
        max_blob_bytes: u64,
        timeout: Duration,
        https_only: bool,
    ) -> Result<Self> {
        validate_identifier(&account_id)?;
        validate_identifier(&root_id)?;
        if max_blob_bytes == 0
            || api_base.origin() != upload_base.origin()
            || !upload_base.path().ends_with('/')
            || upload_base.query().is_some()
            || upload_base.fragment().is_some()
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let read_only = ReadOnlyDrive::build(
            token,
            api_base,
            DEFAULT_MAX_METADATA_BYTES,
            timeout,
            https_only,
        )?;
        Ok(Self {
            read_only,
            upload_base,
            account_id,
            root_id,
            max_blob_bytes,
        })
    }

    #[cfg(test)]
    fn for_test(server: &mockito::Server, max_blob_bytes: u64) -> Self {
        let origin = server.url();
        Self::build(
            AccessToken::new("transfer-secret-token"),
            Url::parse(&format!("{origin}/drive/v3/")).unwrap(),
            Url::parse(&format!("{origin}/upload/drive/v3/")).unwrap(),
            "account_1".into(),
            "root_1".into(),
            max_blob_bytes,
            Duration::from_secs(2),
            false,
        )
        .unwrap()
    }

    /// Constructs an HTTP capability for cross-crate integration tests.
    /// This constructor is deliberately absent from production builds.
    ///
    /// # Errors
    /// Rejects malformed origins, bindings, limits, or client setup.
    #[cfg(feature = "test-support")]
    pub fn for_test_origins(
        api_base: &str,
        upload_base: &str,
        account_id: impl Into<String>,
        root_id: impl Into<String>,
        max_blob_bytes: u64,
    ) -> Result<Self> {
        Self::build(
            AccessToken::new("integration-test-token"),
            Url::parse(api_base).map_err(|_| Error::new(ErrorCode::InvalidInput))?,
            Url::parse(upload_base).map_err(|_| Error::new(ErrorCode::InvalidInput))?,
            account_id.into(),
            root_id.into(),
            max_blob_bytes,
            Duration::from_secs(2),
            false,
        )
    }

    /// Re-establishes account/root/parent ancestry and reconciles both the
    /// operation marker and exact name before a create can be initiated.
    ///
    /// # Errors
    /// Returns only stable redacted classifications.
    pub fn reconcile_create(&self, intent: CreateIntent) -> Result<CreateReconciliation> {
        let marker_query = format!(
            "'{}' in parents and trashed = false and appProperties has {{ key='myvaultOperation' and value='{}' }}",
            intent.parent_id,
            escape_query_literal(&intent.operation_marker)
        );
        let marker_matches = self.query_files(&marker_query, &intent.parent_id)?;
        match marker_matches.as_slice() {
            [file] => {
                if file_matches_intent(file, &intent) {
                    return match self.recheck_existing(&file.id, &intent, true)? {
                        Some(verified) => Ok(CreateReconciliation::VerifiedExisting(
                            remote_object(&verified, &intent)?,
                        )),
                        None => Ok(CreateReconciliation::NeedsReconcile(
                            ReconcileReason::OperationMarkerMismatch,
                        )),
                    };
                }
                return Ok(CreateReconciliation::NeedsReconcile(
                    ReconcileReason::OperationMarkerMismatch,
                ));
            }
            [] => {}
            _ => {
                return Ok(CreateReconciliation::NeedsReconcile(
                    ReconcileReason::DuplicateOperationMarker,
                ));
            }
        }

        let name_query = format!(
            "'{}' in parents and trashed = false and name = '{}'",
            intent.parent_id,
            escape_query_literal(&intent.name)
        );
        let name_matches = self.query_files(&name_query, &intent.parent_id)?;
        match name_matches.as_slice() {
            [] => Ok(CreateReconciliation::Absent(CreatePermit { intent })),
            [file] if file_has_same_content(file, &intent) => {
                match self.recheck_existing(&file.id, &intent, false)? {
                    Some(verified) => Ok(CreateReconciliation::VerifiedExisting(
                        remote_object_same_content(&verified, &intent)?,
                    )),
                    None => Ok(CreateReconciliation::NeedsReconcile(
                        ReconcileReason::ExistingDifferentContent,
                    )),
                }
            }
            [_] => Ok(CreateReconciliation::NeedsReconcile(
                ReconcileReason::ExistingDifferentContent,
            )),
            _ => Ok(CreateReconciliation::NeedsReconcile(
                ReconcileReason::DuplicateName,
            )),
        }
    }

    /// Proves that a completed create is still unique by both its operation
    /// marker and exact display name before callers publish durable completion.
    /// Both queries must return exactly the just-created file, and a final
    /// metadata read must preserve its parent, content, and marker. The provider
    /// version may advance between exact reads, but it must never regress.
    ///
    /// # Errors
    /// Fails closed when either identity is absent, duplicated, points at a
    /// different file, or when the final exact metadata no longer matches.
    pub fn verify_created_upload(
        &self,
        intent: &CreateIntent,
        created: &RemoteObject,
    ) -> Result<RemoteObject> {
        let marker_query = format!(
            "'{}' in parents and trashed = false and appProperties has {{ key='myvaultOperation' and value='{}' }}",
            intent.parent_id,
            escape_query_literal(&intent.operation_marker)
        );
        let marker_matches =
            self.query_files_with_page_size(&marker_query, &intent.parent_id, "2")?;
        require_unique_created_match(&marker_matches, intent, created)?;

        let name_query = format!(
            "'{}' in parents and trashed = false and name = '{}'",
            intent.parent_id,
            escape_query_literal(&intent.name)
        );
        let name_matches = self.query_files_with_page_size(&name_query, &intent.parent_id, "2")?;
        require_unique_created_match(&name_matches, intent, created)?;

        let verified = self
            .recheck_existing(created.file_id(), intent, true)?
            .ok_or_else(|| Error::new(ErrorCode::RevisionMismatch))?;
        let final_object = remote_object(&verified, intent)?;
        require_provider_version_not_regressed(
            &created.provider_version,
            &final_object.provider_version,
        )?;
        Ok(final_object)
    }

    /// Initiates a create-only resumable upload after a consumed absence
    /// permit. No existing file id can be supplied to this endpoint.
    ///
    /// # Errors
    /// Rejects redirects, origin changes, malformed locations, and provider
    /// failures without retaining response bodies.
    pub fn initiate_resumable_create(&self, permit: CreatePermit) -> Result<UploadSession> {
        self.ensure_folder_below_root(&permit.intent.parent_id)?;
        let url = self
            .upload_base
            .join("files")
            .map_err(|_| Error::new(ErrorCode::UnexpectedOrigin))?;
        self.ensure_upload_origin(&url)?;
        let body = serde_json::to_vec(&json!({
            "name": permit.intent.name,
            "mimeType": permit.intent.mime_type,
            "parents": [permit.intent.parent_id],
            "appProperties": {
                "myvaultOperation": permit.intent.operation_marker,
                "myvaultSha256": permit.intent.sha256,
                "myvaultSize": permit.intent.size.to_string()
            }
        }))
        .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        let response = self.read_only.post_resumable_create(
            url,
            TRANSFER_FILE_FIELDS,
            &permit.intent.mime_type,
            permit.intent.size,
            body,
        )?;
        let status = response.status();
        if status.is_redirection() {
            return Err(Error::new(ErrorCode::RedirectRejected));
        }
        if !status.is_success() {
            return Err(map_status_response(&response, false));
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?;
        let session_uri =
            Url::parse(location).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
        self.ensure_session_url(&session_uri)?;
        Ok(UploadSession {
            uri: SecretString::from(session_uri.to_string()),
            intent: permit.intent,
            next_offset: 0,
        })
    }

    /// Uploads one bounded chunk at the session's exact next offset.
    /// Non-final chunks must be aligned to 256 KiB.
    ///
    /// # Errors
    /// Rejects stale offsets, regressing ranges, redirects, expired sessions,
    /// and malformed final metadata.
    pub fn upload_chunk(
        &self,
        session: &mut UploadSession,
        bytes: &[u8],
    ) -> Result<UploadProgress> {
        if bytes.len() > RESUMABLE_UPLOAD_CHUNK_BYTES {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let length = u64::try_from(bytes.len()).map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        let end_offset = session
            .next_offset
            .checked_add(length)
            .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
        if end_offset > session.intent.size
            || (end_offset < session.intent.size
                && (bytes.is_empty() || bytes.len() % CHUNK_ALIGNMENT != 0))
            || (session.intent.size != 0 && bytes.is_empty())
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let content_range = if session.intent.size == 0 {
            "bytes */0".to_owned()
        } else {
            format!(
                "bytes {}-{}/{}",
                session.next_offset,
                end_offset - 1,
                session.intent.size
            )
        };
        let previous = session.next_offset;
        let response = self.put_session(session, bytes.to_vec(), &content_range)?;
        self.decode_upload_response(session, response, Some((previous, end_offset)))
    }

    /// Queries a live resumable session without persisting its bearer-like URI.
    ///
    /// # Errors
    /// Returns `SessionExpired` for 404/410 and rejects malformed ranges.
    pub fn query_upload_status(&self, session: &mut UploadSession) -> Result<UploadProgress> {
        let content_range = format!("bytes */{}", session.intent.size);
        let response = self.put_session(session, Vec::new(), &content_range)?;
        self.decode_upload_response(session, response, None)
    }

    /// Inspects one exact remote download candidate using metadata GETs only.
    /// The result proves the bound account/root ancestry, exact parent and
    /// display name, expected canonical revision, live blob MIME policy,
    /// SHA-256, and bounded byte length.
    ///
    /// # Errors
    /// Fails closed for unbound ancestry, wrong parents, trashed or Google
    /// Workspace-native objects, missing metadata, and oversized blobs.
    pub fn inspect_download_candidate(
        &self,
        file_id: &str,
        parent_id: &str,
        expected_name: &str,
        expected_sync_revision: &str,
    ) -> Result<RemoteObject> {
        validate_name(expected_name)?;
        let expected_provider_version =
            provider_version_from_sync_revision(expected_sync_revision)?;
        let (candidate, current_name) =
            self.inspect_download_candidate_metadata(file_id, parent_id)?;
        if current_name != expected_name || candidate.provider_version != expected_provider_version
        {
            return Err(Error::new(ErrorCode::RevisionMismatch));
        }
        Ok(candidate)
    }

    fn inspect_download_candidate_metadata(
        &self,
        file_id: &str,
        parent_id: &str,
    ) -> Result<(RemoteObject, String)> {
        self.ensure_folder_below_root(parent_id)?;
        let file = self.transfer_file_metadata(file_id)?;
        if file.trashed
            || file.parents.as_slice() != [parent_id]
            || file.mime_type.starts_with("application/vnd.google-apps.")
        {
            return Err(Error::new(ErrorCode::MalformedResponse));
        }
        let version = file
            .version
            .as_deref()
            .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?;
        let sha256 = file
            .sha256_checksum
            .as_deref()
            .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?
            .to_owned();
        let size = parse_size(file.size.as_deref())?;
        if size > self.max_blob_bytes {
            return Err(Error::new(ErrorCode::ResponseTooLarge));
        }
        Ok((
            RemoteObject {
                file_id: file.id,
                provider_version: version.to_owned(),
                sync_revision: sync_revision_from_provider_version(version)?,
                sha256,
                size,
            },
            file.name,
        ))
    }

    /// Rechecks the exact bound account, root ancestry, parent, file id,
    /// canonical revision, SHA-256, and byte length using metadata GETs only.
    /// This lets restart recovery validate an already-complete private stage
    /// without downloading the blob again.
    ///
    /// # Errors
    /// Returns only a stable redacted mismatch or provider classification.
    pub fn verify_download(&self, intent: &DownloadIntent) -> Result<RemoteObject> {
        self.exact_download_metadata(intent)
    }

    /// Streams an exact blob into caller-owned private staging, then validates
    /// byte length, SHA-256, and a second metadata read before returning.
    /// Google Workspace native MIME types are rejected.
    ///
    /// # Errors
    /// Returns a stable redacted classification; caller write failures are
    /// reported only as `LocalIo`.
    pub fn download_blob_to<W: Write>(
        &self,
        intent: &DownloadIntent,
        destination: &mut W,
    ) -> Result<VerifiedDownload> {
        if intent.size > self.max_blob_bytes {
            return Err(Error::new(ErrorCode::ResponseTooLarge));
        }
        let before = self.exact_download_metadata(intent)?;
        let mut response = self.read_only.get_media(&intent.file_id)?;
        if response.status().is_redirection() {
            return Err(Error::new(ErrorCode::RedirectRejected));
        }
        if !response.status().is_success() {
            return Err(map_status_response(&response, false));
        }
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|size| size != intent.size)
        {
            return Err(Error::new(ErrorCode::HashMismatch));
        }
        let mut digest = Sha256::new();
        let mut transferred = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        loop {
            let count = std::io::Read::read(&mut response, &mut buffer)
                .map_err(|_| Error::new(ErrorCode::Transport))?;
            if count == 0 {
                break;
            }
            transferred = transferred
                .checked_add(
                    u64::try_from(count).map_err(|_| Error::new(ErrorCode::ResponseTooLarge))?,
                )
                .ok_or_else(|| Error::new(ErrorCode::ResponseTooLarge))?;
            if transferred > intent.size || transferred > self.max_blob_bytes {
                return Err(Error::new(ErrorCode::ResponseTooLarge));
            }
            digest.update(&buffer[..count]);
            destination
                .write_all(&buffer[..count])
                .map_err(|_| Error::new(ErrorCode::LocalIo))?;
        }
        let sha256 =
            digest
                .finalize()
                .iter()
                .fold(String::with_capacity(64), |mut output, byte| {
                    let _ = write!(&mut output, "{byte:02x}");
                    output
                });
        if transferred != intent.size || sha256 != intent.sha256 {
            return Err(Error::new(ErrorCode::HashMismatch));
        }
        let after = self.exact_download_metadata(intent)?;
        if before.provider_version != after.provider_version
            || before.sha256 != after.sha256
            || before.size != after.size
        {
            return Err(Error::new(ErrorCode::RevisionMismatch));
        }
        Ok(VerifiedDownload {
            size: transferred,
            sha256,
            sync_revision: after.sync_revision,
        })
    }

    fn ensure_binding(&self) -> Result<()> {
        let account = self.read_only.account_identity()?;
        if account.permission_id != self.account_id {
            return Err(Error::new(ErrorCode::InvalidAccount));
        }
        self.read_only.verify_root(&self.root_id)?;
        Ok(())
    }

    fn ensure_folder_below_root(&self, folder_id: &str) -> Result<()> {
        validate_identifier(folder_id)?;
        self.ensure_binding()?;
        if folder_id == self.root_id {
            return Ok(());
        }
        let mut current = folder_id.to_owned();
        let mut visited = BTreeSet::new();
        for _ in 0..MAX_ANCESTRY_DEPTH {
            if !visited.insert(current.clone()) {
                return Err(Error::new(ErrorCode::InvalidRoot));
            }
            let file = self.read_only.file_metadata(&current)?;
            if file.trashed || file.mime_type != FOLDER_MIME_TYPE || file.parents.len() != 1 {
                return Err(Error::new(ErrorCode::InvalidRoot));
            }
            let parent = &file.parents[0];
            if parent == &self.root_id {
                return Ok(());
            }
            current.clone_from(parent);
        }
        Err(Error::new(ErrorCode::InvalidRoot))
    }

    fn query_files(&self, query: &str, parent_id: &str) -> Result<Vec<TransferFile>> {
        self.query_files_with_page_size(query, parent_id, "100")
    }

    fn query_files_with_page_size(
        &self,
        query: &str,
        parent_id: &str,
        page_size: &str,
    ) -> Result<Vec<TransferFile>> {
        if query.len() > 4096 || query.chars().any(char::is_control) {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        if page_size != "2" && page_size != "100" {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        self.ensure_folder_below_root(parent_id)?;
        let url = self.read_only.endpoint("files")?;
        let fields = format!("nextPageToken,incompleteSearch,files({TRANSFER_FILE_FIELDS})");
        let page: TransferFilePage = self.read_only.get_json(
            url,
            &[
                ("q", query),
                ("fields", &fields),
                ("pageSize", page_size),
                ("spaces", "drive"),
                ("corpora", "user"),
                ("includeItemsFromAllDrives", "true"),
                ("supportsAllDrives", "true"),
            ],
        )?;
        if page.incomplete_search || page.next_page_token.is_some() {
            return Err(Error::new(ErrorCode::AmbiguousRemote));
        }
        for file in &page.files {
            validate_transfer_file(file)?;
            if file.trashed || file.parents.as_slice() != [parent_id] {
                return Err(Error::new(ErrorCode::MalformedResponse));
            }
        }
        Ok(page.files)
    }

    fn put_session(
        &self,
        session: &UploadSession,
        body: Vec<u8>,
        content_range: &str,
    ) -> Result<Response> {
        let url = Url::parse(session.uri.expose_secret())
            .map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
        self.ensure_session_url(&url)?;
        self.ensure_folder_below_root(&session.intent.parent_id)?;
        self.read_only
            .put_resumable_session(url, body, content_range)
    }

    fn decode_upload_response(
        &self,
        session: &mut UploadSession,
        response: Response,
        uploaded: Option<(u64, u64)>,
    ) -> Result<UploadProgress> {
        let status = response.status();
        if status.is_redirection() && status != StatusCode::PERMANENT_REDIRECT {
            return Err(Error::new(ErrorCode::RedirectRejected));
        }
        if status == StatusCode::PERMANENT_REDIRECT {
            let next_offset = parse_resume_range(response.headers().get(RANGE))?;
            if next_offset > session.intent.size
                || next_offset < session.next_offset
                || uploaded.is_some_and(|(previous, submitted_end)| {
                    next_offset <= previous || next_offset > submitted_end
                })
            {
                return Err(Error::new(ErrorCode::RangeRejected));
            }
            session.next_offset = next_offset;
            return Ok(UploadProgress::InProgress { next_offset });
        }
        if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
            return Err(Error::new(ErrorCode::SessionExpired));
        }
        if status != StatusCode::OK && status != StatusCode::CREATED {
            return Err(map_status_response(&response, true));
        }
        let file: TransferFile = decode_bounded_json(response, DEFAULT_MAX_METADATA_BYTES)?;
        validate_transfer_file(&file)?;
        if !file_matches_intent(&file, &session.intent) {
            return Err(Error::new(ErrorCode::MalformedResponse));
        }
        let response_version = file
            .version
            .as_deref()
            .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?;
        let verified = self
            .recheck_existing(&file.id, &session.intent, true)?
            .ok_or_else(|| Error::new(ErrorCode::RevisionMismatch))?;
        let verified_version = verified
            .version
            .as_deref()
            .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?;
        require_provider_version_not_regressed(response_version, verified_version)?;
        session.next_offset = session.intent.size;
        Ok(UploadProgress::Complete(remote_object(
            &verified,
            &session.intent,
        )?))
    }

    fn exact_download_metadata(&self, intent: &DownloadIntent) -> Result<RemoteObject> {
        let (observed, _) =
            self.inspect_download_candidate_metadata(&intent.file_id, &intent.parent_id)?;
        if observed.provider_version != intent.provider_version {
            return Err(Error::new(ErrorCode::RevisionMismatch));
        }
        if observed.sha256 != intent.sha256 || observed.size != intent.size {
            return Err(Error::new(ErrorCode::HashMismatch));
        }
        Ok(observed)
    }

    fn transfer_file_metadata(&self, file_id: &str) -> Result<TransferFile> {
        validate_identifier(file_id)?;
        let url = self.read_only.endpoint(&format!("files/{file_id}"))?;
        let file: TransferFile = self.read_only.get_json(
            url,
            &[
                ("fields", TRANSFER_FILE_FIELDS),
                ("supportsAllDrives", "true"),
            ],
        )?;
        validate_transfer_file(&file)?;
        if file.id != file_id {
            return Err(Error::new(ErrorCode::MalformedResponse));
        }
        Ok(file)
    }

    fn recheck_existing(
        &self,
        file_id: &str,
        intent: &CreateIntent,
        require_operation_marker: bool,
    ) -> Result<Option<TransferFile>> {
        self.ensure_folder_below_root(&intent.parent_id)?;
        let file = self.transfer_file_metadata(file_id)?;
        let matches = if require_operation_marker {
            file_matches_intent(&file, intent)
        } else {
            file_has_same_content(&file, intent)
        };
        Ok(matches.then_some(file))
    }

    fn ensure_upload_origin(&self, url: &Url) -> Result<()> {
        if url.origin() != self.upload_base.origin()
            || !url.path().starts_with(self.upload_base.path())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::new(ErrorCode::UnexpectedOrigin));
        }
        Ok(())
    }

    fn ensure_session_url(&self, url: &Url) -> Result<()> {
        self.ensure_upload_origin(url)?;
        if !url.path().ends_with("/files")
            || !url
                .query_pairs()
                .any(|(key, value)| key == "upload_id" && !value.is_empty() && value.len() <= 4096)
        {
            return Err(Error::new(ErrorCode::UnexpectedOrigin));
        }
        Ok(())
    }
}

fn require_unique_created_match(
    matches: &[TransferFile],
    intent: &CreateIntent,
    created: &RemoteObject,
) -> Result<()> {
    match matches {
        [file] if file.id == created.file_id && file_matches_intent(file, intent) => Ok(()),
        _ => Err(Error::new(ErrorCode::AmbiguousRemote)),
    }
}

fn validate_transfer_file(file: &TransferFile) -> Result<()> {
    validate_identifier(&file.id).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    if file.name.is_empty()
        || file.name.len() > 1024
        || file.name.chars().any(char::is_control)
        || file.mime_type.is_empty()
        || file.mime_type.len() > 255
        || file.mime_type.chars().any(char::is_control)
        || file.parents.len() > 1
    {
        return Err(Error::new(ErrorCode::MalformedResponse));
    }
    for parent in &file.parents {
        validate_identifier(parent).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    }
    if let Some(version) = &file.version {
        validate_version(version).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    }
    if let Some(sha256) = &file.sha256_checksum {
        validate_sha256(sha256).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    }
    if file
        .size
        .as_deref()
        .is_some_and(|value| value.parse::<u64>().is_err())
    {
        return Err(Error::new(ErrorCode::MalformedResponse));
    }
    if let Some(properties) = &file.app_properties {
        if properties
            .operation
            .as_deref()
            .is_some_and(|value| validate_identifier(value).is_err())
            || properties
                .sha256
                .as_deref()
                .is_some_and(|value| validate_sha256(value).is_err())
            || properties
                .size
                .as_deref()
                .is_some_and(|value| value.parse::<u64>().is_err())
        {
            return Err(Error::new(ErrorCode::MalformedResponse));
        }
    }
    Ok(())
}

fn file_matches_intent(file: &TransferFile, intent: &CreateIntent) -> bool {
    let properties = file.app_properties.as_ref();
    !file.trashed
        && file.name == intent.name
        && file.mime_type == intent.mime_type
        && file.parents.as_slice() == [intent.parent_id.as_str()]
        && file.sha256_checksum.as_deref() == Some(intent.sha256.as_str())
        && file.size.as_deref() == Some(intent.size.to_string().as_str())
        && properties.and_then(|value| value.operation.as_deref())
            == Some(intent.operation_marker.as_str())
        && properties.and_then(|value| value.sha256.as_deref()) == Some(intent.sha256.as_str())
        && properties.and_then(|value| value.size.as_deref())
            == Some(intent.size.to_string().as_str())
}

fn file_has_same_content(file: &TransferFile, intent: &CreateIntent) -> bool {
    !file.trashed
        && file.name == intent.name
        && file.mime_type == intent.mime_type
        && file.parents.as_slice() == [intent.parent_id.as_str()]
        && file.sha256_checksum.as_deref() == Some(intent.sha256.as_str())
        && file.size.as_deref() == Some(intent.size.to_string().as_str())
}

fn remote_object(file: &TransferFile, intent: &CreateIntent) -> Result<RemoteObject> {
    if !file_matches_intent(file, intent) {
        return Err(Error::new(ErrorCode::ExistingDifferentContent));
    }
    let version = file
        .version
        .as_deref()
        .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?;
    validate_version(version)?;
    Ok(RemoteObject {
        file_id: file.id.clone(),
        provider_version: version.to_owned(),
        sync_revision: sync_revision_from_provider_version(version)?,
        sha256: intent.sha256.clone(),
        size: intent.size,
    })
}

fn remote_object_same_content(file: &TransferFile, intent: &CreateIntent) -> Result<RemoteObject> {
    if !file_has_same_content(file, intent) {
        return Err(Error::new(ErrorCode::ExistingDifferentContent));
    }
    let version = file
        .version
        .as_deref()
        .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?;
    validate_version(version)?;
    Ok(RemoteObject {
        file_id: file.id.clone(),
        provider_version: version.to_owned(),
        sync_revision: sync_revision_from_provider_version(version)?,
        sha256: intent.sha256.clone(),
        size: intent.size,
    })
}

fn decode_bounded_json<T: serde::de::DeserializeOwned>(
    mut response: Response,
    limit: usize,
) -> Result<T> {
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > limit)
    {
        return Err(Error::new(ErrorCode::ResponseTooLarge));
    }
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    std::io::Read::take(&mut response, u64::try_from(limit).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::new(ErrorCode::Transport))?;
    if bytes.len() > limit {
        return Err(Error::new(ErrorCode::ResponseTooLarge));
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::new(ErrorCode::MalformedResponse))
}

fn parse_resume_range(header: Option<&reqwest::header::HeaderValue>) -> Result<u64> {
    let Some(value) = header.and_then(|value| value.to_str().ok()) else {
        return Ok(0);
    };
    let end = value
        .strip_prefix("bytes=0-")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| Error::new(ErrorCode::RangeRejected))?;
    end.checked_add(1)
        .ok_or_else(|| Error::new(ErrorCode::RangeRejected))
}

fn map_status_response(response: &Response, session: bool) -> Error {
    let status = response.status();
    let retry_after = (status == StatusCode::TOO_MANY_REQUESTS)
        .then(|| {
            response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|seconds| *seconds <= MAX_RETRY_AFTER_SECONDS)
        })
        .flatten();
    let code = match status.as_u16() {
        401 => ErrorCode::Unauthorized,
        403 => ErrorCode::Forbidden,
        404 | 410 if session => ErrorCode::SessionExpired,
        404 => ErrorCode::NotFound,
        429 => ErrorCode::RateLimited,
        500..=599 => ErrorCode::TransientProvider,
        _ => ErrorCode::ProviderRejected,
    };
    retry_after.map_or_else(
        || Error::new(code),
        |seconds| Error::with_retry_after(code, seconds),
    )
}

fn validate_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 1024
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}

fn validate_mime_type(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with("application/vnd.google-apps.")
        || value.chars().any(char::is_control)
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}

fn validate_version(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.parse::<u64>().is_err()
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}

fn require_provider_version_not_regressed(previous: &str, current: &str) -> Result<()> {
    validate_version(previous).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    validate_version(current).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    let previous = previous
        .parse::<u64>()
        .map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    let current = current
        .parse::<u64>()
        .map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    if current < previous {
        return Err(Error::new(ErrorCode::RevisionMismatch));
    }
    Ok(())
}

fn provider_version_from_sync_revision(sync_revision: &str) -> Result<String> {
    if sync_revision.len() != 64
        || !sync_revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    let value =
        u64::from_str_radix(sync_revision, 16).map_err(|_| Error::new(ErrorCode::InvalidInput))?;
    if format!("{value:064x}") != sync_revision {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(value.to_string())
}

fn sync_revision_from_provider_version(provider_version: &str) -> Result<String> {
    validate_version(provider_version).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    let value = provider_version
        .parse::<u64>()
        .map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    Ok(format!("{value:064x}"))
}

fn parse_size(value: Option<&str>) -> Result<u64> {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))
}

fn escape_query_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    const SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SYNC_REVISION_2: &str =
        "0000000000000000000000000000000000000000000000000000000000000002";

    fn intent() -> CreateIntent {
        CreateIntent::new("root_1", "note.md", "text/markdown", "operation_1", SHA, 3).unwrap()
    }

    fn binding_mocks(server: &mut Server, count: usize) {
        server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
            .expect(count)
            .create();
        server
            .mock("GET", "/drive/v3/files/root_1")
            .match_query(Matcher::Any)
            .with_body(r#"{"id":"root_1","name":"Root","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"1"}"#)
            .expect(count)
            .create();
    }

    fn complete_file_json() -> String {
        format!(
            r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}","appProperties":{{"myvaultOperation":"operation_1","myvaultSha256":"{SHA}","myvaultSize":"3"}}}}"#
        )
    }

    fn complete_file_json_with_version(version: u64) -> String {
        complete_file_json().replace("\"version\":\"2\"", &format!("\"version\":\"{version}\""))
    }

    fn complete_file_json_for_size(size: u64) -> String {
        complete_file_json()
            .replace("\"size\":\"3\"", &format!("\"size\":\"{size}\""))
            .replace(
                "\"myvaultSize\":\"3\"",
                &format!("\"myvaultSize\":\"{size}\""),
            )
    }

    fn intent_for_size(size: u64) -> CreateIntent {
        CreateIntent::new(
            "root_1",
            "note.md",
            "text/markdown",
            "operation_1",
            SHA,
            size,
        )
        .unwrap()
    }

    fn chunk_plans(total_size: u64) -> Vec<UploadChunkPlan> {
        let mut plans = Vec::new();
        let mut next_offset = 0;
        loop {
            let plan = plan_resumable_upload_chunk(total_size, next_offset).unwrap();
            plans.push(plan);
            if plan.end_offset() == total_size {
                return plans;
            }
            next_offset = plan.end_offset();
        }
    }

    fn chunk_matrix_sizes() -> [u64; 5] {
        let chunk = RESUMABLE_UPLOAD_CHUNK_BYTES as u64;
        [0, 1, chunk, chunk + 1, chunk * 2]
    }

    #[test]
    fn eight_mib_chunk_planner_covers_zero_single_multi_and_exact_boundaries() {
        let chunk = RESUMABLE_UPLOAD_CHUNK_BYTES as u64;
        let cases = [
            (0, vec![(0, 0, 0)]),
            (1, vec![(0, 1, 1)]),
            (chunk, vec![(0, chunk, chunk)]),
            (chunk + 1, vec![(0, chunk, chunk), (chunk, 1, chunk + 1)]),
            (
                chunk * 2,
                vec![(0, chunk, chunk), (chunk, chunk, chunk * 2)],
            ),
        ];

        for (total_size, expected) in cases {
            let actual = chunk_plans(total_size)
                .into_iter()
                .map(|plan| (plan.offset(), plan.byte_len() as u64, plan.end_offset()))
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "total size: {total_size}");
            for pair in actual.windows(2) {
                assert_eq!(pair[0].2, pair[1].0, "total size: {total_size}");
                assert!(pair[1].0 > pair[0].0, "total size: {total_size}");
            }
        }

        assert_eq!(
            plan_resumable_upload_chunk(1, 1).unwrap_err().code(),
            ErrorCode::InvalidInput
        );
        assert_eq!(
            plan_resumable_upload_chunk(1, 2).unwrap_err().code(),
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn upload_protocol_emits_every_planned_range_monotonically_without_create() {
        for total_size in chunk_matrix_sizes() {
            let plans = chunk_plans(total_size);
            let mut server = Server::new();
            binding_mocks(&mut server, plans.len() + 1);
            let blind_create = server
                .mock("POST", "/upload/drive/v3/files")
                .expect(0)
                .create();
            let final_body = complete_file_json_for_size(total_size);
            let metadata = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(Matcher::Regex("fields".into()))
                .with_body(final_body.clone())
                .create();
            let mut uploads = Vec::new();
            for plan in &plans {
                let content_range = if total_size == 0 {
                    "bytes */0".to_owned()
                } else {
                    format!(
                        "bytes {}-{}/{}",
                        plan.offset(),
                        plan.end_offset() - 1,
                        total_size
                    )
                };
                let mut upload = server
                    .mock("PUT", "/upload/drive/v3/files")
                    .match_query(Matcher::Any)
                    .match_header("content-range", Matcher::Exact(content_range))
                    .match_body(Matcher::Any);
                if plan.end_offset() == total_size {
                    upload = upload.with_status(200).with_body(final_body.clone());
                } else {
                    upload = upload
                        .with_status(308)
                        .with_header("range", &format!("bytes=0-{}", plan.end_offset() - 1));
                }
                uploads.push(upload.create());
            }

            let drive = TransferDrive::for_test(&server, total_size.max(1));
            let mut session = UploadSession {
                uri: SecretString::from(format!(
                    "{}/upload/drive/v3/files?upload_id=session",
                    server.url()
                )),
                intent: intent_for_size(total_size),
                next_offset: 0,
            };
            for plan in &plans {
                assert_eq!(session.next_offset(), plan.offset());
                let body = vec![b'x'; plan.byte_len()];
                let progress = drive.upload_chunk(&mut session, &body).unwrap();
                assert_eq!(session.next_offset(), plan.end_offset());
                if plan.end_offset() == total_size {
                    assert!(matches!(progress, UploadProgress::Complete(_)));
                } else {
                    assert_eq!(
                        progress,
                        UploadProgress::InProgress {
                            next_offset: plan.end_offset()
                        }
                    );
                }
            }
            for upload in uploads {
                upload.assert();
            }
            blind_create.assert();
            metadata.assert();
        }
    }

    #[test]
    fn lost_response_status_recovery_covers_every_emitted_boundary_without_create() {
        for total_size in chunk_matrix_sizes() {
            for plan in chunk_plans(total_size) {
                let final_boundary = plan.end_offset() == total_size;
                let mut server = Server::new();
                binding_mocks(&mut server, if final_boundary { 2 } else { 1 });
                let blind_create = server
                    .mock("POST", "/upload/drive/v3/files")
                    .expect(0)
                    .create();
                let final_body = complete_file_json_for_size(total_size);
                let metadata = final_boundary.then(|| {
                    server
                        .mock("GET", "/drive/v3/files/file_1")
                        .match_query(Matcher::Regex("fields".into()))
                        .with_body(final_body.clone())
                        .create()
                });
                let mut status = server
                    .mock("PUT", "/upload/drive/v3/files")
                    .match_query(Matcher::Any)
                    .match_header(
                        "content-range",
                        Matcher::Exact(format!("bytes */{total_size}")),
                    )
                    .match_body("");
                if final_boundary {
                    status = status.with_status(200).with_body(final_body);
                } else {
                    status = status
                        .with_status(308)
                        .with_header("range", &format!("bytes=0-{}", plan.end_offset() - 1));
                }
                let status = status.create();
                let drive = TransferDrive::for_test(&server, total_size.max(1));
                let mut session = UploadSession {
                    uri: SecretString::from(format!(
                        "{}/upload/drive/v3/files?upload_id=session",
                        server.url()
                    )),
                    intent: intent_for_size(total_size),
                    next_offset: plan.offset(),
                };

                let progress = drive.query_upload_status(&mut session).unwrap();
                assert_eq!(session.next_offset(), plan.end_offset());
                if final_boundary {
                    assert!(matches!(progress, UploadProgress::Complete(_)));
                } else {
                    assert_eq!(
                        progress,
                        UploadProgress::InProgress {
                            next_offset: plan.end_offset()
                        }
                    );
                    let retry =
                        plan_resumable_upload_chunk(total_size, session.next_offset()).unwrap();
                    assert_eq!(retry.offset(), plan.end_offset());
                    assert!(retry.offset() > plan.offset());
                }
                status.assert();
                blind_create.assert();
                if let Some(metadata) = metadata {
                    metadata.assert();
                }
            }
        }
    }

    #[test]
    fn secret_capabilities_and_debug_output_are_redacted() {
        static_assertions::assert_not_impl_any!(
            TransferDrive: Clone, serde::Serialize, fmt::Display
        );
        static_assertions::assert_not_impl_any!(
            UploadSession: Clone, serde::Serialize, fmt::Display
        );
        let server = Server::new();
        let drive = TransferDrive::for_test(&server, 1024);
        let debug = format!("{drive:?} {:?}", intent());
        assert!(!debug.contains("transfer-secret-token"));
        assert!(!debug.contains("operation_1"));
        assert!(!debug.contains("note.md"));
    }

    #[test]
    fn production_http_surface_is_statically_allowlisted() {
        let client_source = include_str!("client.rs");
        let transfer_source = include_str!("transfer.rs");
        for forbidden in [
            concat!(".", "patch", "("),
            concat!(".", "delete", "("),
            concat!(".", "request", "("),
        ] {
            assert!(
                !client_source.contains(forbidden),
                "client must not expose a generic or existing-item mutation builder"
            );
            assert!(
                !transfer_source.contains(forbidden),
                "transfer must not construct a generic or existing-item mutation builder"
            );
        }
        let post_builder = concat!(".", "post", "(");
        let put_builder = concat!(".", "put", "(");
        assert_eq!(client_source.matches(post_builder).count(), 1);
        assert_eq!(client_source.matches(put_builder).count(), 1);
        assert_eq!(transfer_source.matches(post_builder).count(), 0);
        assert_eq!(transfer_source.matches(put_builder).count(), 0);
    }

    #[test]
    fn sync_revision_conversion_is_canonical_checked_and_round_trips() {
        let download_intent =
            DownloadIntent::from_sync_revision("file_1", "root_1", SYNC_REVISION_2, SHA, 3)
                .unwrap();
        assert_eq!(download_intent.provider_version, "2");
        assert_eq!(download_intent.sync_revision, SYNC_REVISION_2);

        let max_revision = format!("{:064x}", u64::MAX);
        assert_eq!(
            provider_version_from_sync_revision(&max_revision).unwrap(),
            u64::MAX.to_string()
        );
        assert_eq!(
            sync_revision_from_provider_version(&u64::MAX.to_string()).unwrap(),
            max_revision
        );

        let overflow = format!("{}1{}", "0".repeat(47), "0".repeat(16));
        for malformed in [
            "2".to_owned(),
            SYNC_REVISION_2.replace('2', "A"),
            "g".repeat(64),
            overflow,
        ] {
            assert_eq!(
                DownloadIntent::from_sync_revision("file_1", "root_1", malformed, SHA, 3)
                    .unwrap_err()
                    .code(),
                ErrorCode::InvalidInput
            );
        }

        let file: TransferFile = serde_json::from_str(&complete_file_json()).unwrap();
        assert_eq!(
            remote_object(&file, &intent()).unwrap().sync_revision(),
            SYNC_REVISION_2
        );
    }

    #[test]
    fn reconcile_absence_captures_exact_marker_and_name_queries() {
        let mut server = Server::new();
        binding_mocks(&mut server, 2);
        let marker = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Regex("myvaultOperation.*operation_1".into()))
            .with_body(r#"{"files":[]}"#)
            .create();
        let name = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Regex("name.*note%2Emd|name.*note.md".into()))
            .with_body(r#"{"files":[]}"#)
            .create();
        let result = TransferDrive::for_test(&server, 1024)
            .reconcile_create(intent())
            .unwrap();
        assert!(matches!(result, CreateReconciliation::Absent(_)));
        marker.assert();
        name.assert();
    }

    #[test]
    fn post_create_verification_captures_unique_marker_name_and_exact_metadata() {
        let mut server = Server::new();
        binding_mocks(&mut server, 3);
        let marker = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("myvaultOperation.*operation_1".into()),
            ]))
            .with_body(format!(r#"{{"files":[{}]}}"#, complete_file_json()))
            .create();
        let name = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("name.*note%2Emd|name.*note.md".into()),
            ]))
            .with_body(format!(r#"{{"files":[{}]}}"#, complete_file_json()))
            .create();
        let metadata = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(complete_file_json())
            .create();
        let intent = intent();
        let created_file: TransferFile = serde_json::from_str(&complete_file_json()).unwrap();
        let created = remote_object(&created_file, &intent).unwrap();

        let verified = TransferDrive::for_test(&server, 1024)
            .verify_created_upload(&intent, &created)
            .unwrap();

        assert_eq!(verified, created);
        marker.assert();
        name.assert();
        metadata.assert();
    }

    #[test]
    fn post_create_verification_accepts_only_advancing_provider_versions() {
        let mut advancing = Server::new();
        binding_mocks(&mut advancing, 3);
        let marker = advancing
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("myvaultOperation.*operation_1".into()),
            ]))
            .with_body(format!(
                r#"{{"files":[{}]}}"#,
                complete_file_json_with_version(3)
            ))
            .create();
        let name = advancing
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("name.*note%2Emd|name.*note.md".into()),
            ]))
            .with_body(format!(
                r#"{{"files":[{}]}}"#,
                complete_file_json_with_version(3)
            ))
            .create();
        let metadata = advancing
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(complete_file_json_with_version(4))
            .create();
        let create_intent = intent();
        let created_file: TransferFile =
            serde_json::from_str(&complete_file_json_with_version(2)).unwrap();
        let created = remote_object(&created_file, &create_intent).unwrap();

        let verified = TransferDrive::for_test(&advancing, 1024)
            .verify_created_upload(&create_intent, &created)
            .unwrap();

        assert_eq!(verified.provider_version, "4");
        assert_eq!(verified.sync_revision, format!("{:064x}", 4));
        marker.assert();
        name.assert();
        metadata.assert();

        let mut regressing = Server::new();
        binding_mocks(&mut regressing, 3);
        regressing
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Regex("myvaultOperation.*operation_1".into()))
            .with_body(format!(
                r#"{{"files":[{}]}}"#,
                complete_file_json_with_version(3)
            ))
            .create();
        regressing
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Regex("name.*note%2Emd|name.*note.md".into()))
            .with_body(format!(
                r#"{{"files":[{}]}}"#,
                complete_file_json_with_version(3)
            ))
            .create();
        regressing
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(complete_file_json_with_version(2))
            .create();
        let created_file: TransferFile =
            serde_json::from_str(&complete_file_json_with_version(3)).unwrap();
        let created = remote_object(&created_file, &create_intent).unwrap();

        assert_eq!(
            TransferDrive::for_test(&regressing, 1024)
                .verify_created_upload(&create_intent, &created)
                .unwrap_err()
                .code(),
            ErrorCode::RevisionMismatch
        );
    }

    #[test]
    fn post_create_verification_rejects_concurrent_duplicate_marker() {
        let mut server = Server::new();
        binding_mocks(&mut server, 1);
        let duplicate = complete_file_json().replace("file_1", "file_2");
        let marker = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("myvaultOperation.*operation_1".into()),
            ]))
            .with_body(format!(
                r#"{{"files":[{},{}]}}"#,
                complete_file_json(),
                duplicate
            ))
            .create();
        let intent = intent();
        let created_file: TransferFile = serde_json::from_str(&complete_file_json()).unwrap();
        let created = remote_object(&created_file, &intent).unwrap();

        let error = TransferDrive::for_test(&server, 1024)
            .verify_created_upload(&intent, &created)
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::AmbiguousRemote);
        marker.assert();
    }

    #[test]
    fn post_create_verification_rejects_concurrent_duplicate_name() {
        let mut server = Server::new();
        binding_mocks(&mut server, 2);
        let marker = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("myvaultOperation.*operation_1".into()),
            ]))
            .with_body(format!(r#"{{"files":[{}]}}"#, complete_file_json()))
            .create();
        let duplicate = complete_file_json()
            .replace("file_1", "file_2")
            .replace("operation_1", "operation_2");
        let name = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageSize".into(), "2".into()),
                Matcher::Regex("name.*note%2Emd|name.*note.md".into()),
            ]))
            .with_body(format!(
                r#"{{"files":[{},{}]}}"#,
                complete_file_json(),
                duplicate
            ))
            .create();
        let intent = intent();
        let created_file: TransferFile = serde_json::from_str(&complete_file_json()).unwrap();
        let created = remote_object(&created_file, &intent).unwrap();

        let error = TransferDrive::for_test(&server, 1024)
            .verify_created_upload(&intent, &created)
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::AmbiguousRemote);
        marker.assert();
        name.assert();
    }

    #[test]
    fn existing_same_bytes_is_verified_and_different_bytes_need_reconcile() {
        for (body, expected_verified) in [
            (complete_file_json(), true),
            (complete_file_json().replace("ba7816", "aa7816"), false),
        ] {
            let mut server = Server::new();
            binding_mocks(&mut server, usize::from(expected_verified) + 1);
            let recheck = expected_verified.then(|| {
                server
                    .mock("GET", "/drive/v3/files/file_1")
                    .match_query(Matcher::Regex("fields".into()))
                    .with_body(body.clone())
                    .create()
            });
            let marker = server
                .mock("GET", "/drive/v3/files")
                .match_query(Matcher::Any)
                .with_body(format!(r#"{{"files":[{body}]}}"#))
                .create();
            let result = TransferDrive::for_test(&server, 1024)
                .reconcile_create(intent())
                .unwrap();
            let actual = matches!(&result, CreateReconciliation::VerifiedExisting(_));
            assert_eq!(
                actual, expected_verified,
                "unexpected reconciliation result: {result:?}"
            );
            marker.assert();
            if let Some(recheck) = recheck {
                recheck.assert();
            }
        }
    }

    #[test]
    fn preexisting_same_byte_name_is_a_verified_no_op_without_operation_marker() {
        let mut server = Server::new();
        binding_mocks(&mut server, 3);
        let marker = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Regex("myvaultOperation.*operation_1".into()))
            .with_body(r#"{"files":[]}"#)
            .create();
        let same_bytes = complete_file_json()
            .split(",\"appProperties\"")
            .next()
            .unwrap()
            .to_owned()
            + "}";
        let name = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Regex("name.*note%2Emd|name.*note.md".into()))
            .with_body(format!(r#"{{"files":[{same_bytes}]}}"#))
            .create();
        let recheck = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(same_bytes)
            .create();

        let result = TransferDrive::for_test(&server, 1024)
            .reconcile_create(intent())
            .unwrap();

        assert!(matches!(result, CreateReconciliation::VerifiedExisting(_)));
        marker.assert();
        name.assert();
        recheck.assert();
    }

    #[test]
    fn verified_existing_rechecks_binding_and_current_parent_after_query() {
        let mut server = Server::new();
        binding_mocks(&mut server, 2);
        let query = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Any)
            .with_body(format!(r#"{{"files":[{}]}}"#, complete_file_json()))
            .create();
        let moved = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(complete_file_json().replace("[\"root_1\"]", "[\"other_parent\"]"))
            .create();

        let result = TransferDrive::for_test(&server, 1024)
            .reconcile_create(intent())
            .unwrap();

        assert!(matches!(
            result,
            CreateReconciliation::NeedsReconcile(ReconcileReason::OperationMarkerMismatch)
        ));
        query.assert();
        moved.assert();
    }

    #[test]
    fn resumable_init_is_create_only_and_session_url_is_redacted() {
        let mut server = Server::new();
        binding_mocks(&mut server, 3);
        // Google encodes existing-item content updates, renames, moves, and
        // trash as PATCH on this exact resource. Capture it as forbidden.
        let existing_item_patch = server
            .mock("PATCH", "/drive/v3/files/file_1")
            .expect(0)
            .create();
        let existing_upload = server
            .mock("POST", "/upload/drive/v3/files/file_1")
            .expect(0)
            .create();
        let existing_trash_or_delete = server
            .mock("DELETE", "/drive/v3/files/file_1")
            .expect(0)
            .create();
        let permission_mutation = server
            .mock("POST", "/drive/v3/files/file_1/permissions")
            .expect(0)
            .create();
        server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Any)
            .with_body(r#"{"files":[]}"#)
            .expect(2)
            .create();
        let permit = match TransferDrive::for_test(&server, 1024)
            .reconcile_create(intent())
            .unwrap()
        {
            CreateReconciliation::Absent(value) => value,
            other => panic!("unexpected reconciliation: {other:?}"),
        };
        let location = format!(
            "{}/upload/drive/v3/files?upload_id=secret_session",
            server.url()
        );
        let init = server
            .mock("POST", "/upload/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("uploadType".into(), "resumable".into()),
                Matcher::UrlEncoded("supportsAllDrives".into(), "true".into()),
            ]))
            .match_header("x-upload-content-length", "3")
            .match_body(Matcher::Regex("myvaultOperation.*operation_1".into()))
            .with_status(200)
            .with_header("location", &location)
            .create();
        let session = TransferDrive::for_test(&server, 1024)
            .initiate_resumable_create(permit)
            .unwrap();
        let debug = format!("{session:?}");
        assert!(!debug.contains("secret_session"));
        assert!(!debug.contains("operation_1"));
        init.assert();
        existing_item_patch.assert();
        existing_upload.assert();
        existing_trash_or_delete.assert();
        permission_mutation.assert();
    }

    #[test]
    fn upload_validates_308_range_and_final_metadata() {
        let mut server = Server::new();
        binding_mocks(&mut server, 2);
        let drive = TransferDrive::for_test(&server, 1024);
        let mut session = UploadSession {
            uri: SecretString::from(format!(
                "{}/upload/drive/v3/files?upload_id=session",
                server.url()
            )),
            intent: CreateIntent::new(
                "root_1",
                "note.md",
                "text/markdown",
                "operation_1",
                "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721",
                6,
            )
            .unwrap(),
            next_offset: 0,
        };
        let final_body = complete_file_json()
            .replace(
                SHA,
                "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721",
            )
            .replace("\"3\"", "\"6\"");
        let recheck = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(final_body.clone())
            .create();
        let upload = server
            .mock("PUT", "/upload/drive/v3/files")
            .match_query(Matcher::Any)
            .match_header("content-range", "bytes 0-5/6")
            .match_body("abcdef")
            .with_status(200)
            .with_body(final_body)
            .create();
        let result = drive.upload_chunk(&mut session, b"abcdef").unwrap();
        assert!(matches!(result, UploadProgress::Complete(_)));
        assert_eq!(session.next_offset(), 6);
        upload.assert();
        recheck.assert();

        let mut malformed = Server::new();
        binding_mocks(&mut malformed, 1);
        let drive = TransferDrive::for_test(&malformed, 1024);
        let mut session = UploadSession {
            uri: SecretString::from(format!(
                "{}/upload/drive/v3/files?upload_id=session",
                malformed.url()
            )),
            intent: CreateIntent::new(
                "root_1",
                "blob.bin",
                "application/octet-stream",
                "operation_2",
                SHA,
                u64::try_from(CHUNK_ALIGNMENT * 2).unwrap(),
            )
            .unwrap(),
            next_offset: 0,
        };
        malformed
            .mock("PUT", "/upload/drive/v3/files")
            .match_query(Matcher::Any)
            .with_status(308)
            .with_header("range", "bytes=1-262143")
            .create();
        assert_eq!(
            drive
                .upload_chunk(&mut session, &vec![0; CHUNK_ALIGNMENT])
                .unwrap_err()
                .code(),
            ErrorCode::RangeRejected
        );
    }

    #[test]
    fn final_upload_response_accepts_only_advancing_provider_versions() {
        for (response_version, verified_version, expected) in [
            (2, 3, Ok(format!("{:064x}", 3))),
            (3, 2, Err(ErrorCode::RevisionMismatch)),
        ] {
            let mut server = Server::new();
            binding_mocks(&mut server, 2);
            let drive = TransferDrive::for_test(&server, 1024);
            let upload = server
                .mock("PUT", "/upload/drive/v3/files")
                .match_query(Matcher::Any)
                .with_status(200)
                .with_body(complete_file_json_with_version(response_version))
                .create();
            let recheck = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(Matcher::Regex("fields".into()))
                .with_body(complete_file_json_with_version(verified_version))
                .create();
            let mut session = UploadSession {
                uri: SecretString::from(format!(
                    "{}/upload/drive/v3/files?upload_id=session",
                    server.url()
                )),
                intent: intent(),
                next_offset: 0,
            };

            match expected {
                Ok(expected_revision) => {
                    let UploadProgress::Complete(remote) =
                        drive.upload_chunk(&mut session, b"abc").unwrap()
                    else {
                        panic!("final chunk must complete");
                    };
                    assert_eq!(remote.sync_revision(), expected_revision);
                    assert_eq!(session.next_offset(), 3);
                }
                Err(expected_code) => {
                    assert_eq!(
                        drive.upload_chunk(&mut session, b"abc").unwrap_err().code(),
                        expected_code
                    );
                    assert_eq!(session.next_offset(), 0);
                }
            }
            upload.assert();
            recheck.assert();
        }
    }

    #[test]
    fn final_upload_response_rechecks_revision_and_parent_before_complete() {
        let mut server = Server::new();
        binding_mocks(&mut server, 2);
        let drive = TransferDrive::for_test(&server, 1024);
        let upload = server
            .mock("PUT", "/upload/drive/v3/files")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_body(complete_file_json())
            .create();
        let moved = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(complete_file_json().replace("[\"root_1\"]", "[\"other_parent\"]"))
            .create();
        let mut session = UploadSession {
            uri: SecretString::from(format!(
                "{}/upload/drive/v3/files?upload_id=session",
                server.url()
            )),
            intent: intent(),
            next_offset: 0,
        };

        assert_eq!(
            drive.upload_chunk(&mut session, b"abc").unwrap_err().code(),
            ErrorCode::RevisionMismatch
        );
        assert_eq!(session.next_offset(), 0);
        upload.assert();
        moved.assert();
    }

    #[test]
    fn status_query_accepts_only_a_canonical_advancing_308_range() {
        let mut server = Server::new();
        binding_mocks(&mut server, 1);
        let drive = TransferDrive::for_test(&server, 1024);
        let status = server
            .mock("PUT", "/upload/drive/v3/files")
            .match_query(Matcher::Any)
            .match_header("content-range", "bytes */3")
            .with_status(308)
            .with_header("range", "bytes=0-2")
            .create();
        let mut session = UploadSession {
            uri: SecretString::from(format!(
                "{}/upload/drive/v3/files?upload_id=session",
                server.url()
            )),
            intent: intent(),
            next_offset: 0,
        };

        assert_eq!(
            drive.query_upload_status(&mut session).unwrap(),
            UploadProgress::InProgress { next_offset: 3 }
        );
        assert_eq!(session.next_offset(), 3);
        status.assert();

        let mut regression = Server::new();
        binding_mocks(&mut regression, 1);
        let drive = TransferDrive::for_test(&regression, 1024);
        regression
            .mock("PUT", "/upload/drive/v3/files")
            .match_query(Matcher::Any)
            .with_status(308)
            .with_header("range", "bytes=0-1")
            .create();
        let mut session = UploadSession {
            uri: SecretString::from(format!(
                "{}/upload/drive/v3/files?upload_id=session",
                regression.url()
            )),
            intent: intent(),
            next_offset: 3,
        };
        assert_eq!(
            drive.query_upload_status(&mut session).unwrap_err().code(),
            ErrorCode::RangeRejected
        );
    }

    #[test]
    fn inspect_download_candidate_returns_bounded_blob_metadata_without_content_read() {
        let mut server = Server::new();
        binding_mocks(&mut server, 1);
        let metadata = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(format!(
                r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}"}}"#
            ))
            .create();
        let media = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
            .expect(0)
            .create();

        let candidate = TransferDrive::for_test(&server, 1024)
            .inspect_download_candidate("file_1", "root_1", "note.md", SYNC_REVISION_2)
            .unwrap();

        assert_eq!(candidate.file_id(), "file_1");
        assert_eq!(candidate.sync_revision(), SYNC_REVISION_2);
        assert_eq!(candidate.sha256(), SHA);
        assert_eq!(candidate.size(), 3);
        metadata.assert();
        media.assert();
    }

    #[test]
    fn inspect_download_candidate_fails_closed_for_unsafe_metadata() {
        let cases = [
            (
                format!(
                    r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["other_parent"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}"}}"#
                ),
                1024,
                ErrorCode::MalformedResponse,
            ),
            (
                r#"{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3"}"#.to_owned(),
                1024,
                ErrorCode::MalformedResponse,
            ),
            (
                format!(
                    r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","sha256Checksum":"{SHA}"}}"#
                ),
                1024,
                ErrorCode::MalformedResponse,
            ),
            (
                format!(
                    r#"{{"id":"file_1","name":"Doc","mimeType":"application/vnd.google-apps.document","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}"}}"#
                ),
                1024,
                ErrorCode::MalformedResponse,
            ),
            (
                format!(
                    r#"{{"id":"file_1","name":"large.bin","mimeType":"application/octet-stream","parents":["root_1"],"trashed":false,"version":"2","size":"1025","sha256Checksum":"{SHA}"}}"#
                ),
                1024,
                ErrorCode::ResponseTooLarge,
            ),
        ];

        for (body, max_blob_bytes, expected) in cases {
            let mut server = Server::new();
            binding_mocks(&mut server, 1);
            let metadata = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(Matcher::Regex("fields".into()))
                .with_body(body)
                .create();
            let media = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
                .expect(0)
                .create();

            let error = TransferDrive::for_test(&server, max_blob_bytes)
                .inspect_download_candidate("file_1", "root_1", "note.md", SYNC_REVISION_2)
                .unwrap_err();

            assert_eq!(error.code(), expected);
            metadata.assert();
            media.assert();
        }
    }

    #[test]
    fn inspect_download_candidate_rejects_name_and_revision_mismatch_without_media_get() {
        let revision_3 = "0000000000000000000000000000000000000000000000000000000000000003";
        for (expected_name, expected_revision) in
            [("renamed.md", SYNC_REVISION_2), ("note.md", revision_3)]
        {
            let mut server = Server::new();
            binding_mocks(&mut server, 1);
            let metadata = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(Matcher::Regex("fields".into()))
                .with_body(format!(
                    r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}"}}"#
                ))
                .create();
            let media = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
                .expect(0)
                .create();

            let error = TransferDrive::for_test(&server, 1024)
                .inspect_download_candidate("file_1", "root_1", expected_name, expected_revision)
                .unwrap_err();

            assert_eq!(error.code(), ErrorCode::RevisionMismatch);
            metadata.assert();
            media.assert();
        }
    }

    #[test]
    fn inspect_download_candidate_rejects_malformed_expected_identity_before_network() {
        let server = Server::new();
        let drive = TransferDrive::for_test(&server, 1024);
        for (name, revision) in [
            ("../note.md", SYNC_REVISION_2),
            ("note.md", "2"),
            (
                "note.md",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ),
        ] {
            assert_eq!(
                drive
                    .inspect_download_candidate("file_1", "root_1", name, revision)
                    .unwrap_err()
                    .code(),
                ErrorCode::InvalidInput
            );
        }
    }

    #[test]
    fn metadata_only_download_verification_returns_current_evidence_without_media_get() {
        let mut server = Server::new();
        binding_mocks(&mut server, 1);
        let metadata = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(format!(
                r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}"}}"#
            ))
            .create();
        let media = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
            .expect(0)
            .create();

        let verified = TransferDrive::for_test(&server, 1024)
            .verify_download(
                &DownloadIntent::from_sync_revision("file_1", "root_1", SYNC_REVISION_2, SHA, 3)
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(verified.file_id(), "file_1");
        assert_eq!(verified.sync_revision(), SYNC_REVISION_2);
        assert_eq!(verified.sha256(), SHA);
        assert_eq!(verified.size(), 3);
        metadata.assert();
        media.assert();
    }

    #[test]
    fn metadata_only_download_verification_rejects_wrong_revision_and_parent() {
        for (metadata, expected) in [
            (
                format!(
                    r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"3","size":"3","sha256Checksum":"{SHA}"}}"#
                ),
                ErrorCode::RevisionMismatch,
            ),
            (
                format!(
                    r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["other_parent"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}"}}"#
                ),
                ErrorCode::MalformedResponse,
            ),
        ] {
            let mut server = Server::new();
            binding_mocks(&mut server, 1);
            let metadata_request = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(Matcher::Regex("fields".into()))
                .with_body(metadata)
                .create();
            let media = server
                .mock("GET", "/drive/v3/files/file_1")
                .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
                .expect(0)
                .create();

            let error = TransferDrive::for_test(&server, 1024)
                .verify_download(
                    &DownloadIntent::from_sync_revision(
                        "file_1",
                        "root_1",
                        SYNC_REVISION_2,
                        SHA,
                        3,
                    )
                    .unwrap(),
                )
                .unwrap_err();

            assert_eq!(error.code(), expected);
            metadata_request.assert();
            media.assert();
        }
    }

    #[test]
    fn blob_download_streams_and_verifies_hash_size_and_revision_twice() {
        let mut server = Server::new();
        binding_mocks(&mut server, 2);
        let metadata = format!(
            r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}"}}"#
        );
        server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(metadata)
            .expect(2)
            .create();
        let media = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
            .with_body("abc")
            .create();
        let mut output = Vec::new();
        let verified = TransferDrive::for_test(&server, 1024)
            .download_blob_to(
                &DownloadIntent::from_sync_revision("file_1", "root_1", SYNC_REVISION_2, SHA, 3)
                    .unwrap(),
                &mut output,
            )
            .unwrap();
        assert_eq!(output, b"abc");
        assert_eq!(verified.sha256(), SHA);
        assert_eq!(verified.sync_revision(), SYNC_REVISION_2);
        media.assert();
    }

    #[test]
    fn blob_download_rejects_hash_mismatch_before_a_second_metadata_read() {
        let mut server = Server::new();
        binding_mocks(&mut server, 1);
        let metadata = format!(
            r#"{{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2","size":"3","sha256Checksum":"{SHA}"}}"#
        );
        let metadata_mock = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::Regex("fields".into()))
            .with_body(metadata)
            .create();
        let media = server
            .mock("GET", "/drive/v3/files/file_1")
            .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
            .with_body("abd")
            .create();
        let mut output = Vec::new();
        let error = TransferDrive::for_test(&server, 1024)
            .download_blob_to(
                &DownloadIntent::from_sync_revision("file_1", "root_1", SYNC_REVISION_2, SHA, 3)
                    .unwrap(),
                &mut output,
            )
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::HashMismatch);
        assert_eq!(output, b"abd");
        metadata_mock.assert();
        media.assert();
    }

    #[test]
    fn malformed_final_upload_metadata_is_rejected_without_provider_body() {
        let mut server = Server::new();
        binding_mocks(&mut server, 1);
        let drive = TransferDrive::for_test(&server, 1024);
        let final_response = server
            .mock("PUT", "/upload/drive/v3/files")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_body("{not-json transfer-secret-token}")
            .create();
        let mut session = UploadSession {
            uri: SecretString::from(format!(
                "{}/upload/drive/v3/files?upload_id=secret_session",
                server.url()
            )),
            intent: intent(),
            next_offset: 0,
        };

        let error = drive.upload_chunk(&mut session, b"abc").unwrap_err();
        assert_eq!(error.code(), ErrorCode::MalformedResponse);
        assert!(!format!("{error:?} {error}").contains("transfer-secret-token"));
        assert!(!format!("{error:?} {error}").contains("secret_session"));
        final_response.assert();
    }

    #[test]
    fn stalled_session_response_maps_to_bounded_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });
        let origin = format!("http://{address}");
        let drive = TransferDrive::build(
            AccessToken::new("transfer-secret-token"),
            Url::parse(&format!("{origin}/drive/v3/")).unwrap(),
            Url::parse(&format!("{origin}/upload/drive/v3/")).unwrap(),
            "account_1".into(),
            "root_1".into(),
            1024,
            Duration::from_millis(20),
            false,
        )
        .unwrap();
        let mut session = UploadSession {
            uri: SecretString::from(format!(
                "{origin}/upload/drive/v3/files?upload_id=secret_session"
            )),
            intent: intent(),
            next_offset: 0,
        };

        let error = drive.query_upload_status(&mut session).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Timeout);
        assert!(!format!("{error:?} {error}").contains("secret_session"));
        server.join().unwrap();
    }

    #[test]
    fn redirects_statuses_timeouts_and_hash_mismatch_are_typed_and_redacted() {
        for (status, header, body, expected) in [
            (
                401,
                None,
                "transfer-secret-token provider body",
                ErrorCode::Unauthorized,
            ),
            (
                403,
                None,
                "transfer-secret-token provider body",
                ErrorCode::Forbidden,
            ),
            (
                403,
                Some("12"),
                r#"{"token":"transfer-secret-token","error":{"errors":[{"reason":"permissionDenied"}]}}"#,
                ErrorCode::Forbidden,
            ),
            (
                403,
                Some("12"),
                r#"{"token":"transfer-secret-token","error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#,
                ErrorCode::Forbidden,
            ),
            (
                404,
                None,
                "transfer-secret-token provider body",
                ErrorCode::SessionExpired,
            ),
            (
                429,
                Some("9"),
                "transfer-secret-token provider body",
                ErrorCode::RateLimited,
            ),
            (
                503,
                None,
                "transfer-secret-token provider body",
                ErrorCode::TransientProvider,
            ),
        ] {
            let mut server = Server::new();
            binding_mocks(&mut server, 1);
            let drive = TransferDrive::for_test(&server, 1024);
            let mut mock = server
                .mock("PUT", "/upload/drive/v3/files")
                .match_query(Matcher::Any)
                .with_status(status)
                .with_body(body);
            if let Some(value) = header {
                mock = mock.with_header("retry-after", value);
            }
            mock.create();
            let mut session = UploadSession {
                uri: SecretString::from(format!(
                    "{}/upload/drive/v3/files?upload_id=secret_session",
                    server.url()
                )),
                intent: intent(),
                next_offset: 0,
            };
            let error = drive.query_upload_status(&mut session).unwrap_err();
            assert_eq!(error.code(), expected);
            if status == 403 {
                assert_eq!(error.retry_after_seconds(), None);
            }
            if status == 429 {
                assert_eq!(error.retry_after_seconds(), Some(9));
            }
            assert!(!format!("{error:?} {error}").contains("provider body"));
            assert!(!format!("{error:?} {error}").contains("transfer-secret-token"));
            assert!(!format!("{error:?} {error}").contains("secret_session"));
        }

        let mut server = Server::new();
        binding_mocks(&mut server, 1);
        let drive = TransferDrive::for_test(&server, 1024);
        server
            .mock("PUT", "/upload/drive/v3/files")
            .match_query(Matcher::Any)
            .with_status(302)
            .with_header("location", "https://attacker.invalid/steal")
            .create();
        let mut session = UploadSession {
            uri: SecretString::from(format!(
                "{}/upload/drive/v3/files?upload_id=secret_session",
                server.url()
            )),
            intent: intent(),
            next_offset: 0,
        };
        assert_eq!(
            drive.query_upload_status(&mut session).unwrap_err().code(),
            ErrorCode::RedirectRejected
        );
    }

    #[test]
    fn session_origin_and_path_are_pinned() {
        let server = Server::new();
        let drive = TransferDrive::for_test(&server, 1024);
        for uri in [
            "https://attacker.invalid/upload/drive/v3/files?upload_id=x",
            &format!("{}/drive/v3/files?upload_id=x", server.url()),
            &format!("{}/upload/drive/v3/files", server.url()),
        ] {
            let mut session = UploadSession {
                uri: SecretString::from(uri.to_owned()),
                intent: intent(),
                next_offset: 0,
            };
            assert_eq!(
                drive.query_upload_status(&mut session).unwrap_err().code(),
                ErrorCode::UnexpectedOrigin
            );
        }
    }
}
