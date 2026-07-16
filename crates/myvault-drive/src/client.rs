use crate::{
    model::{AboutResponse, StartTokenResponse},
    AccessToken, AccountIdentity, ChangesPage, Error, ErrorCode, FilePage, RemoteFile, Result,
    VerifiedRoot, FOLDER_MIME_TYPE,
};
use myvault_sync_engine::{
    ChangesPage as EngineChangesPage, DriveClient, RemoteChange, RemoteEntry, RemoteError,
    ScanPage, ScanRequest, VerifiedRemoteBinding,
};
use reqwest::{
    blocking::{Client, Response},
    header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE},
    StatusCode, Url,
};
use serde::de::DeserializeOwned;
use std::{fmt, io::Read, time::Duration};

const GOOGLE_API_BASE: &str = "https://www.googleapis.com/drive/v3/";
const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const FILE_FIELDS: &str =
    "id,name,mimeType,parents,trashed,version,md5Checksum,sha1Checksum,sha256Checksum";
const MAX_CHANGE_ANCESTRY_DEPTH: usize = 64;

/// One raw Drive change resolved against the exact bound root without reading
/// file content or accepting display-path guesses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedDriveChange {
    Inside(RemoteEntry),
    OutsideBoundRoot,
    Removed { file_id: String },
}

pub struct ReadOnlyDrive {
    // Keep the raw HTTP client private. The only write-shaped requests in this
    // crate must pass through the narrowly named, URL-checked helpers below.
    // In particular, this prevents a sibling module from constructing an
    // existing-item PATCH/DELETE/permission request with this capability.
    client: Client,
    token: AccessToken,
    api_base: Url,
    max_response_bytes: usize,
}

impl fmt::Debug for ReadOnlyDrive {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadOnlyDrive")
            .field("api_origin", &self.api_base.origin().ascii_serialization())
            .field("token", &"[REDACTED]")
            .field("max_response_bytes", &self.max_response_bytes)
            .finish_non_exhaustive()
    }
}

impl ReadOnlyDrive {
    pub(crate) fn api_origin(&self) -> String {
        self.api_base.origin().ascii_serialization()
    }

    /// Constructs the production client pinned to Google's Drive v3 origin.
    ///
    /// The adapter assumes the caller obtained a token with
    /// `https://www.googleapis.com/auth/drive` and does not
    /// broaden or refresh that authorization.
    ///
    /// # Errors
    /// Returns a redacted transport classification if the TLS client cannot be
    /// initialized.
    pub fn google(token: AccessToken) -> Result<Self> {
        let api_base =
            Url::parse(GOOGLE_API_BASE).map_err(|_| Error::new(ErrorCode::UnexpectedOrigin))?;
        Self::build(
            token,
            api_base,
            DEFAULT_MAX_RESPONSE_BYTES,
            REQUEST_TIMEOUT,
            true,
        )
    }

