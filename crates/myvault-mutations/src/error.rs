use std::fmt;

use uuid::Uuid;

#[derive(Debug)]
pub enum MutationError {
    Core(myvault_core::CoreError),
    Recovery(myvault_recovery::Error),
    InvalidOperation(&'static str),
    IntentMismatch,
    UnsupportedEvidence { operation_id: Uuid, version: u32 },
}

impl fmt::Display for MutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(error) => write!(formatter, "vault mutation failed: {error}"),
            Self::Recovery(error) => write!(formatter, "recovery journal failed: {error}"),
            Self::InvalidOperation(reason) => write!(formatter, "invalid mutation: {reason}"),
            Self::IntentMismatch => {
                formatter.write_str("journal intent does not match the requested mutation")
            }
            Self::UnsupportedEvidence {
                operation_id,
                version,
            } => write!(
                formatter,
                "operation {operation_id} uses unsupported journal version {version}"
            ),
        }
    }
}

impl std::error::Error for MutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Core(error) => Some(error),
            Self::Recovery(error) => Some(error),
            Self::InvalidOperation(_) | Self::IntentMismatch | Self::UnsupportedEvidence { .. } => {
                None
            }
        }
    }
}

impl From<myvault_core::CoreError> for MutationError {
    fn from(value: myvault_core::CoreError) -> Self {
        Self::Core(value)
    }
}

impl From<myvault_recovery::Error> for MutationError {
    fn from(value: myvault_recovery::Error) -> Self {
        Self::Recovery(value)
    }
}
