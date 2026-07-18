package com.abhuri.myvault.vaultsaf

/**
 * Immutable evidence bound to the exact persisted SAF tree that was inspected.
 *
 * This is intentionally a capability *attestation*, not permission to mutate.
 * R3.4 must keep the allowlist empty until a provider's no-replace and final
 * outcome semantics have been proven independently.
 */
internal data class SafProviderAttestation(
    val authority: String,
    val packageName: String,
    val signingCertificateSetSha256: String,
    val longVersionCode: Long,
    val sdkInt: Int,
    val buildIdentitySha256: String,
    val treeDocumentId: String,
    val persistedReadWrite: Boolean,
)

/** Exact provider/platform tuple that a future, separately approved contract
 * would need to pin. The held tree and persisted grant are attested per call,
 * rather than being static allowlist values. */
internal data class SafProviderAllowlistEntry(
    val authority: String,
    val packageName: String,
    val signingCertificateSetSha256: String,
    val longVersionCode: Long,
    val sdkInt: Int,
    val buildIdentitySha256: String,
)

internal enum class SafConflictCapabilityDenial(val wireValue: String) {
    INVALID_ATTESTATION("invalidAttestation"),
    MISSING_PERSISTED_READ_WRITE("missingPersistedReadWrite"),
    UNSUPPORTED_PROVIDER("unsupportedProvider"),
}

internal data class SafConflictCapabilityDecision(
    val eligible: Boolean,
    val denial: SafConflictCapabilityDenial?,
)

// Deliberately empty. Adding an entry is a safety-contract change and requires
// proof of provider-specific atomic no-replace and authoritative final outcome.
internal val R34_PROVIDER_ALLOWLIST: Set<SafProviderAllowlistEntry> = emptySet()

internal fun decideSafConflictCapability(
    attestation: SafProviderAttestation,
    allowlist: Set<SafProviderAllowlistEntry> = R34_PROVIDER_ALLOWLIST,
): SafConflictCapabilityDecision {
    if (!attestation.isCanonical()) {
        return SafConflictCapabilityDecision(false, SafConflictCapabilityDenial.INVALID_ATTESTATION)
    }
    if (!attestation.persistedReadWrite) {
        return SafConflictCapabilityDecision(false, SafConflictCapabilityDenial.MISSING_PERSISTED_READ_WRITE)
    }
    val exactMatch = allowlist.any {
        it.authority == attestation.authority &&
            it.packageName == attestation.packageName &&
            it.signingCertificateSetSha256 == attestation.signingCertificateSetSha256 &&
            it.longVersionCode == attestation.longVersionCode &&
            it.sdkInt == attestation.sdkInt &&
            it.buildIdentitySha256 == attestation.buildIdentitySha256
    }
    return if (exactMatch) {
        SafConflictCapabilityDecision(true, null)
    } else {
        SafConflictCapabilityDecision(false, SafConflictCapabilityDenial.UNSUPPORTED_PROVIDER)
    }
}

private fun SafProviderAttestation.isCanonical(): Boolean =
    authority.isNotBlank() &&
        packageName.isNotBlank() &&
        treeDocumentId.isNotBlank() &&
        longVersionCode >= 0 &&
        sdkInt >= 1 &&
        LOWER_HEX_256.matches(signingCertificateSetSha256) &&
        LOWER_HEX_256.matches(buildIdentitySha256)

private val LOWER_HEX_256 = Regex("^[0-9a-f]{64}$")