    pub(crate) fn build(
        token: AccessToken,
        api_base: Url,
        max_response_bytes: usize,
        request_timeout: Duration,
        https_only: bool,
    ) -> Result<Self> {
        if max_response_bytes == 0
            || !api_base.path().ends_with('/')
            || api_base.query().is_some()
            || api_base.fragment().is_some()
            || !api_base.username().is_empty()
            || api_base.password().is_some()
            || token.expose().is_empty()
            || token.expose().len() > 16 * 1024
            || token.expose().chars().any(char::is_control)
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let client = Client::builder()
            .https_only(https_only)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::new(ErrorCode::Transport))?;
        Ok(Self {
            client,
            token,
            api_base,
            max_response_bytes,
        })
    }

    #[cfg(test)]
    fn for_test(base: &str, token: AccessToken, max_response_bytes: usize) -> Result<Self> {
        let mut api_base = Url::parse(base).map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        if !api_base.path().ends_with('/') {
            api_base.set_path(&format!("{}/", api_base.path()));
        }
        Self::build(token, api_base, max_response_bytes, REQUEST_TIMEOUT, false)
    }

    /// Constructs a GET-only client for cross-crate integration tests.
    /// This constructor is deliberately absent from production builds.
    ///
    /// # Errors
    /// Rejects malformed origins, bounds, tokens, or client setup.
    #[cfg(feature = "test-support")]
    pub fn for_test_origin(api_base: &str, max_response_bytes: usize) -> Result<Self> {
        let mut api_base = Url::parse(api_base).map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        if !api_base.path().ends_with('/') {
            api_base.set_path(&format!("{}/", api_base.path()));
        }
        Self::build(
            AccessToken::new("integration-test-token"),
            api_base,
            max_response_bytes,
            REQUEST_TIMEOUT,
            false,
        )
    }

    #[cfg(test)]
    fn for_test_with_timeout(
        base: &str,
        token: AccessToken,
        max_response_bytes: usize,
        request_timeout: Duration,
    ) -> Result<Self> {
        let mut api_base = Url::parse(base).map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        if !api_base.path().ends_with('/') {
            api_base.set_path(&format!("{}/", api_base.path()));
        }
        Self::build(token, api_base, max_response_bytes, request_timeout, false)
    }

    /// Returns Google's opaque, provider-stable account permission id.
    ///
    /// # Errors
    /// Returns only a stable redacted classification.
    pub fn account_identity(&self) -> Result<AccountIdentity> {
        let url = self.endpoint("about")?;
        let response: AboutResponse = self.get_json(url, &[("fields", "user(permissionId)")])?;
        validate_identifier(&response.user.permission_id)
            .map_err(|_| Error::new(ErrorCode::InvalidAccount))?;
        Ok(response.user)
    }

    /// Proves the current provider account and exact live folder id match the
    /// requested binding before persistence.
    ///
    /// # Errors
    /// Returns a typed account/root mismatch or another stable redacted
    /// provider classification.
    pub fn verify_binding(
        &self,
        requested_account_id: &str,
        requested_root_id: &str,
    ) -> Result<VerifiedRemoteBinding> {
        validate_identifier(requested_account_id)?;
        validate_identifier(requested_root_id)?;
        let observed_account = self.account_identity()?;
        if observed_account.permission_id != requested_account_id {
            return Err(Error::new(ErrorCode::InvalidAccount));
        }
        let observed_root = self.verify_root(requested_root_id)?;
        VerifiedRemoteBinding::new(
            requested_account_id,
            requested_root_id,
            observed_account.permission_id,
            observed_root.id,
        )
        .map_err(|_| Error::new(ErrorCode::InvalidRoot))
    }

    /// Fetches metadata for one exact provider file id.
    ///
    /// # Errors
    /// Returns only a stable redacted classification.
    pub fn file_metadata(&self, file_id: &str) -> Result<RemoteFile> {
        validate_identifier(file_id)?;
        let url = self.endpoint(&format!("files/{file_id}"))?;
        let file: RemoteFile = self.get_json(
            url,
            &[("fields", FILE_FIELDS), ("supportsAllDrives", "true")],
        )?;
        if file.id != file_id {
            return Err(Error::new(ErrorCode::MalformedResponse));
        }
        validate_file(&file)?;
        Ok(file)
    }

    /// Verifies an exact id refers to a live folder before it is persisted as a
    /// root binding.
    ///
    /// # Errors
    /// Returns `InvalidRoot` for trashed/non-folder/malformed root metadata.
    pub fn verify_root(&self, file_id: &str) -> Result<VerifiedRoot> {
        let file = self.file_metadata(file_id)?;
        if file.trashed || file.mime_type != FOLDER_MIME_TYPE {
            return Err(Error::new(ErrorCode::InvalidRoot));
        }
        let version = file
            .version
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::new(ErrorCode::InvalidRoot))?;
        version
            .parse::<u64>()
            .map_err(|_| Error::new(ErrorCode::InvalidRoot))?;
        Ok(VerifiedRoot {
            id: file.id,
            name: file.name,
            version,
        })
    }

    /// Lists one page of direct children below an exact folder id.
    ///
    /// Duplicate names are preserved as separate `RemoteFile` values. The
    /// caller owns recursion and must durably enqueue every returned folder.
    ///
    /// # Errors
    /// Returns only a stable redacted classification.
    pub fn list_children_page(
        &self,
        parent_id: &str,
        page_token: Option<&str>,
    ) -> Result<FilePage> {
        validate_identifier(parent_id)?;
        if let Some(token) = page_token {
            validate_page_token(token)?;
        }
        let url = self.endpoint("files")?;
        let query = format!("'{parent_id}' in parents and trashed = false");
        let fields = format!("nextPageToken,incompleteSearch,files({FILE_FIELDS})");
        let mut parameters = vec![
            ("q", query.as_str()),
            ("fields", fields.as_str()),
            ("pageSize", "1000"),
            ("spaces", "drive"),
            ("corpora", "user"),
            ("includeItemsFromAllDrives", "true"),
            ("supportsAllDrives", "true"),
        ];
        if let Some(token) = page_token {
            parameters.push(("pageToken", token));
        }
        let page: FilePage = self.get_json(url, &parameters)?;
        if page.incomplete_search {
            return Err(Error::new(ErrorCode::IncompleteSearch));
        }
        validate_optional_page_token(page.next_page_token.as_deref())?;
        for file in &page.files {
            validate_file(file)?;
            // Google accepts the literal `root` as a request alias but returns
            // the root folder's opaque id in each child's `parents` field.
            // Keep the response fail-closed by requiring exactly one valid
            // provider-returned parent; exact ids must still match.
            let parent_matches = if parent_id == "root" {
                file.parents.len() == 1
            } else {
                file.parents.iter().any(|parent| parent == parent_id)
            };
            if !parent_matches {
                return Err(Error::new(ErrorCode::MalformedResponse));
            }
        }
        Ok(page)
    }

    /// Captures the Changes cursor that must precede an initial scan.
    ///
    /// # Errors
    /// Returns only a stable redacted classification.
    pub fn start_page_token(&self) -> Result<String> {
        let url = self.endpoint("changes/startPageToken")?;
        let value: StartTokenResponse = self.get_json(url, &[("supportsAllDrives", "true")])?;
        validate_page_token(&value.start_page_token)?;
        Ok(value.start_page_token)
    }

    /// Fetches exactly one Changes page, including file metadata needed by
    /// integration to recompute ancestry and canonical paths.
    ///
    /// # Errors
    /// Returns `CursorExpired` for HTTP 410 and otherwise only a stable redacted
    /// classification.
    pub fn changes_page(&self, page_token: &str) -> Result<ChangesPage> {
        validate_page_token(page_token)?;
        let url = self.endpoint("changes")?;
        let fields =
            format!("nextPageToken,newStartPageToken,changes(fileId,removed,file({FILE_FIELDS}))");
        let page: ChangesPage = self.get_json(
            url,
            &[
                ("pageToken", page_token),
                ("fields", fields.as_str()),
                ("pageSize", "1000"),
                ("spaces", "drive"),
                ("includeRemoved", "true"),
                ("includeItemsFromAllDrives", "true"),
                ("supportsAllDrives", "true"),
            ],
        )?;
        validate_changes_page(&page)?;
        Ok(page)
    }

    /// Resolves one raw Changes item to a canonical path below an exact root.
    ///
    /// Every ancestry hop is an exact metadata GET with a single-parent,
    /// live-folder check. Google-native files are rejected rather than exported
    /// or transformed. A removed item intentionally carries only its opaque ID;
    /// the durable store decides whether that ID belonged to the bound root.
    ///
    /// # Errors
    /// Returns a redacted ambiguity or malformed-response classification when
    /// ancestry, MIME, identity, or portable path evidence is not exact.
    pub fn resolve_change_below_root(
        &self,
        root_id: &str,
        change: &crate::Change,
    ) -> Result<ResolvedDriveChange> {
        validate_identifier(root_id)?;
        validate_identifier(&change.file_id)
            .map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
        if change.removed || change.file.as_ref().is_some_and(|file| file.trashed) {
            return Ok(ResolvedDriveChange::Removed {
                file_id: change.file_id.clone(),
            });
        }
        let file = change
            .file
            .as_ref()
            .filter(|file| file.id == change.file_id)
            .ok_or_else(|| Error::new(ErrorCode::MalformedResponse))?;
        validate_file(file)?;
        if file.id == root_id {
            return Ok(ResolvedDriveChange::OutsideBoundRoot);
        }
        if file.mime_type.starts_with("application/vnd.google-apps.") && !file.is_folder() {
            return Err(Error::new(ErrorCode::CursorAmbiguous));
        }
        if file.parents.len() != 1 {
            return Err(Error::new(ErrorCode::CursorAmbiguous));
        }

        let mut names = vec![file.name.clone()];
        let mut parent_id = file.parents[0].clone();
        let mut visited = std::collections::BTreeSet::from([file.id.clone()]);
        for _ in 0..MAX_CHANGE_ANCESTRY_DEPTH {
            if parent_id == root_id {
                names.reverse();
                return file
                    .to_remote_entry(names.join("/"))
                    .map(ResolvedDriveChange::Inside);
            }
            if !visited.insert(parent_id.clone()) {
                return Err(Error::new(ErrorCode::CursorAmbiguous));
            }
            let parent = self.file_metadata(&parent_id).map_err(|error| {
                if error.code() == ErrorCode::NotFound {
                    Error::new(ErrorCode::CursorAmbiguous)
                } else {
                    error
                }
            })?;
            if parent.trashed || !parent.is_folder() {
                return Err(Error::new(ErrorCode::CursorAmbiguous));
            }
            names.push(parent.name);
            match parent.parents.as_slice() {
                [next] => parent_id.clone_from(next),
                [] => return Ok(ResolvedDriveChange::OutsideBoundRoot),
                _ => return Err(Error::new(ErrorCode::CursorAmbiguous)),
            }
        }
        Err(Error::new(ErrorCode::CursorAmbiguous))
    }

    pub(crate) fn endpoint(&self, relative: &str) -> Result<Url> {
        if relative.is_empty() || relative.starts_with('/') || relative.contains(['?', '#']) {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let url = self
            .api_base
            .join(relative)
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        if url.origin() != self.api_base.origin() {
            return Err(Error::new(ErrorCode::UnexpectedOrigin));
        }
        Ok(url)
    }

    pub(crate) fn get_json<T: DeserializeOwned>(
        &self,
        url: Url,
        query: &[(&str, &str)],
    ) -> Result<T> {
        if url.origin() != self.api_base.origin() {
            return Err(Error::new(ErrorCode::UnexpectedOrigin));
        }
        let authorization = format!("Bearer {}", self.token.expose());
        let response = self
            .client
            .get(url)
            .query(query)
            .header(AUTHORIZATION, authorization)
            .send()
            .map_err(|error| map_transport_error(&error))?;
        self.decode(response)
    }

    /// Starts the one allowlisted provider mutation: a create-only resumable
    /// upload to the collection endpoint. An existing provider id cannot be
    /// expressed by this URL shape.
    pub(crate) fn post_resumable_create(
        &self,
        url: Url,
        fields: &str,
        mime_type: &str,
        size: u64,
        body: Vec<u8>,
    ) -> Result<Response> {
        self.ensure_create_only_upload_url(&url)?;
        self.client
            .post(url)
            .query(&[
                ("uploadType", "resumable"),
                ("supportsAllDrives", "true"),
                ("fields", fields),
            ])
            .header(AUTHORIZATION, self.authorization())
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .header("X-Upload-Content-Type", mime_type)
            .header("X-Upload-Content-Length", size)
            .body(body)
            .send()
            .map_err(|error| map_transfer_transport_error(&error))
    }

    /// Sends data only to a held resumable-upload session. The session URL is
    /// constrained to the create collection endpoint plus a bounded upload id;
    /// it cannot name an existing Drive item.
    pub(crate) fn put_resumable_session(
        &self,
        url: Url,
        body: Vec<u8>,
        content_range: &str,
    ) -> Result<Response> {
        self.ensure_resumable_session_url(&url)?;
        self.client
            .put(url)
            .header(AUTHORIZATION, self.authorization())
            .header(CONTENT_LENGTH, body.len())
            .header(CONTENT_RANGE, content_range)
            .body(body)
            .send()
            .map_err(|error| map_transfer_transport_error(&error))
    }

    /// Fetches media only for a validated exact file id. This is intentionally
    /// separate from metadata GETs so it cannot become a generic request API.
    pub(crate) fn get_media(&self, file_id: &str) -> Result<Response> {
        validate_identifier(file_id)?;
        let url = self.endpoint(&format!("files/{file_id}"))?;
        self.client
            .get(url)
            .query(&[("alt", "media"), ("supportsAllDrives", "true")])
            .header(AUTHORIZATION, self.authorization())
            .send()
            .map_err(|error| map_transfer_transport_error(&error))
    }

    fn authorization(&self) -> String {
        format!("Bearer {}", self.token.expose())
    }

    fn ensure_create_only_upload_url(&self, url: &Url) -> Result<()> {
        if !self.is_upload_collection_url(url) || url.query().is_some() {
            return Err(Error::new(ErrorCode::UnexpectedOrigin));
        }
        Ok(())
    }

    fn ensure_resumable_session_url(&self, url: &Url) -> Result<()> {
        if !self.is_upload_collection_url(url)
            || !url
                .query_pairs()
                .any(|(key, value)| key == "upload_id" && !value.is_empty() && value.len() <= 4096)
        {
            return Err(Error::new(ErrorCode::UnexpectedOrigin));
        }
        Ok(())
    }

    fn is_upload_collection_url(&self, url: &Url) -> bool {
        url.origin() == self.api_base.origin()
            && url.path() == "/upload/drive/v3/files"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    }

    fn decode<T: DeserializeOwned>(&self, mut response: Response) -> Result<T> {
        let status = response.status();
        if status.is_redirection() {
            return Err(Error::new(ErrorCode::RedirectRejected));
        }
        if !status.is_success() {
            return Err(Error::new(map_status(status)));
        }
        let limit = u64::try_from(self.max_response_bytes)
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > limit)
        {
            return Err(Error::new(ErrorCode::ResponseTooLarge));
        }
        let mut bytes = Vec::with_capacity(self.max_response_bytes.min(64 * 1024));
        response
            .by_ref()
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::new(ErrorCode::Transport))?;
        if bytes.len() > self.max_response_bytes {
            return Err(Error::new(ErrorCode::ResponseTooLarge));
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::new(ErrorCode::MalformedResponse))
    }
}

