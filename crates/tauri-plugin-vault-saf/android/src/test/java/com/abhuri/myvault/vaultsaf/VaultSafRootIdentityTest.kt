package com.abhuri.myvault.vaultsaf

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class VaultSafRootIdentityTest {
    @Test
    fun rootIdentityIsStableDomainSeparatedAndCanonical() {
        val root = "content://com.example.documents/tree/primary%3AVault"
        val identity = stableRootIdentityHex(root)

        assertEquals(
            "80fb38cc804fa63dafa5398d5f78c02a505c616f56fe7f8b744947ae85816c30",
            identity,
        )
        assertEquals(identity, stableRootIdentityHex(root))
        assertNotEquals(identity, stableRootIdentityHex("$root-2"))
        assertTrue(Regex("^[0-9a-f]{64}$").matches(identity))
    }
}
