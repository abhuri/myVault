//! Fail-closed local durable-identity evidence contracts.
//!
//! A [`HeldObjectIdentityToken`] proves only what an already-open platform
//! handle identifies *now*. It is useful input to a verifier, but it is never
//! restart-stable evidence and cannot be placed in a [`DurableExecutionBinding`]
//! directly. A trusted platform/provider verifier must independently validate
//! a [`RestartStableIdentityClaim`] before this module issues opaque
//! [`RestartStableIdentityEvidence`]. This module models and validates
//! evidence; it does not authorize or perform a filesystem mutation.

use std::{error, fmt};

use myvault_platform_fs::HeldObjectIdentityToken;
use sha2::{Digest, Sha256};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

/// Maximum bytes in a verifier/provider identifier.
pub const MAX_IDENTITY_PROVIDER_ID_BYTES: usize = 128;
/// Maximum bytes in a provider's independently-issued durable object identity.
pub const MAX_RESTART_STABLE_OBJECT_ID_BYTES: usize = 1_024;
/// Maximum bytes in a provider attestation token.
pub const MAX_RESTART_STABLE_ATTESTATION_BYTES: usize = 1_024;
/// Maximum bytes in an operation-intent preimage.
pub const MAX_INTENT_FINGERPRINT_INPUT_BYTES: usize = 8_192;
/// Maximum UTF-8 bytes in a target or collision-member name.
pub const MAX_LOCAL_IDENTITY_NAME_BYTES: usize = 255;
/// Maximum UTF-8 bytes in a canonical collision key.
pub const MAX_LOCAL_IDENTITY_COLLISION_KEY_BYTES: usize = 1_024;
/// Maximum rows in one complete collision snapshot.
pub const MAX_COLLISION_SNAPSHOT_MEMBERS: usize = 4_096;

const RESTART_STABLE_EVIDENCE_VERSION: u8 = 1;
const OPAQUE_PROVIDER_EVIDENCE_KIND: u8 = 1;
const IDENTITY_CONTRACT_HASH_VERSION: &[u8] = b"myvault-r3.5-local-identity-v1";

/// Redacted validation failure for local durable identity evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalIdentityError {
    UnsupportedEvidenceVersion,
    UnsupportedEvidenceKind,
    InvalidEvidence,
    VerificationRejected,
    InvalidRoleIdentity,
    InvalidTargetName,
    InvalidCollisionKey,
    CollisionMemberCountMismatch,
    CollisionSnapshotTooLarge,
    DuplicateCollisionMemberName,
    DuplicateCollisionMemberIdentity,
    NonCanonicalCollisionMemberOrder,
    DestinationParentChanged,
    DestinationParentMismatch,
    InvalidOperationId,
    InvalidVaultId,
    IntentTooLarge,
}

impl fmt::Display for LocalIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedEvidenceVersion => {
                "restart-stable identity evidence version is unsupported"
            }
            Self::UnsupportedEvidenceKind => "restart-stable identity evidence kind is unsupported",
            Self::InvalidEvidence => "restart-stable identity evidence is invalid",
            Self::VerificationRejected => {
                "restart-stable identity evidence was not independently verified"
            }
            Self::InvalidRoleIdentity => "a required durable identity role is invalid",
            Self::InvalidTargetName => "target name evidence is invalid",
            Self::InvalidCollisionKey => "collision-key evidence is invalid",
            Self::CollisionMemberCountMismatch => {
                "collision snapshot member count does not match its rows"
            }
            Self::CollisionSnapshotTooLarge => "collision snapshot has too many members",
            Self::DuplicateCollisionMemberName => {
                "collision snapshot contains a duplicate member name"
            }
            Self::DuplicateCollisionMemberIdentity => {
                "collision snapshot contains a duplicate member identity"
            }
            Self::NonCanonicalCollisionMemberOrder => {
                "collision snapshot members are not in canonical order"
            }
            Self::DestinationParentChanged => {
                "destination parent identity changed during collision capture"
            }
            Self::DestinationParentMismatch => {
                "collision snapshot does not match the bound destination parent"
            }
            Self::InvalidOperationId => "durable operation identifier is invalid",
            Self::InvalidVaultId => "durable vault identifier is invalid",
            Self::IntentTooLarge => "operation intent evidence exceeds the allowed size",
        })
    }
}

impl error::Error for LocalIdentityError {}

/// The currently supported independently-issued durable evidence encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartStableEvidenceKind {
    OpaqueProviderToken,
}

impl RestartStableEvidenceKind {
    const fn encoded(self) -> u8 {
        match self {
            Self::OpaqueProviderToken => OPAQUE_PROVIDER_EVIDENCE_KIND,
        }
    }

    const fn decode(value: u8) -> Result<Self, LocalIdentityError> {
        match value {
            OPAQUE_PROVIDER_EVIDENCE_KIND => Ok(Self::OpaqueProviderToken),
            _ => Err(LocalIdentityError::UnsupportedEvidenceKind),
        }
    }
}

/// Untrusted provider material that a verifier may independently validate.
///
/// Constructing this value makes no durability assertion. The material becomes
/// [`RestartStableIdentityEvidence`] only after
/// [`verify_restart_stable_identity`] calls a trusted verifier successfully.
#[derive(Clone, Eq, PartialEq)]
pub struct RestartStableIdentityClaim {
    version: u8,
    kind: RestartStableEvidenceKind,
    provider_id: Vec<u8>,
    object_id: Vec<u8>,
    attestation: Vec<u8>,
}

impl RestartStableIdentityClaim {
    /// Builds bounded untrusted material in the current provider-token format.
    ///
    /// # Errors
    /// Returns a redacted error when any field is empty or exceeds its bound.
    pub fn opaque_provider_token(
        provider_id: impl Into<Vec<u8>>,
        object_id: impl Into<Vec<u8>>,
        attestation: impl Into<Vec<u8>>,
    ) -> Result<Self, LocalIdentityError> {
        Self::from_parts(
            RESTART_STABLE_EVIDENCE_VERSION,
            OPAQUE_PROVIDER_EVIDENCE_KIND,
            provider_id,
            object_id,
            attestation,
        )
    }

    /// Parses a versioned claim and rejects unsupported versions/kinds before a
    /// verifier sees it. Inputs remain untrusted until verification succeeds.
    ///
    /// # Errors
    /// Returns a redacted error for unsupported headers or invalid bounded
    /// evidence fields.
    pub fn from_parts(
        version: u8,
        kind: u8,
        provider_id: impl Into<Vec<u8>>,
        object_id: impl Into<Vec<u8>>,
        attestation: impl Into<Vec<u8>>,
    ) -> Result<Self, LocalIdentityError> {
        if version != RESTART_STABLE_EVIDENCE_VERSION {
            return Err(LocalIdentityError::UnsupportedEvidenceVersion);
        }
        let kind = RestartStableEvidenceKind::decode(kind)?;
        let claim = Self {
            version,
            kind,
            provider_id: provider_id.into(),
            object_id: object_id.into(),
            attestation: attestation.into(),
        };
        claim.validate()?;
        Ok(claim)
    }

