package io.github.stantonmatt.pneu

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.QueueSchedule
import io.github.stantonmatt.pneu.connection.RetryDeadlines
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.protocol.randomId
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class TwentiethReviewDeviceTest {
    private val app=InstrumentationRegistry.getInstrumentation().targetContext.graph
    @Before fun reset()=runBlocking<Unit> {DeviceFixture.reset()}
    private fun row(kind:String,order:Long,state:String)=Outgoing().apply {
        mediaId=randomId();transferId=randomId();this.kind=kind;orderId=order;this.state=state
        attempted=true;optimized=true;revision=7;path="private-snapshot";hash="retained-hash";size=42;nextAttempt=1234;mismatches=2
        app.queue.insert(this)
    }
    @Test fun fileRecoveryPreservesLiveTextAndSnapshotIdentity()=runBlocking<Unit> {
        val file=row("file",1,"sending");val text=row("text",2,"sending")
        app.recoverDeliveryLane(false)
        val current=app.queue.find(file.mediaId)!!
        assertEquals("waiting",current.state);assertEquals("sending",app.queue.find(text.mediaId)!!.state)
        for(field in Outgoing::class.java.fields.filter {it.name!="state"})assertEquals(field.name,field.get(file),field.get(current))
        assertEquals(file.mediaId,QueueSchedule.choose(app.queue.pending(),2000,RetryDeadlines({2000},{2000})::deadline).row!!.mediaId)
        app.recoverDeliveryLane(false) // no abandoned file: text still owns its active slot
        assertEquals("sending",app.queue.find(text.mediaId)!!.state)
    }
    @Test fun textRecoverySupersedesInterruptedOlderValueWithoutResettingRetryDeadline()=runBlocking<Unit> {
        val text=row("text",1,"sending");val latest=row("text",2,"failed");val file=row("file",3,"sending")
        app.recoverDeliveryLane(true)
        val old=app.queue.find(text.mediaId)!!
        assertEquals("superseded",old.state);assertEquals(text.revision+1,old.revision)
        assertEquals("failed",app.queue.find(latest.mediaId)!!.state)
        assertEquals(latest.nextAttempt,app.queue.find(latest.mediaId)!!.nextAttempt)
        assertEquals("sending",app.queue.find(file.mediaId)!!.state)
        assertEquals(latest.mediaId,QueueSchedule.choose(app.queue.pending(),2000,RetryDeadlines({2000},{2000})::deadline).row!!.mediaId)
    }
    @Test fun failedRecoveryRollsBackBeforeSelectionAndCanBeRetried()=runBlocking<Unit> {
        val text=row("text",1,"sending");val latest=row("text",2,"waiting")
        val db=app.database.openHelper.writableDatabase
        db.execSQL("CREATE TRIGGER fail_lane_recovery BEFORE UPDATE ON outgoing WHEN NEW.state='superseded' BEGIN SELECT RAISE(ABORT,'recovery fault'); END")
        try {
            try {app.recoverDeliveryLane(true);fail("Recovery must report storage failure")}catch(_:android.database.sqlite.SQLiteException) {}
            assertEquals("sending",app.queue.find(text.mediaId)!!.state)
            assertEquals("waiting",app.queue.find(latest.mediaId)!!.state)
        } finally {db.execSQL("DROP TRIGGER IF EXISTS fail_lane_recovery")}
        app.recoverDeliveryLane(true)
        assertEquals("superseded",app.queue.find(text.mediaId)!!.state)
        assertEquals(latest.mediaId,QueueSchedule.choose(app.queue.pending(),2000,RetryDeadlines({2000},{2000})::deadline).row!!.mediaId)
    }
    @Test fun savedRetriesUseAndroidElapsedTimeAndClampFutureWallDeadlines() {
        var wall=System.currentTimeMillis()
        val elapsed=android.os.SystemClock::elapsedRealtime
        val retries=RetryDeadlines({wall},elapsed)
        val future=Outgoing().apply {mediaId=randomId();transferId=randomId();nextAttempt=wall+86_400_000}
        val before=elapsed();retries.read(listOf(future));val due=retries.deadline(future);val after=elapsed()
        assertTrue(due-before>=72000);assertTrue(due-after<=72000)
        wall-=86_400_000;retries.read(listOf(future));assertEquals(due,retries.deadline(future))
        val ready=Outgoing().apply {mediaId=randomId();transferId=randomId()}
        assertSame(ready,QueueSchedule.choose(listOf(future,ready),elapsed(),retries::deadline).row)
        val restarted=RetryDeadlines({wall},elapsed)
        assertTrue(restarted.deadline(future)-elapsed()<=72000)
    }

}
