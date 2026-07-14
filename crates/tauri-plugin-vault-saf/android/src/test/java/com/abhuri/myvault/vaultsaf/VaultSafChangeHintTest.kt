package com.abhuri.myvault.vaultsaf

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class VaultSafChangeHintTest {
    @Test
    fun callbacksCoalesceUntilExactGenerationIsConsumed() {
        val tracker = SafDirtyGeneration()

        assertEquals(SafChangeHint(false, 0L), tracker.snapshot())
        tracker.markDirty()
        val first = tracker.snapshot()
        assertEquals(SafChangeHint(true, 1L), first)

        repeat(1000) { tracker.markDirty() }
        assertEquals(first, tracker.snapshot())

        tracker.consume(first.generation + 1)
        assertEquals(first, tracker.snapshot())
        tracker.consume(first.generation)
        assertEquals(SafChangeHint(false, 1L), tracker.snapshot())

        tracker.markDirty()
        assertEquals(SafChangeHint(true, 2L), tracker.snapshot())
    }

    @Test
    fun rootSwitchResetsEvidenceAndStartsDirty() {
        val tracker = SafDirtyGeneration()
        tracker.markDirty()
        tracker.consume(1)
        tracker.markDirty()

        tracker.resetForNewRoot()
        val switched = tracker.snapshot()
        assertTrue(switched.dirty)
        assertEquals(1L, switched.generation)
        tracker.consume(switched.generation)
        assertFalse(tracker.snapshot().dirty)
    }
}