    /// Returns the untrusted provider identifier for verifier use.
    ///
    /// This is deliberately exposed only as claim material, never as issued
    /// durable evidence. Verifiers must treat it as sensitive input.
    #[must_use]
    pub fn provider_id(&self) -> &[u8] {
        &self.provider_id
    }

    /// Returns the untrusted proposed restart-stable object identifier.
    ///
    /// A verifier must independently establish that it names the supplied held
    /// object before durable evidence can be issued.
    #[must_use]
    pub fn object_id(&self) -> &[u8] {
        &self.object_id
    }

    /// Returns the untrusted provider attestation for verifier use.
    ///
    /// Normal formatting remains redacted; verifiers must avoid logging this
    /// sensitive material.
    #[must_use]
    pub fn attestation(&self) -> &[u8] {
        &self.attestation
    }

    fn validate(&self) -> Result<(), LocalIdentityError> {
        if self.version != RESTART_STABLE_EVIDENCE_VERSION
            || self.kind.encoded() != OPAQUE_PROVIDER_EVIDENCE_KIND
            || !bounded_nonempty(&self.provider_id, MAX_IDENTITY_PROVIDER_ID_BYTES)
            || !bounded_nonempty(&self.object_id, MAX_RESTART_STABLE_OBJECT_ID_BYTES)
            || !bounded_nonempty(&self.attestation, MAX_RESTART_STABLE_ATTESTATION_BYTES)
        {
            return Err(LocalIdentityError::InvalidEvidence);
        }
        Ok(())
    }
}

impl fmt::Debug for RestartStableIdentityClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestartStableIdentityClaim")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field("provider_id", &"<redacted>")
            .field("object_id", &"<redacted>")
            .field("attestation", &"<redacted>")
            .finish()
    }
}

/// Opaque evidence issued only after an independent verifier accepts a claim.
///
/// There is deliberately no public constructor. Equality is available for
/// exact identity comparison, while formatting and all errors remain redacted.
#[derive(Clone)]
pub struct RestartStableIdentityEvidence {
    claim: RestartStableIdentityClaim,
    object_kind: myvault_platform_fs::HeldObjectKind,
}

impl RestartStableIdentityEvidence {
    fn append_canonical(&self, stream: &mut Vec<u8>) {
        append_bytes(stream, b"evidence_version", &[self.claim.version]);
        append_bytes(stream, b"evidence_kind", &[self.claim.kind.encoded()]);
        append_bytes(
            stream,
            b"object_kind",
            &[encoded_object_kind(self.object_kind)],
        );
        append_bytes(stream, b"provider_id", &self.claim.provider_id);
        append_bytes(stream, b"object_id", &self.claim.object_id);
    }

    fn identity_fingerprint(&self) -> [u8; 32] {
        let mut stream = Vec::new();
        append_bytes(
            &mut stream,
            b"contract_version",
            IDENTITY_CONTRACT_HASH_VERSION,
        );
        append_bytes(&mut stream, b"domain", b"restart-stable-identity");
        append_bytes(&mut stream, b"evidence_version", &[self.claim.version]);
        append_bytes(&mut stream, b"evidence_kind", &[self.claim.kind.encoded()]);
        append_bytes(
            &mut stream,
            b"object_kind",
            &[encoded_object_kind(self.object_kind)],
        );
        append_bytes(&mut stream, b"provider_id", &self.claim.provider_id);
        append_bytes(&mut stream, b"object_id", &self.claim.object_id);
        sha256(&stream)
    }

    /// Returns the held object kind that the verifier bound to this evidence.
    #[must_use]
    pub const fn object_kind(&self) -> myvault_platform_fs::HeldObjectKind {
        self.object_kind
    }
}

impl PartialEq for RestartStableIdentityEvidence {
    fn eq(&self, other: &Self) -> bool {
        self.claim.version == other.claim.version
            && self.claim.kind == other.claim.kind
            && self.object_kind == other.object_kind
            && self.claim.provider_id == other.claim.provider_id
            && self.claim.object_id == other.claim.object_id
    }
}

impl Eq for RestartStableIdentityEvidence {}

impl fmt::Debug for RestartStableIdentityEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("RestartStableIdentityEvidence(<redacted independently-verified evidence>)")
    }
}

/// Trust boundary for platform/provider-specific restart-stability validation.
///
/// An implementation must independently establish that `claim.object_id` is a
/// restart-stable identity for the same object represented by `held`. Returning
/// `Ok(())` is a trust assertion by that provider; this contract does not make
/// a platform capability claim by itself. The trait is sealed so downstream
/// crates cannot manufacture an accepting verifier; approved provider adapters
/// must be added at this crate's trust boundary.
///
/// ```compile_fail
/// use myvault_sync_engine::local_identity::RestartStableIdentityVerifier;
///
/// struct CallerVerifier;
/// impl RestartStableIdentityVerifier for CallerVerifier {
///     # fn verify_restart_stable_identity(
///     #     &self,
///     #     _: &myvault_platform_fs::HeldObjectIdentityToken,
///     #     _: &myvault_sync_engine::local_identity::RestartStableIdentityClaim,
///     # ) -> Result<(), myvault_sync_engine::local_identity::LocalIdentityError> {
///     #     Ok(())
///     # }
/// }
/// ```
pub trait RestartStableIdentityVerifier: verifier_seal::Sealed {
    /// Validates one untrusted claim against an exact currently-held identity.
    ///
    /// # Errors
    /// Returns a redacted error when independent provider verification fails.
    fn verify_restart_stable_identity(
        &self,
        held: &HeldObjectIdentityToken,
        claim: &RestartStableIdentityClaim,
    ) -> Result<(), LocalIdentityError>;
}

/// Issues opaque durable evidence only after the explicit verifier boundary.
///
/// A held token on its own has no conversion path to durable evidence or a
/// durable execution binding. This function is intentionally the sole public
/// issuance path in this Step 1 contract.
///
/// # Errors
/// Returns a redacted error for malformed claim material or verifier rejection.
pub fn verify_restart_stable_identity<V: RestartStableIdentityVerifier>(
    verifier: &V,
    held: &HeldObjectIdentityToken,
    claim: RestartStableIdentityClaim,
) -> Result<RestartStableIdentityEvidence, LocalIdentityError> {
    claim.validate()?;
    verifier.verify_restart_stable_identity(held, &claim)?;
    Ok(RestartStableIdentityEvidence {
        claim,
        object_kind: held.kind(),
    })
}

