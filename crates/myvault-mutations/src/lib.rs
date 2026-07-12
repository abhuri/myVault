#![forbid(unsafe_code)]

mod error;
mod operation;
mod revision;
mod service;

pub use error::MutationError;
pub use operation::{
    CaseRenameOperation, NormalMoveOperation, OperationId, RestoreOperation, TrashOperation,
};
pub use service::{
    CaseRenameExecutionOutcome, MutationService, NormalMoveExecutionOutcome,
    RestoreExecutionOutcome, TrashExecutionOutcome,
};
