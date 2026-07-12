#![forbid(unsafe_code)]

mod error;
mod operation;
mod revision;
mod service;

pub use error::MutationError;
pub use operation::{NormalMoveOperation, OperationId, RestoreOperation, TrashOperation};
pub use service::{
    MutationService, NormalMoveExecutionOutcome, RestoreExecutionOutcome, TrashExecutionOutcome,
};
