package io.github.stantonmatt.pneu.data

import kotlinx.coroutines.*
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

class StorageDeadlineTest {
    @Test fun internalTimeoutIsRecoverableAndCallerCanContinue()=runTest {
        try {storageDeadline {awaitCancellation()};fail("Expected storage timeout")}
        catch(_:StorageTimeoutException) {currentCoroutineContext().ensureActive()}
        assertEquals("recovered",storageDeadline {"recovered"})
        assertNull(storageDeadline<String?> {null})
    }
    @Test fun callersShorterDeadlineRemainsCancellation()=runTest {
        try {withTimeout(10) {storageDeadline {awaitCancellation()}};fail("Expected caller timeout")}
        catch(_:TimeoutCancellationException) { /* The storage boundary must not convert this. */ }
    }
    @Test fun explicitCancellationStopsWorkAndRunsCleanup()=runTest {
        val entered=CompletableDeferred<Unit>();var cleaned=false
        val job=launch {
            try {storageDeadline {entered.complete(Unit);awaitCancellation()}}
            finally {cleaned=true}
        }
        entered.await();job.cancelAndJoin();assertTrue(job.isCancelled);assertTrue(cleaned)
    }
}
