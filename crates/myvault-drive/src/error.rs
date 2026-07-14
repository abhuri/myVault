use myvault_sync_engine::RemoteError;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// Stable, bounded, non-sensitive failure classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    InvalidInput,
    Transport,
    Timeout,
    RedirectRejected,
    ResponseTooLarge,
    MalformedResponse,
    Unauthorized,
    Forbidden,
    NotFound,
    CursorExpired,
    CursorAmbiguous,
    IncompleteSearch,
    RateLimited,
    ProviderRejected,
    InvalidAccount,
    InvalidRoot,
    UnexpectedOrigin,
}

impl ErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "drive_invalid_input",
            Self::Transport => "drive_transport",
            Self::Timeout => "drive_timeout",
            Self::RedirectRejected => "drive_redirect_rejected",
            Self::ResponseTooLarge => "drive_response_too_large",
            Self::MalformedResponse => "drive_malformed_response",
            Self::Unauthorized => "drive_unauthorized",
            Self::Forbidden => "drive_forbidden",
            Self::NotFound => "drive_not_found",
            Self::CursorExpired => "cursor_expired",
            Self::CursorAmbiguous => "cursor_ambiguous",
            Self::IncompleteSearch => "drive_incomplete_search",
            Self::RateLimited => "drive_rate_limited",
            Self::ProviderRejected => "drive_provider_rejected",
            Self::InvalidAccount => "drive_invalid_account",
            Self::InvalidRoot => "drive_invalid_root",
            Self::UnexpectedOrigin => "drive_unexpected_origin",
        }
    }
}

/// Drive errors contain no provider response body, URL, token, or transport text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    code: ErrorCode,
}

impl Error {
    pub(crate) const fn new(code: ErrorCode) -> Self {
        Self { code }
    }

    #[must_use]
    pub const fn code(self) -> ErrorCode {
        self.code
    }

    /// Converts this adapter failure into the sync engine's bounded error type.
    ///
    /// # Panics
    /// Panics only if a compile-time static `ErrorCode` mapping violates the
    /// sync engine's bounded portable-code invariant.
    #[must_use]
    pub fn to_remote_error(self) -> RemoteError {
        RemoteError::new(self.code.as_str())
            .expect("adapter error codes are compile-time bounded and portable")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "read-only Drive request failed ({})",
            self.code.as_str()
        )
    }
}

impl std::error::Error for Error {}
