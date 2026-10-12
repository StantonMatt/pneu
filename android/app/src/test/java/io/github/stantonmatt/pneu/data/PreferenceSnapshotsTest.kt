package io.github.stantonmatt.pneu.data

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.test.*
import org.junit.*
import org.junit.Assert.*

@OptIn(ExperimentalCoroutinesApi::class)
class PreferenceSnapshotsTest {
    private val paired=Prefs(deviceId="phone",peer=Peer("A","pin-a","pc-a",emptyList()))
    @Test fun coldCaptureKeepsFirstLoadEvenWhenReplacementImmediatelyFollows()=runTest {
        val owner=CoroutineScope(SupervisorJob()+StandardTestDispatcher(testScheduler))
        try {
            val snapshots=PreferenceSnapshots(flow {emit(paired);emit(paired.copy(peer=Peer("B","pin-b","pc-b",emptyList())))},owner)
            assertFalse(snapshots.state.value.loaded)
            val binding=snapshots.captureBinding();assertFalse(binding.isCompleted)
            runCurrent()
            assertEquals("B",snapshots.state.value.peer!!.name)
            assertEquals(DeliveryBinding.from(paired),binding.await())
        } finally {owner.cancel()}
    }
    @Test fun warmCaptureIsSynchronousAndUnpairedIsAnAuthoritativeNull()=runTest {
        val owner=CoroutineScope(SupervisorJob()+StandardTestDispatcher(testScheduler))
        try {
            val source=MutableStateFlow(paired);val snapshots=PreferenceSnapshots(source,owner);runCurrent()
            val binding=snapshots.captureBinding();assertTrue(binding.isCompleted)
            source.value=Prefs();runCurrent()
            assertEquals(DeliveryBinding.from(paired),binding.await())
            assertTrue(snapshots.state.value.loaded);assertNull(snapshots.captureBinding().await())
        } finally {owner.cancel()}
    }
    @Test fun initialLoadDeadlineIsVisibleRecoverableAndDoesNotPreventLaterLoad()=runTest {
        val owner=CoroutineScope(SupervisorJob()+StandardTestDispatcher(testScheduler));val release=CompletableDeferred<Unit>()
        try {
            val snapshots=PreferenceSnapshots(flow {release.await();emit(paired)},owner)
            val binding=snapshots.captureBinding()
            try {binding.await();fail("Expected initial-load storage deadline")}catch(_:StorageTimeoutException) {currentCoroutineContext().ensureActive()}
            assertEquals(5000L,currentTime);assertFalse(snapshots.state.value.loaded)
            release.complete(Unit);runCurrent()
            assertEquals(paired,snapshots.awaitLoaded());assertEquals(DeliveryBinding.from(paired),snapshots.captureBinding().await())
        } finally {owner.cancel()}
    }
    @Test fun callerCancellationRemainsCancellationWhileLoading()=runTest {
        val owner=CoroutineScope(SupervisorJob()+StandardTestDispatcher(testScheduler))
        try {
            val snapshots=PreferenceSnapshots(flow {awaitCancellation()},owner)
            try {withTimeout(10) {snapshots.awaitLoaded()};fail("Expected caller timeout")}catch(_:TimeoutCancellationException) {}
            currentCoroutineContext().ensureActive();assertFalse(snapshots.state.value.loaded)
        } finally {owner.cancel()}
    }
}
