package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.timing.PersistedDeadlines
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.test.*
import org.junit.Assert.*
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class RetryDeadlinesTest {
    private fun row(id:String="retry",next:Long=0)=Outgoing().apply {mediaId=id;transferId=id;nextAttempt=next}
    @Test fun liveNackAndLocalFailureRetriesIgnoreBackwardAndForwardWallJumps() {
        var wall=10_000_000L;var elapsed=100L;val retries=RetryDeadlines({wall},{elapsed})
        val mismatch=row();retries.retryAfter(mismatch,RetryPolicy.nack("hash_mismatch",0,1.0).waitMillis)
        assertEquals(wall+2000,mismatch.nextAttempt);assertEquals(2100L,retries.deadline(mismatch))
        wall-=7_200_000;elapsed+=1000
        assertEquals(1000L,QueueSchedule.choose(listOf(mismatch),elapsed,retries::deadline).waitMillis)
        wall+=100_000_000;elapsed+=1000
        assertSame(mismatch,QueueSchedule.choose(listOf(mismatch),elapsed,retries::deadline).row)
        val storage=row("storage");retries.retryAfter(storage,SendFailures.retryAfter("local_storage"))
        wall-=100_000_000;elapsed+=59_999
        assertEquals(1L,QueueSchedule.choose(listOf(storage),elapsed,retries::deadline).waitMillis)
        elapsed++;assertSame(storage,QueueSchedule.choose(listOf(storage),elapsed,retries::deadline).row)
    }
    @Test fun savedRetriesClampOnceAfterRollbackOrRebootAndNeverSlideOnRepeatedReads() {
        var wall=1000L;var elapsed=50L;val saved=row(next=86_400_000)
        val retries=RetryDeadlines({wall},{elapsed});retries.read(listOf(saved))
        assertEquals(72_050L,retries.deadline(saved))
        repeat(71) {wall-=10_000;elapsed+=1000;retries.read(listOf(saved));assertEquals(72_050L,retries.deadline(saved))}
        assertEquals(1000L,QueueSchedule.choose(listOf(saved),elapsed,retries::deadline).waitMillis)
        elapsed+=1000;assertSame(saved,QueueSchedule.choose(listOf(saved),elapsed,retries::deadline).row)
        // New process/boot reconstructs the saved wall value, never the old uptime.
        elapsed=1;val rebooted=RetryDeadlines({wall},{elapsed})
        assertEquals(72_001L,rebooted.deadline(saved))
        // Earlier ready entries must not postpone the first-read projection.
        val other=row("later",wall+2000);rebooted.read(listOf(row("ready"),other))
        elapsed+=2000;wall-=86_400_000
        assertSame(other,QueueSchedule.choose(listOf(other),elapsed,rebooted::deadline).row)
    }
    @Test fun changedSavedValueOrSnapshotReplacesProjectionAndPermanentRetryStaysBlocked() {
        var wall=10000L;var elapsed=10L;val retries=RetryDeadlines({wall},{elapsed});val row=row(next=wall+5000)
        assertEquals(5010L,retries.deadline(row))
        elapsed=100;row.nextAttempt=wall+1000;assertEquals(1100L,retries.deadline(row))
        elapsed=200;row.revision++;assertEquals(1200L,retries.deadline(row))
        row.nextAttempt=0;assertSame(row,QueueSchedule.choose(listOf(row),elapsed,retries::deadline).row)
        retries.retryAfter(row,Long.MAX_VALUE)
        assertEquals(Long.MAX_VALUE,row.nextAttempt)
        assertEquals(Long.MAX_VALUE,QueueSchedule.choose(listOf(row),elapsed,retries::deadline).waitMillis)
        row.nextAttempt=wall-1;assertSame(row,QueueSchedule.choose(listOf(row),elapsed,retries::deadline).row)
        assertEquals(0L,PersistedDeadlines.remaining(Long.MIN_VALUE,Long.MAX_VALUE,72000))
        assertEquals(72000L,PersistedDeadlines.remaining(Long.MAX_VALUE-1,Long.MIN_VALUE,72000))
        assertEquals(0L,PersistedDeadlines.remaining(42,42,72000))
        assertEquals(1L,PersistedDeadlines.remaining(Long.MAX_VALUE-1,Long.MAX_VALUE-2,72000))
    }
    @Test fun oneSecondDeferralAndSixtySecondStorageCooldownUseMonotonicScheduling() {
        var wall=10_000L;var elapsed=10L;val retries=RetryDeadlines({wall},{elapsed})
        val failed=row();val ready=row("ready")
        for(duration in listOf(1000L,60000L)) {
            val due=elapsed+duration;val deferred=mapOf(failed.mediaId to due)
            wall-=86_400_000;elapsed+=duration-1
            assertEquals(1L,QueueSchedule.choose(listOf(failed),elapsed,retries::deadline,deferred).waitMillis)
            assertSame(ready,QueueSchedule.choose(listOf(failed,ready),elapsed,retries::deadline,deferred).row)
            elapsed++;assertSame(failed,QueueSchedule.choose(listOf(failed),elapsed,retries::deadline,deferred).row)
        }
    }
    @Test fun newWorkInterruptsStorageAndRetryWaitsWithoutWaitingForTheTimer()=runTest {
        val work=Channel<Unit>(Channel.CONFLATED)
        for(wait in listOf(1000L,60000L,Long.MAX_VALUE)) {
            val before=currentTime;val waiting=async {LaneWait.await(work,wait)}
            runCurrent();assertFalse(waiting.isCompleted);advanceTimeBy(100);work.send(Unit)
            assertTrue(waiting.await());assertEquals(before+100,currentTime)
        }
        val timer=async {LaneWait.await(work,60000)}
        runCurrent();advanceTimeBy(59999);assertFalse(timer.isCompleted)
        advanceTimeBy(1);assertFalse(timer.await())
        val cancelled=async {LaneWait.await(work,Long.MAX_VALUE)}
        runCurrent();cancelled.cancelAndJoin();assertTrue(cancelled.isCancelled)
        assertFalse(LaneWait.await(work,0))
        work.close()
    }
}
