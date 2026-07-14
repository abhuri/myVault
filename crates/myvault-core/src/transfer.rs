use std::io::{Read, Write};

use sha2::{Digest, Sha256};

use crate::{CoreError, FileRevision, MoveDurability, Result};

/// Canonical SHA-256 digest used to bind local bytes to provider metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Parses a canonical lowercase SHA-256 digest.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidSha256Digest`] for noncanonical input.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(CoreError::InvalidSha256Digest)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }
}

/// Evidence computed from the exact bytes consumed from one stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentSnapshot {
    pub revision: FileRevision,
    pub sha256: Sha256Digest,
    pub byte_len: u64,
}

/// A successful local publication and its directory durability evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentPublishOutcome {
    Created(MoveDurability),
    Replaced(MoveDurability),
}

/// Copies one bounded stream while computing both the local BLAKE3 revision
/// and provider-facing SHA-256 digest from exactly the bytes written.
///
/// # Errors
/// Returns an I/O error or [`CoreError::ResourceLimitExceeded`] without
/// accepting bytes beyond `max_bytes`.
pub fn stream_content_snapshot(
    reader: &mut impl Read,
    writer: &mut impl Write,
    max_bytes: usize,
) -> Result<ContentSnapshot> {
    let mut sha256 = Sha256::new();
    let mut revision = blake3::Hasher::new();
    let mut byte_len = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        byte_len = byte_len
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or(CoreError::ResourceLimitExceeded {
                resource: "transfer bytes",
                limit: max_bytes,
            })?;
        if byte_len > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
            return Err(CoreError::ResourceLimitExceeded {
                resource: "transfer bytes",
                limit: max_bytes,
            });
        }
        writer.write_all(&buffer[..count])?;
        revision.update(&buffer[..count]);
        sha256.update(&buffer[..count]);
    }
    Ok(ContentSnapshot {
        revision: FileRevision::new(revision.finalize().to_hex().to_string(), byte_len)?,
        sha256: Sha256Digest(format!("{:x}", sha256.finalize())),
        byte_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_standard_vectors_across_chunk_boundaries() {
        for (bytes, expected) in [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                &vec![b'a'; 1_000_000],
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            ),
        ] {
            let mut input = bytes;
            let mut output = Vec::new();
            let snapshot = stream_content_snapshot(&mut input, &mut output, bytes.len()).unwrap();
            assert_eq!(snapshot.sha256.as_str(), expected);
            assert_eq!(output, bytes);
        }
    }
}
