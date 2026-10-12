package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.Outgoing
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

class QueueLifecycleTest {
    private fun row(order:Long,retry:Long)=Outgoing().apply {mediaId="row:$order";orderId=order;nextAttempt=retry}
    private fun choose(rows:List<Outgoing>,now:Long,deferred:Map<String,Long> = emptyMap()):QueueSchedule.Choice {
        val retries=RetryDeadlines({now},{now})
        return QueueSchedule.choose(rows,now,retries::deadline,deferred)
    }
    @Test fun readyRowsBypassEarlierBackoffAndKeepReadyOrder() {
        val delayed=row(1,2000);val ready=row(2,0);val later=row(3,0)
        assertSame(ready,choose(listOf(delayed,ready,later),1000).row)
    }
    @Test fun noReadyRowsWaitForEarliestRetryAndPermanentFailuresNeverWake() {
        assertEquals(500L,choose(listOf(row(1,3000),row(2,1500),row(3,Long.MAX_VALUE)),1000).waitMillis)
        assertNull(choose(listOf(row(1,Long.MAX_VALUE)),1000).row)
        assertEquals(Long.MAX_VALUE,choose(emptyList(),1000).waitMillis)
        assertEquals(0L,choose(listOf(row(1,1000)),1000).waitMillis)
    }
    @Test fun onlyRetryableStatesCanBeSelected() {
        val sending=row(1,0).apply {state="sending"}
        val discarded=row(2,0).apply {state="discarded"}
        val sent=row(3,0).apply {state="sent"}
        val failed=row(4,0).apply {state="failed"}
        assertSame(failed,choose(listOf(sending,discarded,sent,failed),1000).row)
        assertEquals(Long.MAX_VALUE,choose(listOf(sending,discarded,sent),1000).waitMillis)
    }
    @Test fun rejectedEmissionBacksOffWithoutBlockingReadySiblings() {
        val rejected=row(1,0).apply {mediaId="rejected"}
        val ready=row(2,0).apply {mediaId="ready"}
        val deferred=mapOf(rejected.mediaId to 2000L)
        assertSame(ready,choose(listOf(rejected,ready),1000,deferred).row)
        assertEquals(1000L,choose(listOf(rejected),1000,deferred).waitMillis)
        assertNull(choose(listOf(rejected),1999,deferred).row)
        assertSame(rejected,choose(listOf(rejected),2000,deferred).row)
        rejected.nextAttempt=3000
        assertEquals(2000L,choose(listOf(rejected),1000,deferred).waitMillis)
        rejected.nextAttempt=Long.MAX_VALUE
        assertEquals(Long.MAX_VALUE,choose(listOf(rejected),1000,deferred).waitMillis)
    }
    @Test fun failedDurableRemovalNeverNotifiesPeer()=runTest {
        var notified=false
        assertFalse(LifecycleOrder.revoke({false}) {notified=true})
        try {LifecycleOrder.revoke({throw java.io.IOException("disk failure")}) {notified=true};fail()} catch(_:java.io.IOException) {}
        assertFalse(notified)
    }
    @Test fun notificationIsAfterDurableCommitAndBestEffort()=runTest {
        var persisted=false
        assertTrue(LifecycleOrder.revoke({persisted=true;true}) {assertTrue(persisted);throw java.io.IOException("offline")})
        assertTrue(persisted)
    }
}