impl DriveClient for ReadOnlyDrive {
    fn get_start_page_token(&mut self) -> std::result::Result<String, RemoteError> {
        self.start_page_token().map_err(Error::to_remote_error)
    }

    fn scan_folder_page(
        &mut self,
        request: &ScanRequest,
    ) -> std::result::Result<ScanPage, RemoteError> {
        let page = self
            .list_children_page(&request.folder_id, request.page_token.as_deref())
            .map_err(Error::to_remote_error)?;
        let entries = page
            .files
            .iter()
            .map(|file| {
                let path = if request.folder_path.is_empty() {
                    file.name.clone()
                } else {
                    format!("{}/{}", request.folder_path, file.name)
                };
                file.to_remote_entry(path).map_err(Error::to_remote_error)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ScanPage {
            entries,
            next_page_token: page.next_page_token,
        })
    }

    fn changes_page(
        &mut self,
        page_token: &str,
    ) -> std::result::Result<EngineChangesPage, RemoteError> {
        let page = ReadOnlyDrive::changes_page(self, page_token).map_err(Error::to_remote_error)?;
        let mut changes = Vec::with_capacity(page.changes.len());
        for change in page.changes {
            if change.removed || change.file.as_ref().is_some_and(|file| file.trashed) {
                changes.push(RemoteChange::Removed {
                    file_id: change.file_id,
                });
            } else {
                // The trait has no store/path resolver. A folder move or upsert
                // therefore requires a fresh frontier scan to avoid stale paths.
                return Err(Error::new(ErrorCode::CursorAmbiguous).to_remote_error());
            }
        }
        Ok(EngineChangesPage {
            changes,
            next_page_token: page.next_page_token,
            new_start_page_token: page.new_start_page_token,
        })
    }
}

fn validate_file(file: &RemoteFile) -> Result<()> {
    validate_identifier(&file.id)?;
    if file.name.is_empty() || file.name.len() > 1024 || file.name.chars().any(char::is_control) {
        return Err(Error::new(ErrorCode::MalformedResponse));
    }
    if file.mime_type.is_empty() || file.mime_type.len() > 255 {
        return Err(Error::new(ErrorCode::MalformedResponse));
    }
    if file.parents.len() > 1 {
        return Err(Error::new(ErrorCode::MalformedResponse));
    }
    for parent in &file.parents {
        validate_identifier(parent).map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
    }
    if file
        .version
        .as_ref()
        .is_some_and(|version| version.is_empty() || version.len() > 128)
    {
        return Err(Error::new(ErrorCode::MalformedResponse));
    }
    Ok(())
}

fn validate_changes_page(page: &ChangesPage) -> Result<()> {
    validate_optional_page_token(page.next_page_token.as_deref())?;
    validate_optional_page_token(page.new_start_page_token.as_deref())?;
    match (&page.next_page_token, &page.new_start_page_token) {
        (Some(_), None) | (None, Some(_)) => {}
        _ => return Err(Error::new(ErrorCode::MalformedResponse)),
    }
    for change in &page.changes {
        validate_identifier(&change.file_id)
            .map_err(|_| Error::new(ErrorCode::MalformedResponse))?;
        match (change.removed, &change.file) {
            (true, None) => {}
            (false, Some(file)) if file.id == change.file_id => validate_file(file)?,
            _ => return Err(Error::new(ErrorCode::MalformedResponse)),
        }
    }
    Ok(())
}

pub(crate) fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}