mod verifier_seal {
    pub trait Sealed {}
}

macro_rules! identity_role {
    ($name:ident, $description:literal, directory_only) => {
        #[doc = $description]
        #[derive(Clone, Eq, PartialEq)]
        pub struct $name(RestartStableIdentityEvidence);

        impl $name {
            /// Assigns independently-issued durable evidence to this exact role.
            ///
            /// # Errors
            /// Returns a redacted error when the verifier bound a non-directory
            /// object to a role that requires a directory.
            pub fn from_restart_stable(
                evidence: RestartStableIdentityEvidence,
            ) -> Result<Self, LocalIdentityError> {
                if evidence.object_kind() != myvault_platform_fs::HeldObjectKind::Directory {
                    return Err(LocalIdentityError::InvalidRoleIdentity);
                }
                Ok(Self(evidence))
            }

            fn append_canonical(&self, stream: &mut Vec<u8>) {
                self.0.append_canonical(stream);
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "(<redacted durable identity>)"))
            }
        }
    };

    ($name:ident, $description:literal, any_object) => {
        #[doc = $description]
        #[derive(Clone, Eq, PartialEq)]
        pub struct $name(RestartStableIdentityEvidence);

        impl $name {
            /// Assigns independently-issued durable file or directory evidence
            /// to the source-object role while preserving its verified kind.
            #[must_use]
            pub fn from_restart_stable(evidence: RestartStableIdentityEvidence) -> Self {
                Self(evidence)
            }

            /// Returns whether the verified source object is a file or directory.
            #[must_use]
            pub const fn object_kind(&self) -> myvault_platform_fs::HeldObjectKind {
                self.0.object_kind()
            }

            fn append_canonical(&self, stream: &mut Vec<u8>) {
                self.0.append_canonical(stream);
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "(<redacted durable identity>)"))
            }
        }
    };
}

identity_role!(
    VaultRootIdentity,
    "Restart-stable directory identity for the vault/root role; not interchangeable with a parent.",
    directory_only
);
identity_role!(
    SourceParentIdentity,
    "Restart-stable directory identity for the source-parent role; not interchangeable with the destination.",
    directory_only
);
identity_role!(
    SourceObjectIdentity,
    "Restart-stable identity for the source-object role. Content revisions are not object identity.

```compile_fail
use myvault_core::FileRevision;
use myvault_sync_engine::local_identity::SourceObjectIdentity;

let revision = FileRevision::from_bytes(b\"content\");
let _identity = SourceObjectIdentity::from_restart_stable(revision);
```",
    any_object
);
identity_role!(
    DestinationParentIdentity,
    "Restart-stable directory identity for the destination-parent role; not interchangeable with the source.",
    directory_only
);

/// A SHA-256 fingerprint of separately supplied operation-intent bytes.
///
/// This is intentionally distinct from file content/revision evidence. Callers
/// supply the operation's canonical intent preimage; a
/// [`myvault_core::FileRevision`] alone
/// cannot become an object identity through this API.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct IntentFingerprint([u8; 32]);

impl IntentFingerprint {
    /// Domain-separates and hashes bounded canonical operation-intent bytes.
    ///
    /// # Errors
    /// Returns a redacted error when the preimage exceeds its explicit bound.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, LocalIdentityError> {
        if bytes.len() > MAX_INTENT_FINGERPRINT_INPUT_BYTES {
            return Err(LocalIdentityError::IntentTooLarge);
        }
        let mut stream = Vec::new();
        append_bytes(
            &mut stream,
            b"contract_version",
            IDENTITY_CONTRACT_HASH_VERSION,
        );
        append_bytes(&mut stream, b"domain", b"intent-fingerprint");
        append_bytes(&mut stream, b"intent", bytes);
        Ok(Self(sha256(&stream)))
    }

    fn append_canonical(&self, stream: &mut Vec<u8>) {
        append_bytes(stream, b"intent_sha256", &self.0);
    }

    /// Returns the canonical SHA-256 bytes for durable storage.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Derives the local ledger's intent binding from the exact, persisted R3
/// intent-fingerprint string.  This is intentionally crate-private: callers
/// must not be able to substitute an arbitrary local intent for an R3 row.
pub(crate) fn local_intent_fingerprint_from_r3_intent(
    r3_intent_fingerprint: &str,
) -> Result<[u8; 32], LocalIdentityError> {
    Ok(*IntentFingerprint::from_canonical_bytes(r3_intent_fingerprint.as_bytes())?.as_bytes())
}

impl fmt::Debug for IntentFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IntentFingerprint(<sha256>)")
    }
}

/// Target-name and collision-key evidence for one intended destination entry.
#[derive(Clone, Eq, PartialEq)]
pub struct TargetNameEvidence {
    name: String,
    collision_key: String,
}

impl TargetNameEvidence {
    /// Validates one exact name and its required canonical collision key.
    ///
    /// # Errors
    /// Returns a redacted error when the name or its supplied key is invalid or
    /// non-canonical.
    pub fn new(
        name: impl Into<String>,
        collision_key: impl Into<String>,
    ) -> Result<Self, LocalIdentityError> {
        let name = name.into();
        let collision_key = collision_key.into();
        validate_name(&name)?;
        if canonical_collision_key(&name)? != collision_key {
            return Err(LocalIdentityError::InvalidCollisionKey);
        }
        Ok(Self {
            name,
            collision_key,
        })
    }

    fn append_canonical(&self, stream: &mut Vec<u8>) {
        append_bytes(stream, b"target_name", self.name.as_bytes());
        append_bytes(
            stream,
            b"target_collision_key",
            self.collision_key.as_bytes(),
        );
    }
}

impl fmt::Debug for TargetNameEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TargetNameEvidence(<redacted>)")
    }
}

/// One deterministic row in a complete destination collision set.
#[derive(Clone, Eq, PartialEq)]
pub struct CollisionMember {
    name: String,
    collision_key: String,
    object_identity: RestartStableIdentityEvidence,
}

impl CollisionMember {
    /// Creates a row only when the supplied key is the canonical key for name.
    ///
    /// # Errors
    /// Returns a redacted error when the name or collision key is invalid.
    pub fn new(
        name: impl Into<String>,
        collision_key: impl Into<String>,
        object_identity: RestartStableIdentityEvidence,
    ) -> Result<Self, LocalIdentityError> {
        let target = TargetNameEvidence::new(name, collision_key)?;
        Ok(Self {
            name: target.name,
            collision_key: target.collision_key,
            object_identity,
        })
    }

    fn append_canonical(&self, stream: &mut Vec<u8>) {
        append_bytes(stream, b"member_name", self.name.as_bytes());
        append_bytes(
            stream,
            b"member_collision_key",
            self.collision_key.as_bytes(),
        );
        self.object_identity.append_canonical(stream);
    }

