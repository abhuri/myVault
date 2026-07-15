#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use myvault_core::{FileRevision, Sha256Digest, VaultPath};
use uuid::Uuid;

pub(crate) const ANDROID_MAX_TRANSFER_BYTES: usize = 16 * 1024 * 1024;

const ANDROID_TRANSFER_NAMESPACE: Uuid = Uuid::from_u128(0x49fb_8672_6749_4f9a_a220_994f_f771_a841);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AndroidTransferDirection {
    Upload,
    Download,
}

impl AndroidTransferDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AndroidTransferPolicyError {
    TooLarge,
    InvalidPath,
    ProtectedPath,
    DuplicatePath,
    PortablePathCollision,
    InvalidSha256,
    InvalidRevision,
    LengthMismatch,
    DigestMismatch,
    RevisionMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedSafEvidence {
    sha256: Sha256Digest,
    revision: FileRevision,
}

impl ExpectedSafEvidence {
    pub(crate) fn new(
        sha256_hex: impl Into<String>,
        revision_hex: impl Into<String>,
        byte_len: u64,
    ) -> Result<Self, AndroidTransferPolicyError> {
        let sha256 = Sha256Digest::parse(sha256_hex)
            .map_err(|_| AndroidTransferPolicyError::InvalidSha256)?;
        let revision = FileRevision::new(revision_hex, byte_len)
            .map_err(|_| AndroidTransferPolicyError::InvalidRevision)?;
        Ok(Self { sha256, revision })
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            sha256: Sha256Digest::from_bytes(bytes),
            revision: FileRevision::from_bytes(bytes),
        }
    }
}

/// Non-content evidence ready to be copied into a durable Android transfer intent.
///
/// The portable path contributes to the deterministic UUID but is deliberately not
/// retained in this value. This type has no serialization implementation and cannot
/// carry a SAF body, URI, OAuth credential, or provider response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AndroidTransferEvidence {
    operation_id: Uuid,
    direction: AndroidTransferDirection,
    sha256: Sha256Digest,
    revision: FileRevision,
}

impl AndroidTransferEvidence {
    pub(crate) const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    pub(crate) const fn direction(&self) -> AndroidTransferDirection {
        self.direction
    }

    pub(crate) fn sha256(&self) -> &str {
        self.sha256.as_str()
    }

    pub(crate) fn revision(&self) -> &FileRevision {
        &self.revision
    }

    pub(crate) const fn byte_len(&self) -> u64 {
        self.revision.byte_len
    }

    pub(crate) fn operation_marker(&self) -> String {
        format!("android-r2-{}", self.operation_id.simple())
    }
}

/// Recomputes exact byte evidence before an Android SAF object may be enqueued.
///
/// The size cap is checked before hashing. The returned value contains no path or
/// content body; callers retain their already-authorized SAF capability separately.
pub(crate) fn prepare_saf_transfer(
    direction: AndroidTransferDirection,
    portable_path: &str,
    bytes: &[u8],
    expected: &ExpectedSafEvidence,
) -> Result<AndroidTransferEvidence, AndroidTransferPolicyError> {
    if bytes.len() > ANDROID_MAX_TRANSFER_BYTES {
        return Err(AndroidTransferPolicyError::TooLarge);
    }
    let path = validated_content_path(portable_path)?;
    let byte_len = u64::try_from(bytes.len()).map_err(|_| AndroidTransferPolicyError::TooLarge)?;
    if expected.revision.byte_len != byte_len {
        return Err(AndroidTransferPolicyError::LengthMismatch);
    }
    let sha256 = Sha256Digest::from_bytes(bytes);
    if sha256 != expected.sha256 {
        return Err(AndroidTransferPolicyError::DigestMismatch);
    }
    let revision = FileRevision::from_bytes(bytes);
    if revision != expected.revision {
        return Err(AndroidTransferPolicyError::RevisionMismatch);
    }
    let operation_id = deterministic_operation_id(direction, &path, &sha256, &revision);
    Ok(AndroidTransferEvidence {
        operation_id,
        direction,
        sha256,
        revision,
    })
}

