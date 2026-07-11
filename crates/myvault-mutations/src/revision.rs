use myvault_core::FileRevision as CoreRevision;
use myvault_recovery::FileRevision as RecoveryRevision;

use crate::MutationError;

pub(crate) fn to_recovery(value: &CoreRevision) -> RecoveryRevision {
    RecoveryRevision {
        blake3_hex: value.hex.clone(),
        byte_len: value.byte_len,
    }
}

pub(crate) fn to_core(value: &RecoveryRevision) -> Result<CoreRevision, MutationError> {
    Ok(CoreRevision::new(value.blake3_hex.clone(), value.byte_len)?)
}
