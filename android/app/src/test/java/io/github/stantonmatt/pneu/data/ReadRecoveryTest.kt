package io.github.stantonmatt.pneu.data

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.test.*
import org.junit.Assert.*
import org.junit.Test
import java.io.IOException

@OptIn(ExperimentalCoroutinesApi::class)
class ReadRecoveryTest {
    @Test fun transientReadsPreserveLastValueAndResumeWithAuthoritativeState()=runTest {
        var attempts=0
        val values=flow {
            when(++attempts) {
                1->{emit("trusted");throw IOException()}
                2->throw IOException()
                else->{emit("updated");awaitCancellation()}
            }
        }.recoverReads().stateIn(backgroundScope,SharingStarted.Eagerly,"initializing")
        runCurrent();assertEquals("trusted",values.value)
        advanceTimeBy(2000);runCurrent();assertEquals("trusted",values.value);assertEquals(2,attempts)
        advanceTimeBy(2000);runCurrent();assertEquals("updated",values.value);assertEquals(3,attempts)
    }
    @Test fun cancellationIsNotRetriedOrConvertedToDefaults()=runTest {
        var attempts=0
        val reader=async {flow<Int> {attempts++;throw CancellationException()}.recoverReads().first()}
        runCurrent();assertTrue(reader.isCancelled);assertEquals(1,attempts)
    }
}
