package io.github.stantonmatt.pneu

import kotlinx.coroutines.*
import kotlinx.coroutines.test.*
import org.junit.Assert.*
import org.junit.Test
import java.io.IOException

@OptIn(ExperimentalCoroutinesApi::class)
class ServiceInitializationTest {
    @Test fun failedStartupRetriesWithBackoffAndACommandWakesItWithoutDuplicates()=runTest {
        var attempts=0;var failing=true;var failures=0
        val initializer=ServiceInitialization(backgroundScope,{attempts++;if(failing)throw IOException()},{failures++})
        runCurrent();assertEquals(1,attempts)
        advanceTimeBy(999);runCurrent();assertEquals(1,attempts)
        advanceTimeBy(1);runCurrent();assertEquals(2,attempts)
        advanceTimeBy(1999);runCurrent();assertEquals(2,attempts)
        initializer.kick();initializer.kick();runCurrent();assertEquals(3,attempts)
        failing=false;initializer.kick();runCurrent();assertEquals(4,attempts);assertEquals(3,failures)
        repeat(10) {initializer.kick()};advanceTimeBy(60000);runCurrent();assertEquals(4,attempts)
    }
    @Test fun readTimeoutRetriesButOwnerCancellationDoesNot()=runTest {
        var calls=0
        val initializer=ServiceInitialization(backgroundScope,{calls++;io.github.stantonmatt.pneu.data.storageDeadline(100) {awaitCancellation()}},{})
        runCurrent();advanceTimeBy(100);runCurrent();assertEquals(1,calls)
        initializer.kick();runCurrent();assertEquals(2,calls)
        backgroundScope.cancel();advanceTimeBy(60000);runCurrent();assertEquals(2,calls)
    }
}