    fn canonical_order_key(&self) -> (&str, &str, [u8; 32]) {
        (
            &self.collision_key,
            &self.name,
            self.object_identity.identity_fingerprint(),
        )
    }
}

impl fmt::Debug for CollisionMember {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CollisionMember(<redacted>)")
    }
}

/// A complete, deterministic collision-set capture under one destination parent.
#[derive(Clone, Eq, PartialEq)]
pub struct CollisionSnapshot {
    destination_parent_start: DestinationParentIdentity,
    destination_parent_end: DestinationParentIdentity,
    member_count: u32,
    members: Vec<CollisionMember>,
    fingerprint: CollisionSnapshotFingerprint,
}

impl CollisionSnapshot {
    /// Validates a caller-declared complete, canonically ordered collision set.
    ///
    /// The caller's explicit count must exactly match all supplied rows. Both
    /// held captures of the destination parent must be the same durable role.
    /// This data contract cannot inspect a filesystem and therefore does not by
    /// itself prove that enumeration was exhaustive; a later execution adapter
    /// must revalidate completeness before treating the snapshot as authority.
    ///
    /// # Errors
    /// Returns a redacted error for changed parents, invalid rows, duplicate or
    /// unordered members, oversized sets, or a count mismatch.
    pub fn new(
        destination_parent_start: DestinationParentIdentity,
        destination_parent_end: DestinationParentIdentity,
        member_count: u32,
        members: Vec<CollisionMember>,
    ) -> Result<Self, LocalIdentityError> {
        if members.len() > MAX_COLLISION_SNAPSHOT_MEMBERS {
            return Err(LocalIdentityError::CollisionSnapshotTooLarge);
        }
        if usize::try_from(member_count)
            .map_err(|_| LocalIdentityError::CollisionMemberCountMismatch)?
            != members.len()
        {
            return Err(LocalIdentityError::CollisionMemberCountMismatch);
        }
        if destination_parent_start != destination_parent_end {
            return Err(LocalIdentityError::DestinationParentChanged);
        }
        validate_collision_members(&members)?;
        let mut stream = Vec::new();
        append_bytes(
            &mut stream,
            b"contract_version",
            IDENTITY_CONTRACT_HASH_VERSION,
        );
        append_bytes(&mut stream, b"domain", b"collision-snapshot");
        destination_parent_start.append_canonical(&mut stream);
        destination_parent_end.append_canonical(&mut stream);
        append_bytes(&mut stream, b"member_count", &member_count.to_be_bytes());
        for member in &members {
            member.append_canonical(&mut stream);
        }
        Ok(Self {
            destination_parent_start,
            destination_parent_end,
            member_count,
            members,
            fingerprint: CollisionSnapshotFingerprint(sha256(&stream)),
        })
    }

    /// Returns the deterministic redacted fingerprint of this complete set.
    #[must_use]
    pub const fn fingerprint(&self) -> CollisionSnapshotFingerprint {
        self.fingerprint
    }

    /// Returns the verified number of member rows.
    #[must_use]
    pub const fn member_count(&self) -> u32 {
        self.member_count
    }

    fn matches_destination_parent(&self, destination: &DestinationParentIdentity) -> bool {
        &self.destination_parent_start == destination && &self.destination_parent_end == destination
    }

    fn append_canonical(&self, stream: &mut Vec<u8>) {
        self.destination_parent_start.append_canonical(stream);
        self.destination_parent_end.append_canonical(stream);
        append_bytes(stream, b"member_count", &self.member_count.to_be_bytes());
        for member in &self.members {
            member.append_canonical(stream);
        }
    }
}

impl fmt::Debug for CollisionSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CollisionSnapshot")
            .field("destination_parent_start", &"<redacted>")
            .field("destination_parent_end", &"<redacted>")
            .field("member_count", &self.member_count)
            .field("members", &"<redacted>")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Redacted SHA-256 fingerprint of canonical collision snapshot evidence.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct CollisionSnapshotFingerprint([u8; 32]);

impl fmt::Debug for CollisionSnapshotFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CollisionSnapshotFingerprint(<sha256>)")
    }
}

impl CollisionSnapshotFingerprint {
    /// Returns the canonical SHA-256 bytes for durable storage.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Immutable evidence bound to one prospective durable execution.
///
/// Constructing this validates data only. It does not authorize a mutation,
/// reserve a name, or claim a platform supports durable execution.
#[derive(Debug)]
pub struct DurableExecutionBindingInput {
    pub operation_id: Uuid,
    pub vault_id: Uuid,
    pub intent: IntentFingerprint,
    pub vault_root: VaultRootIdentity,
    pub source_parent: SourceParentIdentity,
    pub source_object: SourceObjectIdentity,
    pub destination_parent: DestinationParentIdentity,
    pub target: TargetNameEvidence,
    pub collision_snapshot: CollisionSnapshot,
}

/// Immutable evidence bound to one prospective durable execution.
///
/// Constructing this validates data only. It does not authorize a mutation,
/// reserve a name, or claim a platform supports durable execution.
#[derive(Clone, Eq, PartialEq)]
pub struct DurableExecutionBinding {
    operation_id: Uuid,
    vault_id: Uuid,
    intent: IntentFingerprint,
    vault_root: VaultRootIdentity,
    source_parent: SourceParentIdentity,
    source_object: SourceObjectIdentity,
    destination_parent: DestinationParentIdentity,
    target: TargetNameEvidence,
    collision_snapshot: CollisionSnapshot,
    fingerprint: DurableExecutionBindingFingerprint,
}

impl DurableExecutionBinding {
    /// Binds all typed durable identity roles and exhaustive collision evidence.
    ///
    /// # Errors
    /// Returns a redacted error when identifiers are nil or collision evidence
    /// was captured under a different destination parent.
    pub fn new(input: DurableExecutionBindingInput) -> Result<Self, LocalIdentityError> {
        if input.operation_id.is_nil() {
            return Err(LocalIdentityError::InvalidOperationId);
        }
        if input.vault_id.is_nil() {
            return Err(LocalIdentityError::InvalidVaultId);
        }
        if !input
            .collision_snapshot
            .matches_destination_parent(&input.destination_parent)
        {
            return Err(LocalIdentityError::DestinationParentMismatch);
        }
        let mut stream = Vec::new();
        append_bytes(
            &mut stream,
            b"contract_version",
            IDENTITY_CONTRACT_HASH_VERSION,
        );
        append_bytes(&mut stream, b"domain", b"durable-execution-binding");
        append_bytes(&mut stream, b"operation_id", input.operation_id.as_bytes());
        append_bytes(&mut stream, b"vault_id", input.vault_id.as_bytes());
        input.intent.append_canonical(&mut stream);
        input.vault_root.append_canonical(&mut stream);
        input.source_parent.append_canonical(&mut stream);
        input.source_object.append_canonical(&mut stream);
        input.destination_parent.append_canonical(&mut stream);
        input.target.append_canonical(&mut stream);
        input.collision_snapshot.append_canonical(&mut stream);
        Ok(Self {
            operation_id: input.operation_id,
            vault_id: input.vault_id,
            intent: input.intent,
            vault_root: input.vault_root,
            source_parent: input.source_parent,
            source_object: input.source_object,
            destination_parent: input.destination_parent,
            target: input.target,
            collision_snapshot: input.collision_snapshot,
            fingerprint: DurableExecutionBindingFingerprint(sha256(&stream)),
        })
    }

