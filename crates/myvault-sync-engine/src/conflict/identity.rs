use std::{error, fmt, fmt::Write as _};

use myvault_core::VaultPath;
use sha2::{Digest, Sha256};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{ConflictCell, ConflictOutcome};

pub const CONFLICT_ID_VERSION: &str = "myvault-r3-conflict-id-v1";
pub const CONFLICT_NAMING_VERSION: &str = "r3-conflict-name-v1-nfkc17-casefold9-nfkc";

const OPERATION_NAMESPACE: Uuid = Uuid::from_bytes([
    0xa8, 0x35, 0x7e, 0x62, 0x4e, 0x5d, 0x5a, 0x88, 0x91, 0x69, 0x91, 0x3b, 0x90, 0x6d, 0x46, 0xcf,
]);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentFingerprint {
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictIdentityInput {
    pub account_id: String,
    pub remote_root_id: String,
    pub object_identity: String,
    pub cell: ConflictCell,
    pub outcome: ConflictOutcome,
    pub canonical_identity_path: String,
    pub target_parent_id: String,
    pub base: Option<ContentFingerprint>,
    pub local: Option<ContentFingerprint>,
    pub remote: Option<ContentFingerprint>,
    pub naming_version: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictOperationDomain {
    ConflictCopy,
    MergePublish,
    BasePublish,
    LocalPublish,
    RemoteExistingBlocked,
    GuardedLocalRename,
    GuardedLocalMove,
    GuardedLocalRenameMove,
}

impl ConflictOperationDomain {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ConflictCopy => "conflict-copy",
            Self::MergePublish => "merge-publish",
            Self::BasePublish => "base-publish",
            Self::LocalPublish => "local-publish",
            Self::RemoteExistingBlocked => "remote-existing-blocked",
            Self::GuardedLocalRename => "guarded-local-rename",
            Self::GuardedLocalMove => "guarded-local-move",
            Self::GuardedLocalRenameMove => "guarded-local-rename-move",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OccupiedConflictCopy {
    pub normalized_collision_key: String,
    pub conflict_id: String,
    pub expected_content: ContentFingerprint,
    pub destination_path: String,
    pub object_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictCopyNameRequest {
    pub conflict_id: String,
    pub source_path: String,
    pub destination_parent_path: Option<String>,
    pub expected_content: ContentFingerprint,
    pub naming_version: String,
    pub occupied: Vec<OccupiedConflictCopy>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictCopyName {
    pub destination_path: String,
    pub normalized_collision_key: String,
    pub id_prefix_length: usize,
    pub existing_object_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictCopyNameOutcome {
    Create(ConflictCopyName),
    Reuse(ConflictCopyName),
    NeedsReconcile(ConflictNameFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictNameFailure {
    UnsupportedNamingVersion,
    InvalidConflictId,
    InvalidContentFingerprint,
    InvalidSourcePath,
    InvalidDestinationParent,
    InvalidNormalizedComponent,
    InvalidIdentityField,
    InvalidOccupiedEvidence,
    ExhaustedConflictId,
}

impl fmt::Display for ConflictNameFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedNamingVersion => "unsupported conflict naming version",
            Self::InvalidConflictId => "invalid conflict identifier",
            Self::InvalidContentFingerprint => "invalid content fingerprint",
            Self::InvalidSourcePath => "invalid conflict source path",
            Self::InvalidDestinationParent => "invalid conflict destination parent",
            Self::InvalidNormalizedComponent => "invalid normalized path component",
            Self::InvalidIdentityField => "invalid bounded conflict identity field",
            Self::InvalidOccupiedEvidence => "invalid occupied conflict-copy evidence",
            Self::ExhaustedConflictId => "conflict identifier prefix space is exhausted",
        })
    }
}

impl error::Error for ConflictNameFailure {}

/// Derives the stable domain-separated conflict identity from immutable evidence.
///
/// # Errors
/// Returns a typed failure when a path, hash, code, or naming version is invalid.
pub fn derive_conflict_id(input: &ConflictIdentityInput) -> Result<String, ConflictNameFailure> {
    validate_identity_input(input)?;
    let mut stream = Vec::new();
    push_field(&mut stream, "identity_version", Some(CONFLICT_ID_VERSION));
    push_field(&mut stream, "account_id", Some(&input.account_id));
    push_field(&mut stream, "remote_root_id", Some(&input.remote_root_id));
    push_field(&mut stream, "object_identity", Some(&input.object_identity));
    push_field(&mut stream, "stable_cell_id", Some(input.cell.as_str()));
    push_field(
        &mut stream,
        "classification_code",
        Some(input.outcome.as_str()),
    );
    push_field(
        &mut stream,
        "canonical_identity_path",
        Some(&input.canonical_identity_path),
    );
    push_field(
        &mut stream,
        "target_parent_identity",
        Some(&input.target_parent_id),
    );
    push_fingerprint(&mut stream, "base", input.base.as_ref());
    push_fingerprint(&mut stream, "local", input.local.as_ref());
    push_fingerprint(&mut stream, "remote", input.remote.as_ref());
    push_field(&mut stream, "naming_version", Some(&input.naming_version));
    Ok(hex_sha256(&stream))
}

#[must_use]
pub fn derive_operation_id(domain: ConflictOperationDomain, conflict_id: &str) -> Uuid {
    let mut name = Vec::with_capacity(domain.as_str().len() + conflict_id.len() + 1);
    name.extend_from_slice(domain.as_str().as_bytes());
    name.push(0);
    name.extend_from_slice(conflict_id.as_bytes());
    Uuid::new_v5(&OPERATION_NAMESPACE, &name)
}

#[must_use]
pub fn operation_marker(domain: ConflictOperationDomain, conflict_id: &str) -> String {
    let prefix = conflict_id.chars().take(32).collect::<String>();
    format!("r3.{}.{prefix}", domain.as_str())
}

/// Resolves a deterministic portable conflict-copy name without consulting a filesystem.
#[must_use]
pub fn resolve_conflict_copy_name(request: &ConflictCopyNameRequest) -> ConflictCopyNameOutcome {
    if request.naming_version != CONFLICT_NAMING_VERSION {
        return ConflictCopyNameOutcome::NeedsReconcile(
            ConflictNameFailure::UnsupportedNamingVersion,
        );
    }
    if !is_lower_hex_64(&request.conflict_id) {
        return ConflictCopyNameOutcome::NeedsReconcile(ConflictNameFailure::InvalidConflictId);
    }
    if !valid_fingerprint(&request.expected_content) {
        return ConflictCopyNameOutcome::NeedsReconcile(
            ConflictNameFailure::InvalidContentFingerprint,
        );
    }
    if request.occupied.len() > 4_096
        || request.occupied.iter().any(|occupied| {
            !is_lower_hex_64(&occupied.normalized_collision_key)
                || !is_lower_hex_64(&occupied.conflict_id)
                || !valid_fingerprint(&occupied.expected_content)
                || !is_remote_id(&occupied.object_id)
                || normalized_collision_key(&occupied.destination_path).as_deref()
                    != Ok(occupied.normalized_collision_key.as_str())
        })
    {
        return ConflictCopyNameOutcome::NeedsReconcile(
            ConflictNameFailure::InvalidOccupiedEvidence,
        );
    }
    let Ok(source) = exact_vault_path(&request.source_path) else {
        return ConflictCopyNameOutcome::NeedsReconcile(ConflictNameFailure::InvalidSourcePath);
    };
    if request
        .destination_parent_path
        .as_deref()
        .is_some_and(|parent| exact_vault_path(parent).is_err())
    {
        return ConflictCopyNameOutcome::NeedsReconcile(
            ConflictNameFailure::InvalidDestinationParent,
        );
    }
    let source_name = source
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or(source.as_str());
    let (stem, extension) = split_extension(source_name);

    for prefix_length in (12..=64).step_by(4) {
        let suffix = format!(" (conflict {})", &request.conflict_id[..prefix_length]);
        let Some(candidate) = build_candidate_path(
            request.destination_parent_path.as_deref(),
            stem,
            extension,
            &suffix,
        ) else {
            return ConflictCopyNameOutcome::NeedsReconcile(
                ConflictNameFailure::InvalidDestinationParent,
            );
        };
        let normalized_collision_key = match normalized_collision_key(candidate.as_str()) {
            Ok(key) => key,
            Err(failure) => return ConflictCopyNameOutcome::NeedsReconcile(failure),
        };
        let name = ConflictCopyName {
            destination_path: candidate.as_str().to_owned(),
            normalized_collision_key,
            id_prefix_length: prefix_length,
            existing_object_id: None,
        };
        let collisions = request
            .occupied
            .iter()
            .filter(|occupied| occupied.normalized_collision_key == name.normalized_collision_key)
            .collect::<Vec<_>>();
        if collisions.is_empty() {
            return ConflictCopyNameOutcome::Create(name);
        }
        if collisions.iter().all(|occupied| {
            occupied.conflict_id == request.conflict_id
                && occupied.expected_content == request.expected_content
        }) {
            let object_id = collisions[0].object_id.clone();
            if collisions.iter().any(|occupied| {
                occupied.object_id != object_id
                    || occupied.destination_path != name.destination_path
            }) {
                return ConflictCopyNameOutcome::NeedsReconcile(
                    ConflictNameFailure::InvalidOccupiedEvidence,
                );
            }
            return ConflictCopyNameOutcome::Reuse(ConflictCopyName {
                existing_object_id: Some(object_id),
                ..name
            });
        }
    }
    ConflictCopyNameOutcome::NeedsReconcile(ConflictNameFailure::ExhaustedConflictId)
}

/// Computes the redacted persisted digest for the versioned collision comparator.
///
/// # Errors
/// Rejects non-canonical paths or components that normalize into separators/control data.
pub fn normalized_collision_key(path: &str) -> Result<String, ConflictNameFailure> {
    let path = exact_vault_path(path).map_err(|()| ConflictNameFailure::InvalidSourcePath)?;
    let mut stream = Vec::new();
    for component in path.as_str().split('/') {
        let normalized: String = component.nfkc().case_fold().nfkc().collect();
        if normalized.is_empty()
            || normalized
                .chars()
                .any(|character| character == '/' || character == '\0' || character.is_control())
            || !VaultPath::from_portable(&normalized)
                .is_ok_and(|component| component.as_str() == normalized)
        {
            return Err(ConflictNameFailure::InvalidNormalizedComponent);
        }
        stream.extend_from_slice(&(normalized.len() as u64).to_be_bytes());
        stream.extend_from_slice(normalized.as_bytes());
    }
    Ok(hex_sha256(&stream))
}

fn validate_identity_input(input: &ConflictIdentityInput) -> Result<(), ConflictNameFailure> {
    if input.naming_version != CONFLICT_NAMING_VERSION {
        return Err(ConflictNameFailure::UnsupportedNamingVersion);
    }
    exact_vault_path(&input.canonical_identity_path)
        .map_err(|()| ConflictNameFailure::InvalidSourcePath)?;
    if [
        input.account_id.as_str(),
        input.remote_root_id.as_str(),
        input.object_identity.as_str(),
        input.target_parent_id.as_str(),
    ]
    .iter()
    .any(|value| !is_remote_id(value))
        || input
            .local
            .as_ref()
            .is_some_and(|local| !valid_fingerprint(local))
        || input
            .remote
            .as_ref()
            .is_some_and(|remote| !valid_fingerprint(remote))
        || input
            .base
            .as_ref()
            .is_some_and(|base| !valid_fingerprint(base))
    {
        return Err(ConflictNameFailure::InvalidIdentityField);
    }
    Ok(())
}

fn is_remote_id(value: &str) -> bool {
    (1..=512).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_fingerprint(value: &ContentFingerprint) -> bool {
    is_lower_hex_64(&value.sha256) && i64::try_from(value.byte_length).is_ok()
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn exact_vault_path(value: &str) -> Result<VaultPath, ()> {
    let path = VaultPath::from_portable(value).map_err(|_| ())?;
    let protected = path
        .as_str()
        .split('/')
        .next()
        .map(|component| component.nfkc().case_fold().nfkc().collect::<String>())
        .is_some_and(|component| matches!(component.as_str(), ".obsidian" | ".trash"));
    if path.as_str() == value && !protected {
        Ok(path)
    } else {
        Err(())
    }
}

pub(super) fn is_exact_content_path(value: &str) -> bool {
    exact_vault_path(value).is_ok()
}

fn split_extension(name: &str) -> (&str, &str) {
    name.rfind('.').map_or((name, ""), |index| {
        if index == 0 || index + 1 == name.len() {
            (name, "")
        } else {
            (&name[..index], &name[index..])
        }
    })
}

fn build_candidate_path(
    parent: Option<&str>,
    stem: &str,
    extension: &str,
    suffix: &str,
) -> Option<VaultPath> {
    let mut truncated_stem = stem.to_owned();
    loop {
        let component = format!("{truncated_stem}{suffix}{extension}");
        let candidate = parent.map_or_else(
            || component.clone(),
            |parent| format!("{parent}/{component}"),
        );
        if let Ok(path) = exact_vault_path(&candidate) {
            return Some(path);
        }
        truncated_stem.pop()?;
    }
}

fn push_fingerprint(stream: &mut Vec<u8>, prefix: &str, value: Option<&ContentFingerprint>) {
    if let Some(value) = value {
        push_field(stream, &format!("{prefix}_sha256"), Some(&value.sha256));
        push_field(
            stream,
            &format!("{prefix}_byte_length"),
            Some(&value.byte_length.to_string()),
        );
    } else {
        push_field(stream, &format!("{prefix}_sha256"), None);
        push_field(stream, &format!("{prefix}_byte_length"), None);
    }
}

fn push_field(stream: &mut Vec<u8>, name: &str, value: Option<&str>) {
    stream.extend_from_slice(&(name.len() as u64).to_be_bytes());
    stream.extend_from_slice(name.as_bytes());
    if let Some(value) = value {
        stream.push(1);
        stream.extend_from_slice(&(value.len() as u64).to_be_bytes());
        stream.extend_from_slice(value.as_bytes());
    } else {
        stream.push(0);
        stream.extend_from_slice(&0_u64.to_be_bytes());
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
            output
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(byte: u8, byte_length: u64) -> ContentFingerprint {
        ContentFingerprint {
            sha256: format!("{byte:02x}").repeat(32),
            byte_length,
        }
    }

    #[test]
    fn identity_and_operation_ids_are_stable_and_domain_separated() {
        let input = ConflictIdentityInput {
            account_id: "account-1".to_owned(),
            remote_root_id: "root-1".to_owned(),
            object_identity: "object-1".to_owned(),
            cell: ConflictCell::C09,
            outcome: ConflictOutcome::PreserveBothLocal,
            canonical_identity_path: "notes/a.md".to_owned(),
            target_parent_id: "parent-1".to_owned(),
            base: Some(fingerprint(1, 10)),
            local: Some(fingerprint(2, 11)),
            remote: Some(fingerprint(3, 12)),
            naming_version: CONFLICT_NAMING_VERSION.to_owned(),
        };
        let first = derive_conflict_id(&input).expect("valid identity");
        assert_eq!(first, derive_conflict_id(&input).expect("same identity"));
        assert_ne!(
            derive_operation_id(ConflictOperationDomain::ConflictCopy, &first),
            derive_operation_id(ConflictOperationDomain::MergePublish, &first)
        );
    }

    #[test]
    fn naming_reuses_exact_identity_and_expands_for_collision() {
        let expected = fingerprint(7, 14);
        let request = ConflictCopyNameRequest {
            conflict_id: "a".repeat(64),
            source_path: "notes/Résumé.md".to_owned(),
            destination_parent_path: Some("notes".to_owned()),
            expected_content: expected.clone(),
            naming_version: CONFLICT_NAMING_VERSION.to_owned(),
            occupied: Vec::new(),
        };
        let ConflictCopyNameOutcome::Create(first) = resolve_conflict_copy_name(&request) else {
            panic!("expected create");
        };
        let mut exact = request.clone();
        exact.occupied.push(OccupiedConflictCopy {
            normalized_collision_key: first.normalized_collision_key.clone(),
            conflict_id: request.conflict_id.clone(),
            expected_content: expected.clone(),
            destination_path: first.destination_path.clone(),
            object_id: "object-existing".to_owned(),
        });
        assert!(matches!(
            resolve_conflict_copy_name(&exact),
            ConflictCopyNameOutcome::Reuse(_)
        ));
        let mut collision = request;
        collision.occupied.push(OccupiedConflictCopy {
            normalized_collision_key: first.normalized_collision_key,
            conflict_id: "b".repeat(64),
            expected_content: expected,
            destination_path: first.destination_path,
            object_id: "object-collision".to_owned(),
        });
        let ConflictCopyNameOutcome::Create(expanded) = resolve_conflict_copy_name(&collision)
        else {
            panic!("expected expanded create");
        };
        assert_eq!(expanded.id_prefix_length, 16);
    }

    #[test]
    fn naming_fails_closed_after_full_conflict_id_collision() {
        let mut request = ConflictCopyNameRequest {
            conflict_id: "a".repeat(64),
            source_path: "notes/a.md".to_owned(),
            destination_parent_path: Some("notes".to_owned()),
            expected_content: fingerprint(7, 14),
            naming_version: CONFLICT_NAMING_VERSION.to_owned(),
            occupied: Vec::new(),
        };
        for _ in 0..14 {
            let ConflictCopyNameOutcome::Create(candidate) = resolve_conflict_copy_name(&request)
            else {
                panic!("candidate before full-prefix exhaustion");
            };
            request.occupied.push(OccupiedConflictCopy {
                normalized_collision_key: candidate.normalized_collision_key,
                conflict_id: "b".repeat(64),
                expected_content: fingerprint(8, 14),
                destination_path: candidate.destination_path,
                object_id: "object-collision".to_owned(),
            });
        }
        assert_eq!(
            resolve_conflict_copy_name(&request),
            ConflictCopyNameOutcome::NeedsReconcile(ConflictNameFailure::ExhaustedConflictId)
        );
    }

    #[test]
    fn collision_digest_folds_compatibility_and_case() {
        assert_eq!(
            normalized_collision_key("Notes/Ａ.md").expect("key"),
            normalized_collision_key("notes/a.md").expect("key")
        );
        assert_eq!(
            normalized_collision_key("notes/／.md"),
            Err(ConflictNameFailure::InvalidNormalizedComponent)
        );
        for protected in [".trash/a.md", ".ＴＲＡＳＨ/a.md", ".obsidian/a.md"] {
            assert_eq!(
                normalized_collision_key(protected),
                Err(ConflictNameFailure::InvalidSourcePath)
            );
        }
        for normalizes_to_invalid in ["notes/ＦＯＯ＼bar.md", "notes/ＣＯＮ.md"] {
            assert_eq!(
                normalized_collision_key(normalizes_to_invalid),
                Err(ConflictNameFailure::InvalidNormalizedComponent)
            );
        }
    }
}
