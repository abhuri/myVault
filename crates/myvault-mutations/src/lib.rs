#![forbid(unsafe_code)]

mod error;
mod operation;
mod revision;
mod service;

pub use error::MutationError;
pub use operation::{OperationId, RestoreOperation, TrashOperation};
pub use service::{MutationService, RestoreExecutionOutcome, TrashExecutionOutcome};