    /// Returns the deterministic redacted fingerprint of every bound field.
    #[must_use]
    pub const fn fingerprint(&self) -> DurableExecutionBindingFingerprint {
        self.fingerprint
    }

    /// Returns the crate-private durable-storage projection of verifier-issued
    /// evidence. This projection is deliberately one-way: `SQLite` rows are
    /// untrusted data and cannot reconstruct `RestartStableIdentityEvidence`
    /// without the sealed verifier path.
    pub(crate) fn persistence_projection(&self) -> DurableExecutionBindingProjection<'_> {
        DurableExecutionBindingProjection {
            operation_id: self.operation_id,
            vault_id: self.vault_id,
            intent_fingerprint: self.intent.0,
            contract_fingerprint: self.fingerprint.0,
            vault_root: PersistedIdentityEvidence::from_evidence("vault_root", &self.vault_root.0),
            source_parent: PersistedIdentityEvidence::from_evidence(
                "source_parent",
                &self.source_parent.0,
            ),
            source_object: PersistedIdentityEvidence::from_evidence(
                "source_object",
                &self.source_object.0,
            ),
            destination_parent: PersistedIdentityEvidence::from_evidence(
                "destination_parent",
                &self.destination_parent.0,
            ),
            collision_parent_start: PersistedIdentityEvidence::from_evidence(
                "collision_parent_start",
                &self.collision_snapshot.destination_parent_start.0,
            ),
            collision_parent_end: PersistedIdentityEvidence::from_evidence(
                "collision_parent_end",
                &self.collision_snapshot.destination_parent_end.0,
            ),
            target_name: &self.target.name,
            target_collision_key: &self.target.collision_key,
            collision_member_count: self.collision_snapshot.member_count,
            collision_snapshot_fingerprint: self.collision_snapshot.fingerprint.0,
            collision_members: self
                .collision_snapshot
                .members
                .iter()
                .map(|member| PersistedCollisionMember {
                    name: &member.name,
                    collision_key: &member.collision_key,
                    identity: PersistedIdentityEvidence::from_evidence(
                        "collision_member",
                        &member.object_identity,
                    ),
                })
                .collect(),
        }
    }
}

impl fmt::Debug for DurableExecutionBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableExecutionBinding")
            .field("operation_id", &self.operation_id)
            .field("vault_id", &self.vault_id)
            .field("intent", &"<redacted>")
            .field("vault_root", &"<redacted>")
            .field("source_parent", &"<redacted>")
            .field("source_object", &"<redacted>")
            .field("destination_parent", &"<redacted>")
            .field("target", &"<redacted>")
            .field("collision_snapshot", &"<redacted>")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Redacted SHA-256 fingerprint of a complete durable execution binding.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DurableExecutionBindingFingerprint([u8; 32]);

impl fmt::Debug for DurableExecutionBindingFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DurableExecutionBindingFingerprint(<sha256>)")
    }
}

impl DurableExecutionBindingFingerprint {
    /// Returns the canonical SHA-256 bytes for durable storage.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One-way, crate-private projection used only to persist a verified binding.
/// It contains verifier-issued material but has no constructor from database
/// rows and never crosses the public store read boundary.
pub(crate) struct DurableExecutionBindingProjection<'a> {
    pub operation_id: Uuid,
    pub vault_id: Uuid,
    pub intent_fingerprint: [u8; 32],
    pub contract_fingerprint: [u8; 32],
    pub vault_root: PersistedIdentityEvidence<'a>,
    pub source_parent: PersistedIdentityEvidence<'a>,
    pub source_object: PersistedIdentityEvidence<'a>,
    pub destination_parent: PersistedIdentityEvidence<'a>,
    pub collision_parent_start: PersistedIdentityEvidence<'a>,
    pub collision_parent_end: PersistedIdentityEvidence<'a>,
    pub target_name: &'a str,
    pub target_collision_key: &'a str,
    pub collision_member_count: u32,
    pub collision_snapshot_fingerprint: [u8; 32],
    pub collision_members: Vec<PersistedCollisionMember<'a>>,
}

/// One immutable verifier-issued identity record for the storage boundary.
pub(crate) struct PersistedIdentityEvidence<'a> {
    pub role: &'static str,
    pub version: u8,
    pub kind: u8,
    pub object_kind: u8,
    pub provider_id: &'a [u8],
    pub object_id: &'a [u8],
    pub attestation: &'a [u8],
    pub stable_identity_fingerprint: [u8; 32],
}

impl<'a> PersistedIdentityEvidence<'a> {
    fn from_evidence(role: &'static str, evidence: &'a RestartStableIdentityEvidence) -> Self {
        Self {
            role,
            version: evidence.claim.version,
            kind: evidence.claim.kind.encoded(),
            object_kind: encoded_object_kind(evidence.object_kind),
            provider_id: &evidence.claim.provider_id,
            object_id: &evidence.claim.object_id,
            attestation: &evidence.claim.attestation,
            stable_identity_fingerprint: evidence.identity_fingerprint(),
        }
    }
}

/// One immutable collision-member projection for the storage boundary.
pub(crate) struct PersistedCollisionMember<'a> {
    pub name: &'a str,
    pub collision_key: &'a str,
    pub identity: PersistedIdentityEvidence<'a>,
}

const fn encoded_object_kind(kind: myvault_platform_fs::HeldObjectKind) -> u8 {
    match kind {
        myvault_platform_fs::HeldObjectKind::Directory => 1,
        myvault_platform_fs::HeldObjectKind::File => 2,
    }
}

