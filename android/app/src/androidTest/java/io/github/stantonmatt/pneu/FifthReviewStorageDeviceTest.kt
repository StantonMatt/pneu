package io.github.stantonmatt.pneu

import android.content.Intent
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.io.IOException

@RunWith(AndroidJUnit4::class)
class FifthReviewStorageDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    @Test fun committedAdmissionAndOptimizedBytesSurviveMaintenanceAndInterruptedTextIsLatestOnly()=runBlocking<Unit> {
        val c=InstrumentationRegistry.getInstrumentation().targetContext;val app=c.graph
        InstrumentationRegistry.getInstrumentation().uiAutomation.grantRuntimePermission(c.packageName,android.Manifest.permission.READ_MEDIA_IMAGES)
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding()
        val raw="pneu://pair?v=1&name=Fifth%20PC&fp=${"a".repeat(64)}&addr=10.0.2.2%3A47861&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
        val db=app.database.openHelper.writableDatabase
        try {
            repeat(51) {n->app.queue.insert(Outgoing().apply {mediaId="terminal:$n";transferId=randomId();state="sent";automatic=false;orderId=n+1L})}
            app.queue.rememberOutgoing(0,51)
            db.execSQL("CREATE TRIGGER fail_pruning BEFORE DELETE ON outgoing WHEN OLD.state='sent' BEGIN SELECT RAISE(ABORT,'Synthetic maintenance failure'); END")
            app.preferences.beforeMutation={if(it=="localCapacity")throw IOException("Synthetic post-admission flag failure")}
            val row=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="screenshot";automatic=true;fileName="Fixture.bin";mime="application/octet-stream";uri="content://io.github.stantonmatt.pneu.test.review/complete";createdAt=System.currentTimeMillis()}
            app.staging.capture(row)
            assertTrue(File(row.path).exists());assertEquals(sha256(byteArrayOf(1,2,3,4)),row.hash);assertEquals(row.path,app.queue.find(row.mediaId)!!.path);assertEquals(1,app.queue.seen(row.mediaId))
            val text=ManualEntries.text(app,"Admitted through maintenance failure","share");assertNotNull(app.queue.find(text.mediaId))
            app.preferences.beforeMutation={};db.execSQL("DROP TRIGGER fail_pruning")
            val image=ManualEntries.file(app,Uri.parse("content://io.github.stantonmatt.pneu.test.review/image"),"file")
            val optimized=Staging(app,deleteCommittedSource={throw IOException("Synthetic committed-source cleanup failure")}).optimize(image)
            assertEquals(optimized.path,app.queue.find(image.mediaId)!!.path);assertTrue(File(optimized.path).exists());assertEquals(optimized.hash,sha256(File(optimized.path).readBytes()));assertTrue(File(image.path).exists())
            optimized.state="sent"
            assertTrue(Staging(app,deleteCommittedSource={throw IOException("Synthetic committed-source cleanup failure")}).completeFile(optimized));assertEquals("sent",app.queue.find(image.mediaId)!!.state);assertTrue(File(optimized.path).exists())
            val old=text.apply {state="sending";attempted=true};assertTrue(app.updateDelivery(old))
            val newer=ManualEntries.text(app,"Newest after interruption","share").apply {state="failed";nextAttempt=System.currentTimeMillis()+60000};assertTrue(app.updateDelivery(newer))
            row.state="sending";assertTrue(app.updateDelivery(row))
            app.recoverOutgoing()
            assertEquals("superseded",app.queue.find(old.mediaId)!!.state);assertEquals("failed",app.queue.find(newer.mediaId)!!.state);assertEquals(newer.nextAttempt,app.queue.find(newer.mediaId)!!.nextAttempt)
            assertEquals(listOf(newer.mediaId),app.queue.pending().filter {it.kind=="text"}.map {it.mediaId});assertEquals("waiting",app.queue.find(row.mediaId)!!.state)
            // A lone interrupted text is retryable, with ID/timestamp retained.
            newer.state="sending";assertTrue(app.updateDelivery(newer));val timestamp=newer.createdAt;val id=newer.transferId
            app.recoverOutgoing();val retained=app.queue.find(newer.mediaId)!!;assertEquals("waiting",retained.state);assertEquals(id,retained.transferId);assertEquals(timestamp,retained.createdAt)
        } finally {
            app.preferences.beforeMutation={};db.execSQL("DROP TRIGGER IF EXISTS fail_pruning");app.connection.stop();app.forgetBinding();app.staging.cleanup()
        }
    }
}
