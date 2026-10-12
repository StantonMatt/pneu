package io.github.stantonmatt.pneu

import android.content.Intent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.features.ClipboardDelivery
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class FourthReviewStorageDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    @Test fun failedReceiptCommitPreservesRetrySourceAndInboundReadFailureNacks()=runBlocking<Unit> {
        val c=InstrumentationRegistry.getInstrumentation().targetContext;val app=c.graph
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding()
        val raw="pneu://pair?v=1&name=Storage%20PC&fp=${"a".repeat(64)}&addr=10.0.2.2%3A47861&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
        app.preferences.toggle("clipboardFromPc",true)
        val db=app.database.openHelper.writableDatabase
        try {
            val row=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="file";automatic=false;fileName="Storage fixture.bin";mime="application/octet-stream";uri="content://io.github.stantonmatt.pneu.test.review/complete";createdAt=System.currentTimeMillis()}
            val source=File(c.filesDir,"outgoing/${row.transferId}.bin").apply {parentFile!!.mkdirs();writeBytes(byteArrayOf(1,2,3,4))}
            row.path=source.path;row.size=4;row.hash=sha256(source.readBytes());assertTrue(app.enqueueFixture(row));val oldUri=row.uri;row.state="sent"
            db.execSQL("CREATE TRIGGER fail_receipt BEFORE UPDATE ON outgoing WHEN NEW.state='sent' BEGIN SELECT RAISE(ABORT,'Synthetic receipt commit failure'); END")
            try {app.staging.completeFile(row);fail("Committed faulted receipt")}catch(_:android.database.sqlite.SQLiteException) {}
            assertEquals(source.path,row.path);assertEquals(oldUri,row.uri);assertTrue(source.exists());assertEquals("waiting",app.queue.find(row.mediaId)!!.state)
            db.execSQL("DROP TRIGGER fail_receipt")
            assertTrue("Receipt committed",app.staging.completeFile(row));assertEquals("sent",app.queue.find(row.mediaId)!!.state);assertFalse("Original deleted after commit",source.exists());assertEquals("",row.path)
            db.execSQL("ALTER TABLE receipts RENAME TO unavailable_receipts")
            var applications=0
            val delivery=ClipboardDelivery(app) {_,_->applications++}
            val result=delivery.receive("d".repeat(32),json("transfer_id" to randomId(),"text" to "Local read fixture","created_at" to 1,"origin" to "clipboard"))
            assertEquals(Type.NACK,result.type);assertEquals("storage_failed",result.body["code"].asString);assertTrue(result.body["retryable"].asBoolean);assertEquals(0,applications)
            db.execSQL("ALTER TABLE unavailable_receipts RENAME TO receipts")
            val cancelled=ClipboardDelivery(app) {_,_->throw CancellationException("Synthetic cancellation")}
            try {cancelled.receive("d".repeat(32),json("transfer_id" to randomId(),"text" to "Cancelled local operation","created_at" to 2,"origin" to "clipboard"));fail("Swallowed cancellation")}
            catch(_:CancellationException) {}
            assertEquals(0,app.queue.receiptCount())
        } finally {
            runCatching {db.execSQL("DROP TRIGGER IF EXISTS fail_receipt")};runCatching {db.execSQL("ALTER TABLE unavailable_receipts RENAME TO receipts")}
            app.connection.stop();app.forgetBinding()
        }
    }
}