fn validate_page_token(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}

fn validate_optional_page_token(value: Option<&str>) -> Result<()> {
    value.map_or(Ok(()), validate_page_token)
}

fn map_transport_error(error: &reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::new(ErrorCode::Timeout)
    } else {
        Error::new(ErrorCode::Transport)
    }
}

fn map_transfer_transport_error(error: &reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::new(ErrorCode::Timeout)
    } else if error.is_redirect() {
        Error::new(ErrorCode::RedirectRejected)
    } else {
        Error::new(ErrorCode::Transport)
    }
}

const fn map_status(status: StatusCode) -> ErrorCode {
    match status.as_u16() {
        401 => ErrorCode::Unauthorized,
        403 => ErrorCode::Forbidden,
        404 => ErrorCode::NotFound,
        410 => ErrorCode::CursorExpired,
        429 => ErrorCode::RateLimited,
        _ => ErrorCode::ProviderRejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    const TOKEN: &str = "secret-access-token";
    const FILE_JSON: &str = r#"{
        "id":"folder_1","name":"Root","mimeType":"application/vnd.google-apps.folder",
        "parents":[],"trashed":false,"version":"7"
    }"#;

    fn client(server: &Server) -> ReadOnlyDrive {
        ReadOnlyDrive::for_test(
            &format!("{}/drive/v3/", server.url()),
            AccessToken::new(TOKEN),
            4096,
        )
        .unwrap()
    }

    #[test]
    fn token_and_client_debug_are_redacted() {
        static_assertions::assert_not_impl_any!(AccessToken: Clone, serde::Serialize, fmt::Display);
        static_assertions::assert_not_impl_any!(ReadOnlyDrive: Clone, serde::Serialize);
        let mut server = Server::new();
        let client = client(&server);
        assert_eq!(
            format!("{:?}", AccessToken::new(TOKEN)),
            "AccessToken([REDACTED])"
        );
        let debug = format!("{client:?}");
        assert!(!debug.contains(TOKEN));
        assert!(debug.contains("[REDACTED]"));
        server.reset();
    }

    #[test]
    fn write_helpers_reject_existing_item_urls_before_network() {
        let server = Server::new();
        let client = client(&server);
        let existing_item_upload =
            Url::parse(&format!("{}/upload/drive/v3/files/file_1", server.url())).unwrap();
        let existing_item_session = Url::parse(&format!(
            "{}/upload/drive/v3/files/file_1?upload_id=held_session",
            server.url()
        ))
        .unwrap();

        assert_eq!(
            client
                .post_resumable_create(existing_item_upload, "id", "text/markdown", 0, Vec::new(),)
                .unwrap_err()
                .code(),
            ErrorCode::UnexpectedOrigin
        );
        assert_eq!(
            client
                .put_resumable_session(existing_item_session, Vec::new(), "bytes */0")
                .unwrap_err()
                .code(),
            ErrorCode::UnexpectedOrigin
        );
    }

    #[test]
    fn exact_binding_uses_account_and_root_get_requests() {
        let mut server = Server::new();
        let about = server
            .mock("GET", "/drive/v3/about")
            .match_header("authorization", Matcher::Exact(format!("Bearer {TOKEN}")))
            .match_query(Matcher::UrlEncoded(
                "fields".into(),
                "user(permissionId)".into(),
            ))
            .with_body(r#"{"user":{"permissionId":"account_1"}}"#)
            .create();
        let root = server
            .mock("GET", "/drive/v3/files/folder_1")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("fields".into(), FILE_FIELDS.into()),
                Matcher::UrlEncoded("supportsAllDrives".into(), "true".into()),
            ]))
            .with_body(FILE_JSON)
            .create();
        let drive = client(&server);
        let binding = drive.verify_binding("account_1", "folder_1").unwrap();
        assert_eq!(binding.account_id(), "account_1");
        assert_eq!(binding.remote_root_id(), "folder_1");
        about.assert();
        root.assert();
    }

    #[test]
    fn root_rejects_trashed_and_non_folder_metadata() {
        for body in [
            r#"{"id":"root_1","name":"Root","mimeType":"application/vnd.google-apps.folder","trashed":true,"version":"1"}"#,
            r#"{"id":"root_1","name":"Root","mimeType":"text/plain","trashed":false,"version":"1"}"#,
        ] {
            let mut server = Server::new();
            let mock = server
                .mock("GET", "/drive/v3/files/root_1")
                .match_query(Matcher::Any)
                .with_body(body)
                .create();
            assert_eq!(
                client(&server).verify_root("root_1").unwrap_err().code(),
                ErrorCode::InvalidRoot
            );
            mock.assert();
        }
    }

    #[test]
    fn exact_binding_rejects_wrong_account_before_root_lookup() {
        let mut server = Server::new();
        let about = server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body(r#"{"user":{"permissionId":"observed_account"}}"#)
            .create();
        let error = client(&server)
            .verify_binding("requested_account", "folder_1")
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidAccount);
        about.assert();
    }

    #[test]
    fn child_pagination_preserves_duplicates_and_nested_folder_candidates() {
        let mut server = Server::new();
        let first = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("q".into(), "'root_1' in parents and trashed = false".into()),
                Matcher::UrlEncoded("pageSize".into(), "1000".into()),
            ]))
            .with_body(r#"{
                "files":[
                    {"id":"file_a","name":"duplicate.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"1"},
                    {"id":"file_b","name":"duplicate.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"2"},
                    {"id":"folder_nested","name":"Nested","mimeType":"application/vnd.google-apps.folder","parents":["root_1"],"trashed":false,"version":"3"}
                ],"nextPageToken":"page_2"
            }"#)
            .create();
        let second = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("q".into(), "'root_1' in parents and trashed = false".into()),
                Matcher::UrlEncoded("pageToken".into(), "page_2".into()),
            ]))
            .with_body(r#"{"files":[]}"#)
            .create();
        let drive = client(&server);
        let page = drive.list_children_page("root_1", None).unwrap();
        assert_eq!(page.files.len(), 3);
        assert_eq!(page.files[0].name, page.files[1].name);
        assert_ne!(page.files[0].id, page.files[1].id);
        assert!(page.files[2].is_folder());
        assert_eq!(page.next_page_token.as_deref(), Some("page_2"));
        assert!(drive
            .list_children_page("root_1", Some("page_2"))
            .unwrap()
            .files
            .is_empty());
        first.assert();
        second.assert();
    }

    #[test]
    fn root_alias_accepts_the_opaque_parent_id_returned_by_google() {
        let mut server = Server::new();
        let page = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::UrlEncoded(
                "q".into(),
                "'root' in parents and trashed = false".into(),
            ))
            .with_body(
                r#"{
                    "files":[{
                        "id":"folder_1","name":"Fixture","mimeType":"application/vnd.google-apps.folder",
                        "parents":["opaque_google_root_id"],"trashed":false,"version":"1"
                    }]
                }"#,
            )
            .create();

        let result = client(&server).list_children_page("root", None).unwrap();

        page.assert();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].parents, ["opaque_google_root_id"]);
    }

    #[test]
    fn child_page_still_rejects_a_mismatched_exact_parent() {
        let mut server = Server::new();
        let page = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Any)
            .with_body(
                r#"{
                    "files":[{
                        "id":"file_1","name":"note.md","mimeType":"text/markdown",
                        "parents":["other_parent"],"trashed":false,"version":"1"
                    }]
                }"#,
            )
            .create();

        let error = client(&server)
            .list_children_page("expected_parent", None)
            .expect_err("an exact parent mismatch must fail closed");

        page.assert();
        assert_eq!(error.code(), ErrorCode::MalformedResponse);
    }

    #[test]
    fn root_alias_rejects_a_child_without_a_parent() {
        let mut server = Server::new();
        let page = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Any)
            .with_body(
                r#"{
                    "files":[{
                        "id":"file_1","name":"orphan.md","mimeType":"text/markdown",
                        "parents":[],"trashed":false,"version":"1"
                    }]
                }"#,
            )
            .create();

        let error = client(&server)
            .list_children_page("root", None)
            .expect_err("a root-alias child without a parent must fail closed");

        page.assert();
        assert_eq!(error.code(), ErrorCode::MalformedResponse);
    }

    #[test]
    fn incomplete_child_search_fails_closed() {
        let mut server = Server::new();
        let page = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Any)
            .with_body(r#"{"files":[],"incompleteSearch":true}"#)
            .create();
        let drive = client(&server);

        let error = drive
            .list_children_page("folder_1", None)
            .expect_err("incomplete search must not be committed");

        page.assert();
        assert_eq!(error.code(), ErrorCode::IncompleteSearch);
    }

    #[test]
    fn start_token_and_changes_pages_include_metadata() {
        let mut server = Server::new();
        let start = server
            .mock("GET", "/drive/v3/changes/startPageToken")
            .match_query(Matcher::Any)
            .with_body(r#"{"startPageToken":"start_1"}"#)
            .create();
        let changes = server
            .mock("GET", "/drive/v3/changes")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("pageToken".into(), "start_1".into()),
                Matcher::UrlEncoded("pageSize".into(), "1000".into()),
            ]))
            .with_body(r#"{
                "changes":[
                    {"fileId":"file_1","removed":false,"file":{"id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],"trashed":false,"version":"4","md5Checksum":"0123456789abcdef0123456789abcdef"}},
                    {"fileId":"file_2","removed":true}
                ],"newStartPageToken":"durable_2"
            }"#)
            .create();
        let drive = client(&server);
        assert_eq!(drive.start_page_token().unwrap(), "start_1");
        let page = drive.changes_page("start_1").unwrap();
        assert_eq!(page.changes.len(), 2);
        assert_eq!(page.new_start_page_token.as_deref(), Some("durable_2"));
        start.assert();
        changes.assert();
    }

    #[test]
    fn redirect_is_not_followed() {
        let mut server = Server::new();
        let redirect = server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_status(302)
            .with_header("location", "https://attacker.invalid/steal")
            .create();
        let error = client(&server).account_identity().unwrap_err();
        assert_eq!(error.code(), ErrorCode::RedirectRejected);
        redirect.assert();
    }

    #[test]
    fn malformed_and_oversized_bodies_are_bounded_and_redacted() {
        let mut malformed_server = Server::new();
        let malformed = malformed_server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body(format!(r#"{{"token":"{TOKEN}"}}"#))
            .create();
        let error = client(&malformed_server).account_identity().unwrap_err();
        assert_eq!(error.code(), ErrorCode::MalformedResponse);
        assert!(!format!("{error:?} {error}").contains(TOKEN));
        malformed.assert();

        let mut oversized_server = Server::new();
        let oversized = oversized_server
            .mock("GET", "/drive/v3/about")
            .match_query(Matcher::Any)
            .with_body("x".repeat(4097))
            .create();
        assert_eq!(
            client(&oversized_server)
                .account_identity()
                .unwrap_err()
                .code(),
            ErrorCode::ResponseTooLarge
        );
        oversized.assert();
    }

    #[test]
    fn provider_statuses_have_stable_redacted_codes() {
        for (status, expected) in [
            (401, ErrorCode::Unauthorized),
            (403, ErrorCode::Forbidden),
            (404, ErrorCode::NotFound),
            (410, ErrorCode::CursorExpired),
        ] {
            let mut server = Server::new();
            let mock = server
                .mock("GET", "/drive/v3/changes")
                .match_query(Matcher::Any)
                .with_status(status)
                .with_body(format!(r#"{{"error":"{TOKEN}"}}"#))
                .create();
            let error = client(&server).changes_page("cursor_1").unwrap_err();
            assert_eq!(error.code(), expected);
            assert!(!format!("{error:?} {error}").contains(TOKEN));
            mock.assert();
        }
    }

    #[test]
    fn stalled_provider_response_maps_to_bounded_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });
        let drive = ReadOnlyDrive::for_test_with_timeout(
            &format!("http://{address}/drive/v3/"),
            AccessToken::new(TOKEN),
            4096,
            Duration::from_millis(20),
        )
        .unwrap();

        let error = drive.account_identity().unwrap_err();

        assert_eq!(error.code(), ErrorCode::Timeout);
        assert!(!format!("{error:?} {error}").contains(TOKEN));
        server.join().unwrap();
    }

    #[test]
    fn invalid_identifiers_cannot_change_request_origin_or_path() {
        let server = Server::new();
        let drive = client(&server);
        for id in ["https://attacker.invalid/x", "../about", "id?alt=media", ""] {
            assert_eq!(
                drive.file_metadata(id).unwrap_err().code(),
                ErrorCode::InvalidInput
            );
        }
    }

    #[test]
    fn metadata_maps_to_valid_sync_entry_only_after_path_resolution() {
        let file: RemoteFile = serde_json::from_str(r#"{
            "id":"file_1","name":"note.md","mimeType":"text/markdown","parents":["root_1"],
            "trashed":false,"version":"8","sha256Checksum":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }"#).unwrap();
        let entry = file.to_remote_entry("Nested/note.md").unwrap();
        assert_eq!(entry.file_id, "file_1");
        assert_eq!(entry.path, "Nested/note.md");
        assert!(entry.content_hash.is_some());
        assert_eq!(entry.remote_revision.len(), 64);
    }

    #[test]
    fn unicode_metadata_preserves_the_exact_canonical_path() {
        let file: RemoteFile = serde_json::from_str(
            r#"{
                "id":"file_1","name":"บันทึก.md","mimeType":"text/markdown",
                "parents":["root_1"],"trashed":false,"version":"8"
            }"#,
        )
        .unwrap();

        let entry = file.to_remote_entry("ภาษาไทย/บันทึก.md").unwrap();

        assert_eq!(entry.path, "ภาษาไทย/บันทึก.md");
    }

    #[test]
    fn drive_trait_maps_durable_folder_request_to_scan_page() {
        let mut server = Server::new();
        let list = server
            .mock("GET", "/drive/v3/files")
            .match_query(Matcher::Any)
            .with_body(
                r#"{
                "files":[{"id":"file_1","name":"note.md","mimeType":"text/markdown",
                    "parents":["folder_1"],"trashed":false,"version":"9"}]
            }"#,
            )
            .create();
        let mut drive = client(&server);
        let page = DriveClient::scan_folder_page(
            &mut drive,
            &ScanRequest {
                folder_id: "folder_1".into(),
                folder_path: "Nested".into(),
                page_token: None,
            },
        )
        .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].path, "Nested/note.md");
        assert_eq!(page.entries[0].parent_id, "folder_1");
        list.assert();
    }

    #[test]
    fn drive_trait_fail_closes_incremental_upsert_without_path_context() {
        let mut server = Server::new();
        let changes = server
            .mock("GET", "/drive/v3/changes")
            .match_query(Matcher::Any)
            .with_body(
                r#"{
                "changes":[{"fileId":"file_1","removed":false,"file":{
                    "id":"file_1","name":"moved.md","mimeType":"text/markdown",
                    "parents":["folder_2"],"trashed":false,"version":"10"
                }}],"newStartPageToken":"durable_2"
            }"#,
            )
            .create();
        let mut drive = client(&server);
        let error = DriveClient::changes_page(&mut drive, "cursor_1").unwrap_err();
        assert_eq!(error.code(), "cursor_ambiguous");
        changes.assert();
    }

    #[test]
    fn drive_trait_maps_removed_change_without_path_context() {
        let mut server = Server::new();
        let changes = server
            .mock("GET", "/drive/v3/changes")
            .match_query(Matcher::Any)
            .with_body(
                r#"{
                "changes":[{"fileId":"file_1","removed":true}],
                "newStartPageToken":"durable_2"
            }"#,
            )
            .create();
        let mut drive = client(&server);
        let page = DriveClient::changes_page(&mut drive, "cursor_1").unwrap();
        assert_eq!(
            page.changes,
            vec![RemoteChange::Removed {
                file_id: "file_1".into()
            }]
        );
        changes.assert();
    }

    fn changed_file(parent_id: &str, name: &str, mime_type: &str) -> crate::Change {
        crate::Change {
            file_id: "file_1".into(),
            removed: false,
            file: Some(RemoteFile {
                id: "file_1".into(),
                name: name.into(),
                mime_type: mime_type.into(),
                parents: vec![parent_id.into()],
                trashed: false,
                version: Some("10".into()),
                md5_checksum: None,
                sha1_checksum: None,
                sha256_checksum: None,
            }),
        }
    }

    #[test]
    fn change_resolver_builds_nested_unicode_path_below_exact_root() {
        let mut server = Server::new();
        let parent = server
            .mock("GET", "/drive/v3/files/folder_1")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"id":"folder_1","name":"ภาษาไทย","mimeType":"application/vnd.google-apps.folder","parents":["root_1"],"trashed":false,"version":"2"}"#,
            )
            .create();
        let drive = client(&server);

        let resolved = drive
            .resolve_change_below_root(
                "root_1",
                &changed_file("folder_1", "บันทึก.md", "text/markdown"),
            )
            .unwrap();

        let ResolvedDriveChange::Inside(entry) = resolved else {
            panic!("inside root")
        };
        assert_eq!(entry.path, "ภาษาไทย/บันทึก.md");
        assert_eq!(entry.parent_id, "folder_1");
        parent.assert();
    }

    #[test]
    fn change_resolver_distinguishes_outside_root_and_removed_id() {
        let mut server = Server::new();
        let outside = server
            .mock("GET", "/drive/v3/files/outside_1")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"id":"outside_1","name":"Other root","mimeType":"application/vnd.google-apps.folder","parents":[],"trashed":false,"version":"2"}"#,
            )
            .create();
        let drive = client(&server);
        assert_eq!(
            drive
                .resolve_change_below_root(
                    "root_1",
                    &changed_file("outside_1", "note.md", "text/markdown"),
                )
                .unwrap(),
            ResolvedDriveChange::OutsideBoundRoot
        );
        assert_eq!(
            drive
                .resolve_change_below_root(
                    "root_1",
                    &crate::Change {
                        file_id: "removed_1".into(),
                        removed: true,
                        file: None,
                    },
                )
                .unwrap(),
            ResolvedDriveChange::Removed {
                file_id: "removed_1".into()
            }
        );
        outside.assert();
    }

    #[test]
    fn change_resolver_rejects_native_mime_and_ancestry_loops() {
        let mut server = Server::new();
        let folder_a = server
            .mock("GET", "/drive/v3/files/folder_a")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"id":"folder_a","name":"A","mimeType":"application/vnd.google-apps.folder","parents":["folder_b"],"trashed":false,"version":"2"}"#,
            )
            .create();
        let folder_b = server
            .mock("GET", "/drive/v3/files/folder_b")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"id":"folder_b","name":"B","mimeType":"application/vnd.google-apps.folder","parents":["folder_a"],"trashed":false,"version":"2"}"#,
            )
            .create();
        let drive = client(&server);
        assert_eq!(
            drive
                .resolve_change_below_root(
                    "root_1",
                    &changed_file("root_1", "sheet", "application/vnd.google-apps.spreadsheet"),
                )
                .unwrap_err()
                .code(),
            ErrorCode::CursorAmbiguous
        );
        assert_eq!(
            drive
                .resolve_change_below_root(
                    "root_1",
                    &changed_file("folder_a", "note.md", "text/markdown"),
                )
                .unwrap_err()
                .code(),
            ErrorCode::CursorAmbiguous
        );
        folder_a.assert();
        folder_b.assert();
    }
}
