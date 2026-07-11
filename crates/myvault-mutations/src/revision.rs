use myvault_core::FileRevision as CoreRevision;
use myvault_recovery::FileRevision as RecoveryRevision;

pub(crate) fn to_recovery(value: &CoreRevision) -> RecoveryRevision {
    RecoveryRevision {
        blake3_hex: value.hex.clone(),
        byte_len: value.byte_len,
    }
}