/// Rejects exact duplicates and cross-platform Unicode/case aliases before enqueue.
pub(crate) fn validate_saf_path_set<'a>(
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<(), AndroidTransferPolicyError> {
    let mut exact = BTreeSet::new();
    let mut portable = BTreeMap::new();
    for value in paths {
        let path = validated_content_path(value)?;
        if !exact.insert(path.as_str().to_owned()) {
            return Err(AndroidTransferPolicyError::DuplicatePath);
        }
        let key = path.collision_key();
        if portable
            .insert(key, path.as_str().to_owned())
            .is_some_and(|existing| existing != path.as_str())
        {
            return Err(AndroidTransferPolicyError::PortablePathCollision);
        }
    }
    Ok(())
}

fn validated_content_path(value: &str) -> Result<VaultPath, AndroidTransferPolicyError> {
    let path =
        VaultPath::from_portable(value).map_err(|_| AndroidTransferPolicyError::InvalidPath)?;
    let protected_root = path
        .collision_key()
        .split('/')
        .next()
        .is_some_and(|root| matches!(root, ".obsidian" | ".trash"));
    if protected_root {
        Err(AndroidTransferPolicyError::ProtectedPath)
    } else {
        Ok(path)
    }
}

fn deterministic_operation_id(
    direction: AndroidTransferDirection,
    path: &VaultPath,
    sha256: &Sha256Digest,
    revision: &FileRevision,
) -> Uuid {
    let mut evidence = String::from("myvault-android-r2\0");
    for part in [
        direction.as_str(),
        path.as_str(),
        sha256.as_str(),
        revision.hex.as_str(),
        &revision.byte_len.to_string(),
    ] {
        evidence.push_str(&part.len().to_string());
        evidence.push(':');
        evidence.push_str(part);
        evidence.push('\0');
    }
    Uuid::new_v5(&ANDROID_TRANSFER_NAMESPACE, evidence.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare(bytes: &[u8], path: &str) -> AndroidTransferEvidence {
        prepare_saf_transfer(
            AndroidTransferDirection::Download,
            path,
            bytes,
            &ExpectedSafEvidence::from_bytes(bytes),
        )
        .expect("valid SAF transfer")
    }

    #[test]
    fn zero_bytes_have_exact_standard_evidence_and_stable_identity() {
        let first = prepare(&[], "empty.bin");
        let second = prepare(&[], "empty.bin");
        assert_eq!(first, second);
        assert_eq!(first.byte_len(), 0);
        assert_eq!(
            first.sha256(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(first.direction(), AndroidTransferDirection::Download);
        assert_eq!(first.operation_marker().len(), 43);
        assert!(!first.operation_id().is_nil());
    }

    #[test]
    fn unicode_path_is_accepted_but_absent_from_operation_evidence() {
        let path = "บันทึก/你好 world.md";
        let evidence = prepare("สวัสดี".as_bytes(), path);
        let debug = format!("{evidence:?}");
        assert!(!debug.contains(path));
        assert!(!debug.contains("สวัสดี"));
        assert_eq!(evidence.byte_len(), "สวัสดี".len() as u64);
        assert_eq!(evidence.revision().byte_len, evidence.byte_len());
    }

    #[test]
    fn five_mib_and_exact_cap_are_accepted() {
        let five_mib = vec![0x5a; 5 * 1024 * 1024];
        assert_eq!(prepare(&five_mib, "files/five.bin").byte_len(), 5_242_880);

        let at_cap = vec![0xa5; ANDROID_MAX_TRANSFER_BYTES];
        assert_eq!(
            prepare(&at_cap, "files/cap.bin").byte_len(),
            ANDROID_MAX_TRANSFER_BYTES as u64
        );
    }

    #[test]
    fn cap_plus_one_is_rejected_before_enqueue() {
        let oversized = vec![0; ANDROID_MAX_TRANSFER_BYTES + 1];
        assert_eq!(
            prepare_saf_transfer(
                AndroidTransferDirection::Upload,
                "large.bin",
                &oversized,
                &ExpectedSafEvidence::from_bytes(&[]),
            ),
            Err(AndroidTransferPolicyError::TooLarge)
        );
    }

    #[test]
    fn malformed_expected_hashes_are_rejected() {
        let valid_sha = Sha256Digest::from_bytes(b"abc").as_str().to_owned();
        let valid_revision = FileRevision::from_bytes(b"abc").hex;
        for sha in ["", "abc", &"A".repeat(64), &"z".repeat(64)] {
            assert_eq!(
                ExpectedSafEvidence::new(sha, valid_revision.clone(), 3),
                Err(AndroidTransferPolicyError::InvalidSha256)
            );
        }
        for revision in ["", "abc", &"A".repeat(64), &"z".repeat(64)] {
            assert_eq!(
                ExpectedSafEvidence::new(valid_sha.clone(), revision, 3),
                Err(AndroidTransferPolicyError::InvalidRevision)
            );
        }
    }

    #[test]
    fn recomputation_rejects_length_digest_and_revision_mismatch() {
        let actual = b"abc";
        let exact = ExpectedSafEvidence::from_bytes(actual);
        assert_eq!(
            prepare_saf_transfer(
                AndroidTransferDirection::Upload,
                "note.md",
                actual,
                &ExpectedSafEvidence::new(exact.sha256.as_str(), exact.revision.hex.clone(), 4,)
                    .unwrap(),
            ),
            Err(AndroidTransferPolicyError::LengthMismatch)
        );

        let other = ExpectedSafEvidence::from_bytes(b"abd");
        assert_eq!(
            prepare_saf_transfer(AndroidTransferDirection::Upload, "note.md", actual, &other,),
            Err(AndroidTransferPolicyError::DigestMismatch)
        );

        let wrong_revision = ExpectedSafEvidence::new(
            exact.sha256.as_str(),
            other.revision.hex,
            actual.len() as u64,
        )
        .unwrap();
        assert_eq!(
            prepare_saf_transfer(
                AndroidTransferDirection::Upload,
                "note.md",
                actual,
                &wrong_revision,
            ),
            Err(AndroidTransferPolicyError::RevisionMismatch)
        );
    }

    #[test]
    fn protected_and_invalid_paths_fail_closed() {
        let expected = ExpectedSafEvidence::from_bytes(b"abc");
        for path in [
            ".obsidian/config",
            ".trash/old.md",
            ".ｏｂｓｉｄｉａｎ/theme.css",
            ".ｔｒａｓｈ/old.md",
        ] {
            assert_eq!(
                prepare_saf_transfer(AndroidTransferDirection::Upload, path, b"abc", &expected,),
                Err(AndroidTransferPolicyError::ProtectedPath)
            );
        }
        for path in ["", "../escape.md", "/absolute.md", r"bad\path.md"] {
            assert_eq!(
                prepare_saf_transfer(AndroidTransferDirection::Upload, path, b"abc", &expected,),
                Err(AndroidTransferPolicyError::InvalidPath)
            );
        }
    }

    #[test]
    fn exact_duplicates_and_unicode_case_collisions_are_distinct_errors() {
        assert_eq!(
            validate_saf_path_set(["Notes/one.md", "Notes/one.md"]),
            Err(AndroidTransferPolicyError::DuplicatePath)
        );
        for aliases in [
            ["Notes/One.md", "notes/one.md"],
            ["Cafe\u{301}.md", "Caf\u{e9}.md"],
            ["Ａ.md", "a.md"],
        ] {
            assert_eq!(
                validate_saf_path_set(aliases),
                Err(AndroidTransferPolicyError::PortablePathCollision)
            );
        }
        assert!(validate_saf_path_set(["Notes/one.md", "Notes/two.md"]).is_ok());
    }

    #[test]
    fn deterministic_identity_binds_direction_path_and_bytes_without_echo() {
        let bytes = b"abc";
        let expected = ExpectedSafEvidence::from_bytes(bytes);
        let download = prepare_saf_transfer(
            AndroidTransferDirection::Download,
            "one.md",
            bytes,
            &expected,
        )
        .unwrap();
        let upload =
            prepare_saf_transfer(AndroidTransferDirection::Upload, "one.md", bytes, &expected)
                .unwrap();
        let other_path = prepare_saf_transfer(
            AndroidTransferDirection::Download,
            "two.md",
            bytes,
            &expected,
        )
        .unwrap();
        assert_ne!(download.operation_id(), upload.operation_id());
        assert_ne!(download.operation_id(), other_path.operation_id());
        let rendered = format!("{download:?}");
        assert!(!rendered.contains("one.md"));
        assert!(!rendered.to_ascii_lowercase().contains("bearer"));
    }
}