fn validate_collision_members(members: &[CollisionMember]) -> Result<(), LocalIdentityError> {
    for (index, member) in members.iter().enumerate() {
        validate_name(&member.name)?;
        if canonical_collision_key(&member.name)? != member.collision_key {
            return Err(LocalIdentityError::InvalidCollisionKey);
        }
        if members[..index]
            .iter()
            .any(|previous| previous.name == member.name)
        {
            return Err(LocalIdentityError::DuplicateCollisionMemberName);
        }
        if members[..index]
            .iter()
            .any(|previous| previous.object_identity == member.object_identity)
        {
            return Err(LocalIdentityError::DuplicateCollisionMemberIdentity);
        }
        if index > 0 && members[index - 1].canonical_order_key() >= member.canonical_order_key() {
            return Err(LocalIdentityError::NonCanonicalCollisionMemberOrder);
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), LocalIdentityError> {
    if !bounded_nonempty(name.as_bytes(), MAX_LOCAL_IDENTITY_NAME_BYTES)
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '\0') || character.is_control())
    {
        return Err(LocalIdentityError::InvalidTargetName);
    }
    Ok(())
}

fn canonical_collision_key(name: &str) -> Result<String, LocalIdentityError> {
    validate_name(name)?;
    let canonical: String = name.nfkc().case_fold().nfkc().collect();
    if !bounded_nonempty(canonical.as_bytes(), MAX_LOCAL_IDENTITY_COLLISION_KEY_BYTES)
        || canonical
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '\0') || character.is_control())
    {
        return Err(LocalIdentityError::InvalidCollisionKey);
    }
    Ok(canonical)
}

/// Recomputes the durable identity hash from its persisted preimage.  This is
/// crate-private so reopening storage can verify bytes without treating rows
/// as verifier-issued evidence; attestation is intentionally excluded.
pub(crate) fn persisted_stable_identity_fingerprint(
    version: u8,
    kind: u8,
    object_kind: u8,
    provider_id: &[u8],
    object_id: &[u8],
) -> Result<[u8; 32], LocalIdentityError> {
    if version != RESTART_STABLE_EVIDENCE_VERSION {
        return Err(LocalIdentityError::UnsupportedEvidenceVersion);
    }
    RestartStableEvidenceKind::decode(kind)?;
    if !matches!(object_kind, 1 | 2)
        || !bounded_nonempty(provider_id, MAX_IDENTITY_PROVIDER_ID_BYTES)
        || !bounded_nonempty(object_id, MAX_RESTART_STABLE_OBJECT_ID_BYTES)
    {
        return Err(LocalIdentityError::InvalidEvidence);
    }
    let mut stream = Vec::new();
    append_bytes(
        &mut stream,
        b"contract_version",
        IDENTITY_CONTRACT_HASH_VERSION,
    );
    append_bytes(&mut stream, b"domain", b"restart-stable-identity");
    append_bytes(&mut stream, b"evidence_version", &[version]);
    append_bytes(&mut stream, b"evidence_kind", &[kind]);
    append_bytes(&mut stream, b"object_kind", &[object_kind]);
    append_bytes(&mut stream, b"provider_id", provider_id);
    append_bytes(&mut stream, b"object_id", object_id);
    Ok(sha256(&stream))
}

/// Applies exactly the collision-key canonicalization used by constructors to
/// a persisted name during semantic reopen validation.
pub(crate) fn persisted_canonical_collision_key(name: &str) -> Result<String, LocalIdentityError> {
    canonical_collision_key(name)
}

fn bounded_nonempty(bytes: &[u8], maximum: usize) -> bool {
    (1..=maximum).contains(&bytes.len())
}

