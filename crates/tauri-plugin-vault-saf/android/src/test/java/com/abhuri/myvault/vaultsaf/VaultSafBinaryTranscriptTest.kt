package com.abhuri.myvault.vaultsaf

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class VaultSafBinaryTranscriptTest {
    @Test
    fun sessionIdsAreCanonicalCallerProvidedUuids() {
        assertTrue(isCanonicalBinarySessionId("123e4567-e89b-42d3-a456-426614174000"))
        assertFalse(isCanonicalBinarySessionId("123E4567-E89B-42D3-A456-426614174000"))
        assertFalse(isCanonicalBinarySessionId("123e4567-e89b-12d3-a456-426614174000"))
        assertFalse(isCanonicalBinarySessionId("not-a-session"))
    }

    @Test
    fun readOffsetsAndEofCoverExactChunkAndTransferBoundaries() {
        assertTrue(isValidBinaryReadOffset(0))
        assertTrue(isValidBinaryReadOffset(BINARY_MAX_TRANSFER_BYTES.toLong()))
        assertFalse(isValidBinaryReadOffset(-1))
        assertFalse(isValidBinaryReadOffset(BINARY_MAX_TRANSFER_BYTES.toLong() + 1))

        assertTrue(isBinaryChunkEof(0))
        assertTrue(isBinaryChunkEof(BINARY_BRIDGE_CHUNK_BYTES - 1))
        assertFalse(isBinaryChunkEof(BINARY_BRIDGE_CHUNK_BYTES))
    }

    @Test
    fun writeTranscriptRequiresExactOffsetAndNeverExceedsExpectedLength() {
        assertEquals(
            BINARY_BRIDGE_CHUNK_BYTES.toLong(),
            nextBinaryWriteOffset(0, BINARY_MAX_TRANSFER_BYTES.toLong(), 0, BINARY_BRIDGE_CHUNK_BYTES),
        )
        assertNull(nextBinaryWriteOffset(1, 10, 0, 1))
        assertNull(nextBinaryWriteOffset(0, 10, 0, 0))
        assertNull(nextBinaryWriteOffset(9, 10, 9, 2))
        assertNull(nextBinaryWriteOffset(0, 10, 0, BINARY_BRIDGE_CHUNK_BYTES + 1))
        assertEquals(10L, nextBinaryWriteOffset(9, 10, 9, 1))
    }

    @Test
    fun malformedTranscriptsPublishConservativelyAfterBegin() {
        assertEquals("invalidRequest", malformedBinaryWriteOutcome(false))
        assertEquals("writeOutcomeUnknown", malformedBinaryWriteOutcome(true))
        assertEquals("invalidRequest", malformedBinaryReadOutcome(false))
        assertEquals("nativeBridge", malformedBinaryReadOutcome(true))
    }

    @Test
    fun onlyTheExactSessionAndRootOwnTranscriptCleanup() {
        val ownerSession = "123e4567-e89b-42d3-a456-426614174000"
        val foreignSession = "223e4567-e89b-42d3-a456-426614174000"
        val ownerRoot = "a".repeat(64)
        val foreignRoot = "b".repeat(64)

        assertEquals(
            BinarySessionOwnership.IDLE,
            classifyBinarySessionOwnership(null, null, ownerSession, ownerRoot),
        )
        assertEquals(
            BinarySessionOwnership.OWNER,
            classifyBinarySessionOwnership(ownerSession, ownerRoot, ownerSession, ownerRoot),
        )
        assertEquals(
            BinarySessionOwnership.FOREIGN,
            classifyBinarySessionOwnership(ownerSession, ownerRoot, foreignSession, ownerRoot),
        )
        assertEquals(
            BinarySessionOwnership.FOREIGN,
            classifyBinarySessionOwnership(ownerSession, ownerRoot, ownerSession, foreignRoot),
        )
        assertEquals(
            BinarySessionOwnership.FOREIGN,
            classifyBinarySessionOwnership(ownerSession, ownerRoot, null, null),
        )
        assertEquals(
            BinarySessionOwnership.FOREIGN,
            classifyBinarySessionOwnership(ownerSession, null, ownerSession, ownerRoot),
        )
    }
}
