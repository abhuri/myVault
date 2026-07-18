package com.abhuri.myvault.vaultsaf

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SafConflictCapabilityTest {
    private val attestation = SafProviderAttestation(
        authority = "com.example.documents",
        packageName = "com.example.provider",
        signingCertificateSetSha256 = "a".repeat(64),
        longVersionCode = 42,
        sdkInt = 36,
        buildIdentitySha256 = "b".repeat(64),
        treeDocumentId = "primary:Vault",
        persistedReadWrite = true,
    )

    private val exactAllowlist = setOf(
        SafProviderAllowlistEntry(
            authority = attestation.authority,
            packageName = attestation.packageName,
            signingCertificateSetSha256 = attestation.signingCertificateSetSha256,
            longVersionCode = attestation.longVersionCode,
            sdkInt = attestation.sdkInt,
            buildIdentitySha256 = attestation.buildIdentitySha256,
        ),
    )

    @Test
    fun shippedAllowlistFailsClosedForEveryProvider() {
        val decision = decideSafConflictCapability(attestation)

        assertFalse(decision.eligible)
        assertEquals(SafConflictCapabilityDenial.UNSUPPORTED_PROVIDER, decision.denial)
    }

    @Test
    fun exactTestAllowlistRequiresEveryPinnedProviderPlatformIdentity() {
        assertTrue(decideSafConflictCapability(attestation, exactAllowlist).eligible)
        for (substituted in listOf(
            attestation.copy(authority = "com.attacker.documents"),
            attestation.copy(packageName = "com.attacker.provider"),
            attestation.copy(signingCertificateSetSha256 = "c".repeat(64)),
            attestation.copy(longVersionCode = 43),
            attestation.copy(sdkInt = 35),
            attestation.copy(buildIdentitySha256 = "d".repeat(64)),
        )) {
            val decision = decideSafConflictCapability(substituted, exactAllowlist)
            assertFalse(decision.eligible)
            assertEquals(SafConflictCapabilityDenial.UNSUPPORTED_PROVIDER, decision.denial)
        }
    }

    @Test
    fun incompleteHeldRootOrPermissionEvidenceIsNotEligible() {
        for (unproven in listOf(
            attestation.copy(treeDocumentId = ""),
            attestation.copy(persistedReadWrite = false),
            attestation.copy(signingCertificateSetSha256 = "A".repeat(64)),
        )) {
            val decision = decideSafConflictCapability(unproven, exactAllowlist)
            assertFalse(decision.eligible)
        }
        assertEquals(
            SafConflictCapabilityDenial.MISSING_PERSISTED_READ_WRITE,
            decideSafConflictCapability(attestation.copy(persistedReadWrite = false), exactAllowlist).denial,
        )
    }
}
