use crate::{CoreError, Result};

/// A canonical BLAKE3 digest and byte length for one regular file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRevision {
    pub hex: String,
    pub byte_len: u64,
}

impl FileRevision {
    /// Creates and validates a stored revision.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidRevision`] unless `hex` is canonical lowercase BLAKE3 hex.
    pub fn new(hex: impl Into<String>, byte_len: u64) -> Result<Self> {
        let revision = Self {
            hex: hex.into(),
            byte_len,
        };
        revision.validate()?;
        Ok(revision)
    }

    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            hex: blake3::hash(bytes).to_hex().to_string(),
            byte_len: bytes.len() as u64,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let valid = self.hex.len() == 64
            && self
                .hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(())
        } else {
            Err(CoreError::InvalidRevision)
        }
    }
}
