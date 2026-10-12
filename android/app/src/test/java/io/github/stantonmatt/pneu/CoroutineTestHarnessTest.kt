package io.github.stantonmatt.pneu

import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Test

class CoroutineTestHarnessTest {
    @Test
    @OptIn(ExperimentalCoroutinesApi::class)
    fun coroutineTestsUseVirtualTime() = runTest {
        val result = async {
            delay(60_000)
            "complete"
        }

        assertEquals("complete", result.await())
        assertEquals(60_000L, testScheduler.currentTime)
    }
}
