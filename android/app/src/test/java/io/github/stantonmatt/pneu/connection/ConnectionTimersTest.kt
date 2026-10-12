package io.github.stantonmatt.pneu.connection

import org.junit.Assert.*
import org.junit.Test

class ConnectionTimersTest {
    @Test fun completionBetweenSnapshotAndClockReadCannotExpireWrite() {
        var now=100_000L
        var onClock: (() -> Unit)?=null
        val timer=ProgressDeadline(60_000) { onClock?.also {onClock=null}?.invoke();now }
        timer.progress()
        val snapshot=timer.snapshot()!!
        // The old watchdog read nonzero, then completion wrote zero before its
        // second read. Inject that exact interleaving during the deadline read.
        now=103_000;onClock={timer.complete()}
        assertEquals(57_000,timer.remaining(snapshot))
        assertTrue(60_000-(now-0L)<0) // old second read of the zero sentinel expired early
        assertFalse(timer.expire(snapshot));assertNull(timer.snapshot())
        now=160_000;assertFalse(timer.expire(snapshot))
    }
    @Test fun progressBetweenDeadlineDecisionAndExpiryWinsEvenInSameTick() {
        var now=100_000L
        val timer=ProgressDeadline(60_000) {now}
        timer.progress();val old=timer.snapshot()!!
        now+=60_000;assertEquals(0,timer.remaining(old))
        timer.progress();assertFalse(timer.expire(old));assertEquals(60_000,timer.remaining(timer.snapshot()!!))
        val sameTick=timer.snapshot()!!;timer.progress()
        now+=60_000;assertFalse(timer.expire(sameTick));assertTrue(timer.expire(timer.snapshot()!!))
    }
    @Test fun genuineWriteStallRequiresFullTimeoutAndExpiresOnlyOnce() {
        var now=1L;val timer=ProgressDeadline(60_000) {now}
        timer.progress();val snapshot=timer.snapshot()!!
        now=60_000;assertFalse(timer.expire(snapshot))
        now=60_001;assertTrue(timer.expire(snapshot));assertFalse(timer.expire(snapshot))
    }
    @Test fun responseBudgetStartsAfterWriteAndImmediatePongIsNotRearmed() {
        var now=1L;val ping=PingDeadline(30_000) {now}
        val first=ping.start("a");now+=45_000
        ping.sent(first);assertEquals(30_000,ping.remaining(ping.snapshot()!!))
        assertFalse(ping.expire(first));assertTrue(ping.pong("a"))
        val second=ping.start("b");assertTrue(ping.pong("b"))
        ping.sent(second);assertNull(ping.snapshot())
    }
    @Test fun writeCompletionRacingPongNeverLosesThePong() {
        val ping=PingDeadline(30_000) {1L}
        val start=java.util.concurrent.CyclicBarrier(3)
        val done=java.util.concurrent.CyclicBarrier(3)
        val token=java.util.concurrent.atomic.AtomicReference<PingDeadline.Pending>()
        val pool=java.util.concurrent.Executors.newFixedThreadPool(2)
        try {
            val writer=pool.submit {repeat(2000) {start.await();ping.sent(token.get());done.await()}}
            val reader=pool.submit {repeat(2000) {start.await();assertTrue(ping.pong(token.get().nonce));done.await()}}
            repeat(2000) {token.set(ping.start(it.toString()));start.await();done.await();assertNull(ping.snapshot())}
            writer.get();reader.get()
        } finally {pool.shutdownNow()}
    }
    @Test fun pongBetweenSnapshotAndExpiryWins() {
        var now=1L;val ping=PingDeadline(30_000) {now}
        ping.start("a");val pending=ping.snapshot()!!
        now+=30_000;assertEquals(0,ping.remaining(pending))
        assertTrue(ping.pong("a"));assertFalse(ping.expire(pending))
        ping.start("b");assertFalse(ping.pong("a"));now+=29_999
        assertFalse(ping.expire(ping.snapshot()!!));now++
        assertTrue(ping.expire(ping.snapshot()!!))
    }
}