fn append_bytes(stream: &mut Vec<u8>, field: &[u8], value: &[u8]) {
    stream.extend_from_slice(&(field.len() as u64).to_be_bytes());
    stream.extend_from_slice(field);
    stream.extend_from_slice(&(value.len() as u64).to_be_bytes());
    stream.extend_from_slice(value);
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
pub(crate) fn test_durable_execution_binding(
    operation_id: Uuid,
    vault_id: Uuid,
) -> DurableExecutionBinding {
    test_durable_execution_binding_with_intent_and_attestation(
        operation_id,
        vault_id,
        b"test local execution intent",
        0,
    )
}

/// Test-only reissuance of the same stable identities with independently
/// rotated attestation material.  Storage must retain first issuance bytes.
#[cfg(test)]
pub(crate) fn test_durable_execution_binding_with_attestation_offset(
    operation_id: Uuid,
    vault_id: Uuid,
    attestation_offset: u8,
) -> DurableExecutionBinding {
    test_durable_execution_binding_with_intent_and_attestation(
        operation_id,
        vault_id,
        b"test local execution intent",
        attestation_offset,
    )
}

#[cfg(test)]
pub(crate) fn test_durable_execution_binding_for_r3_intent(
    operation_id: Uuid,
    vault_id: Uuid,
    r3_intent_fingerprint: &str,
) -> DurableExecutionBinding {
    // This is the explicit bridge relation: local intent is the local
    // canonical hash of the exact R3 mutation intent fingerprint string.
    test_durable_execution_binding_with_intent_and_attestation(
        operation_id,
        vault_id,
        r3_intent_fingerprint.as_bytes(),
        0,
    )
}

#[cfg(test)]
fn test_durable_execution_binding_with_intent_and_attestation(
    operation_id: Uuid,
    vault_id: Uuid,
    intent_bytes: &[u8],
    attestation_offset: u8,
) -> DurableExecutionBinding {
    struct TestVerifier;

    impl verifier_seal::Sealed for TestVerifier {}

    impl RestartStableIdentityVerifier for TestVerifier {
        fn verify_restart_stable_identity(
            &self,
            _held: &HeldObjectIdentityToken,
            _claim: &RestartStableIdentityClaim,
        ) -> Result<(), LocalIdentityError> {
            Ok(())
        }
    }

    fn issued(
        verifier: &TestVerifier,
        kind: u8,
        object: u8,
        attestation_offset: u8,
    ) -> RestartStableIdentityEvidence {
        let held = HeldObjectIdentityToken::from_canonical_bytes(&[
            1, kind, 0, 0, 0, 0, 0, 0, 0, 7, object, object, object, object, object, object,
            object, object, object, object, object, object, object, object, object, object,
        ])
        .expect("test held token");
        verify_restart_stable_identity(
            verifier,
            &held,
            RestartStableIdentityClaim::opaque_provider_token(
                b"test-provider".to_vec(),
                vec![object],
                vec![object.wrapping_add(1).wrapping_add(attestation_offset)],
            )
            .expect("test claim"),
        )
        .expect("test evidence")
    }

    let verifier = TestVerifier;
    let vault_root =
        VaultRootIdentity::from_restart_stable(issued(&verifier, 1, 1, attestation_offset))
            .expect("vault root");
    let source_parent =
        SourceParentIdentity::from_restart_stable(issued(&verifier, 1, 2, attestation_offset))
            .expect("source parent");
    let source_object =
        SourceObjectIdentity::from_restart_stable(issued(&verifier, 2, 3, attestation_offset));
    let destination =
        DestinationParentIdentity::from_restart_stable(issued(&verifier, 1, 4, attestation_offset))
            .expect("destination parent");
    let target = TargetNameEvidence::new(
        "new-note.md",
        canonical_collision_key("new-note.md").expect("key"),
    )
    .expect("target");
    let member = CollisionMember::new(
        "existing-note.md",
        canonical_collision_key("existing-note.md").expect("key"),
        issued(&verifier, 2, 5, attestation_offset),
    )
    .expect("member");
    let snapshot =
        CollisionSnapshot::new(destination.clone(), destination.clone(), 1, vec![member])
            .expect("snapshot");
    DurableExecutionBinding::new(DurableExecutionBindingInput {
        operation_id,
        vault_id,
        intent: IntentFingerprint::from_canonical_bytes(intent_bytes).expect("intent"),
        vault_root,
        source_parent,
        source_object,
        destination_parent: destination,
        target,
        collision_snapshot: snapshot,
    })
    .expect("binding")
}

#[cfg(test)]
mod tests {
    use super::*;
    use myvault_platform_fs::HeldObjectIdentityToken;

    struct AcceptingVerifier;

    impl verifier_seal::Sealed for AcceptingVerifier {}

    impl RestartStableIdentityVerifier for AcceptingVerifier {
        fn verify_restart_stable_identity(
            &self,
            _held: &HeldObjectIdentityToken,
            _claim: &RestartStableIdentityClaim,
        ) -> Result<(), LocalIdentityError> {
            Ok(())
        }
    }

    struct RejectingVerifier;

    impl verifier_seal::Sealed for RejectingVerifier {}

    impl RestartStableIdentityVerifier for RejectingVerifier {
        fn verify_restart_stable_identity(
            &self,
            _held: &HeldObjectIdentityToken,
            _claim: &RestartStableIdentityClaim,
        ) -> Result<(), LocalIdentityError> {
            Err(LocalIdentityError::VerificationRejected)
        }
    }

    fn held() -> HeldObjectIdentityToken {
        held_kind(1)
    }

    fn held_kind(kind: u8) -> HeldObjectIdentityToken {
        HeldObjectIdentityToken::from_canonical_bytes(&[
            1, kind, 0, 0, 0, 0, 0, 0, 0, 7, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
        ])
        .expect("held token")
    }

    fn evidence(byte: u8) -> RestartStableIdentityEvidence {
        evidence_with_attestation(byte, byte.wrapping_add(1))
    }

    fn evidence_with_attestation(object_id: u8, attestation: u8) -> RestartStableIdentityEvidence {
        evidence_with_kind_and_attestation(1, object_id, attestation)
    }

    fn evidence_with_kind_and_attestation(
        kind: u8,
        object_id: u8,
        attestation: u8,
    ) -> RestartStableIdentityEvidence {
        verify_restart_stable_identity(
            &AcceptingVerifier,
            &held_kind(kind),
            RestartStableIdentityClaim::opaque_provider_token(
                b"provider".to_vec(),
                vec![object_id],
                vec![attestation],
            )
            .expect("claim"),
        )
        .expect("verified evidence")
    }

    fn target(name: &str) -> TargetNameEvidence {
        TargetNameEvidence::new(name, canonical_collision_key(name).expect("key")).expect("target")
    }

    fn member(name: &str, byte: u8) -> CollisionMember {
        CollisionMember::new(
            name,
            canonical_collision_key(name).expect("key"),
            evidence(byte),
        )
        .expect("member")
    }

    fn destination(byte: u8) -> DestinationParentIdentity {
        DestinationParentIdentity::from_restart_stable(evidence(byte)).expect("directory evidence")
    }

    #[test]
    fn held_runtime_token_cannot_self_issue_durable_evidence() {
        let claim = RestartStableIdentityClaim::opaque_provider_token(
            b"provider".to_vec(),
            b"object".to_vec(),
            b"attestation".to_vec(),
        )
        .expect("bounded claim");
        assert_eq!(
            verify_restart_stable_identity(&RejectingVerifier, &held(), claim),
            Err(LocalIdentityError::VerificationRejected)
        );
        // `DurableExecutionBinding::new` accepts typed restart-stable roles,
        // not HeldObjectIdentityToken, so a held token alone is type-impossible
        // to bind without the verifier issuance path above.
    }

    #[test]
    fn evidence_claim_is_bounded_versioned_and_redacted() {
        assert_eq!(
            RestartStableIdentityClaim::from_parts(2, 1, vec![1], vec![2], vec![3]),
            Err(LocalIdentityError::UnsupportedEvidenceVersion)
        );
        assert_eq!(
            RestartStableIdentityClaim::from_parts(1, 2, vec![1], vec![2], vec![3]),
            Err(LocalIdentityError::UnsupportedEvidenceKind)
        );
        assert_eq!(
            RestartStableIdentityClaim::opaque_provider_token(
                vec![1; MAX_IDENTITY_PROVIDER_ID_BYTES + 1],
                vec![2],
                vec![3]
            ),
            Err(LocalIdentityError::InvalidEvidence)
        );
        let claim = RestartStableIdentityClaim::opaque_provider_token(
            b"provider-secret".to_vec(),
            b"object-secret".to_vec(),
            b"attestation-secret".to_vec(),
        )
        .expect("claim");
        let debug = format!("{claim:?}");
        assert!(!debug.contains("secret"));
        assert!(!LocalIdentityError::InvalidEvidence
            .to_string()
            .contains("secret"));
    }

    #[test]
    fn verified_kind_is_bound_and_directory_roles_reject_files() {
        let directory = evidence_with_kind_and_attestation(1, 7, 1);
        let file = evidence_with_kind_and_attestation(2, 7, 1);

        assert_eq!(
            directory.object_kind(),
            myvault_platform_fs::HeldObjectKind::Directory
        );
        assert_eq!(
            file.object_kind(),
            myvault_platform_fs::HeldObjectKind::File
        );
        assert_eq!(
            VaultRootIdentity::from_restart_stable(file.clone()),
            Err(LocalIdentityError::InvalidRoleIdentity)
        );
        assert_eq!(
            SourceParentIdentity::from_restart_stable(file.clone()),
            Err(LocalIdentityError::InvalidRoleIdentity)
        );
        assert_eq!(
            DestinationParentIdentity::from_restart_stable(file.clone()),
            Err(LocalIdentityError::InvalidRoleIdentity)
        );
        assert_eq!(
            SourceObjectIdentity::from_restart_stable(file).object_kind(),
            myvault_platform_fs::HeldObjectKind::File
        );
        assert!(VaultRootIdentity::from_restart_stable(directory).is_ok());
    }

    #[test]
    fn attestation_rotation_does_not_change_stable_object_identity() {
        let first = evidence_with_attestation(7, 1);
        let rotated = evidence_with_attestation(7, 2);

        assert_eq!(first, rotated);
        assert_eq!(first.identity_fingerprint(), rotated.identity_fingerprint());
        let first_member =
            CollisionMember::new("item", canonical_collision_key("item").expect("key"), first)
                .expect("member");
        let rotated_member = CollisionMember::new(
            "item",
            canonical_collision_key("item").expect("key"),
            rotated,
        )
        .expect("member");
        assert_eq!(
            CollisionSnapshot::new(destination(8), destination(8), 1, vec![first_member])
                .expect("snapshot")
                .fingerprint(),
            CollisionSnapshot::new(destination(8), destination(8), 1, vec![rotated_member])
                .expect("snapshot")
                .fingerprint()
        );
    }

    #[test]
    fn roles_are_distinct_and_collision_snapshot_rejects_invalid_sets() {
        let start = destination(8);
        let end = destination(8);
        assert_eq!(
            CollisionSnapshot::new(start.clone(), end.clone(), 2, vec![member("b", 2)]),
            Err(LocalIdentityError::CollisionMemberCountMismatch)
        );
        assert_eq!(
            CollisionSnapshot::new(
                start.clone(),
                end.clone(),
                2,
                vec![member("b", 2), member("a", 1)]
            ),
            Err(LocalIdentityError::NonCanonicalCollisionMemberOrder)
        );
        assert_eq!(
            CollisionSnapshot::new(
                start.clone(),
                end.clone(),
                2,
                vec![member("a", 1), member("a", 2)]
            ),
            Err(LocalIdentityError::DuplicateCollisionMemberName)
        );
        let duplicate_identity = CollisionMember::new(
            "b",
            canonical_collision_key("b").expect("key"),
            evidence_with_attestation(1, 99),
        )
        .expect("member");
        assert_eq!(
            CollisionSnapshot::new(start, end, 2, vec![member("a", 1), duplicate_identity]),
            Err(LocalIdentityError::DuplicateCollisionMemberIdentity)
        );
        // VaultRootIdentity, SourceParentIdentity, SourceObjectIdentity, and
        // DestinationParentIdentity are separate types, so they cannot be
        // substituted at a DurableExecutionBinding call site.
    }

    #[test]
    fn collision_keys_and_fingerprints_are_canonical_and_sensitive_data_is_redacted() {
        assert_eq!(
            TargetNameEvidence::new("Résumé", "resume"),
            Err(LocalIdentityError::InvalidCollisionKey)
        );
        let first = CollisionSnapshot::new(
            destination(8),
            destination(8),
            2,
            vec![member("Alpha", 1), member("Beta", 2)],
        )
        .expect("snapshot");
        let repeated = CollisionSnapshot::new(
            destination(8),
            destination(8),
            2,
            vec![member("Alpha", 1), member("Beta", 2)],
        )
        .expect("snapshot");
        let changed = CollisionSnapshot::new(
            destination(8),
            destination(8),
            2,
            vec![member("Alpha", 1), member("Gamma", 2)],
        )
        .expect("snapshot");
        let changed_identity = CollisionSnapshot::new(
            destination(8),
            destination(8),
            2,
            vec![member("Alpha", 1), member("Beta", 3)],
        )
        .expect("snapshot");
        let changed_parent = CollisionSnapshot::new(
            destination(9),
            destination(9),
            2,
            vec![member("Alpha", 1), member("Beta", 2)],
        )
        .expect("snapshot");
        let changed_count = CollisionSnapshot::new(
            destination(8),
            destination(8),
            3,
            vec![member("Alpha", 1), member("Beta", 2), member("Gamma", 3)],
        )
        .expect("snapshot");
        assert_eq!(first.fingerprint(), repeated.fingerprint());
        assert_ne!(first.fingerprint(), changed.fingerprint());
        assert_ne!(first.fingerprint(), changed_identity.fingerprint());
        assert_ne!(first.fingerprint(), changed_parent.fingerprint());
        assert_ne!(first.fingerprint(), changed_count.fingerprint());
        let debug = format!("{:?}", member("private-name", 5));
        assert!(!debug.contains("private-name"));
    }

    #[test]
    fn snapshot_parent_and_binding_destination_mismatches_fail_closed() {
        assert_eq!(
            CollisionSnapshot::new(destination(1), destination(2), 0, Vec::new()),
            Err(LocalIdentityError::DestinationParentChanged)
        );
        let snapshot = CollisionSnapshot::new(destination(8), destination(8), 0, Vec::new())
            .expect("snapshot");
        let result = DurableExecutionBinding::new(DurableExecutionBindingInput {
            operation_id: Uuid::from_u128(1),
            vault_id: Uuid::from_u128(2),
            intent: IntentFingerprint::from_canonical_bytes(b"intent").expect("intent"),
            vault_root: VaultRootIdentity::from_restart_stable(evidence(3))
                .expect("directory evidence"),
            source_parent: SourceParentIdentity::from_restart_stable(evidence(4))
                .expect("directory evidence"),
            source_object: SourceObjectIdentity::from_restart_stable(evidence(5)),
            destination_parent: destination(9),
            target: target("private-name"),
            collision_snapshot: snapshot,
        });
        assert_eq!(result, Err(LocalIdentityError::DestinationParentMismatch));
    }

    #[test]
    fn intent_and_binding_fingerprints_are_deterministic_and_complete() {
        let intent = IntentFingerprint::from_canonical_bytes(b"intent").expect("intent");
        assert_eq!(
            intent,
            IntentFingerprint::from_canonical_bytes(b"intent").expect("same intent")
        );
        assert_ne!(
            intent,
            IntentFingerprint::from_canonical_bytes(b"other").expect("other intent")
        );
        assert_eq!(
            IntentFingerprint::from_canonical_bytes(&vec![
                0;
                MAX_INTENT_FINGERPRINT_INPUT_BYTES + 1
            ]),
            Err(LocalIdentityError::IntentTooLarge)
        );

        let binding = |target_name: &str| {
            DurableExecutionBinding::new(DurableExecutionBindingInput {
                operation_id: Uuid::from_u128(1),
                vault_id: Uuid::from_u128(2),
                intent,
                vault_root: VaultRootIdentity::from_restart_stable(evidence(3))
                    .expect("directory evidence"),
                source_parent: SourceParentIdentity::from_restart_stable(evidence(4))
                    .expect("directory evidence"),
                source_object: SourceObjectIdentity::from_restart_stable(evidence(5)),
                destination_parent: destination(8),
                target: target(target_name),
                collision_snapshot: CollisionSnapshot::new(
                    destination(8),
                    destination(8),
                    0,
                    Vec::new(),
                )
                .expect("snapshot"),
            })
            .expect("binding")
        };
        let first = binding("result");
        let second = binding("result");
        let changed = binding("other");
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_ne!(first.fingerprint(), changed.fingerprint());
        assert!(!format!("{first:?}").contains("result"));
    }
}
