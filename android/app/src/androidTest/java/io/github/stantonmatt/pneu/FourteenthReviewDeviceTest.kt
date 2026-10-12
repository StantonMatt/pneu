package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class FourteenthReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private fun raw(name:String,pin:Char)="pneu://pair?v=1&name=$name&fp=${pin.toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private suspend fun bind(name:String="Original",pin:Char='a') {
        val uri=raw(name,pin);assertTrue(app.requestPairing(uri,app.preferences.read().peer?.pin));assertTrue(app.installBinding(Invitation.parse(uri),randomId()))
    }
    private fun image():Outgoing {
        val row=Outgoing().apply {mediaId=randomId();transferId=randomId();mime="image/png";fileName="fixture.png";kind="file";automatic=false}
        val source=File(c.filesDir,"outgoing/${row.transferId}.bin").apply {parentFile!!.mkdirs()}
        source.outputStream().use {out->Bitmap.createBitmap(32,32,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,out);recycle()}}
        row.path=source.path;row.size=source.length();row.hash=sha256(source.readBytes());app.queue.insert(row)
        return row
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(200);app.forgetBinding();bind()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.POST_NOTIFICATIONS)
        app.preferences.toggle("setup",true)
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.connection.beforeUnpairRemoval={};app.staging.beforeThumbnailWrite={}
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding();app.staging.cleanup()
    }
    @Test(timeout=20000) fun staleUnpairAndCancellationReportFailureAndPreserveReplacement()=runBlocking<Unit> {
        val old=app.preferences.read()
        app.connection.beforeUnpairRemoval={bind("Replacement",'c')}
        assertFalse(app.connection.unpair());app.connection.beforeUnpairRemoval={}
        val replacement=app.preferences.read();assertEquals("Replacement",replacement.peer!!.name);assertEquals(old.deviceId,replacement.deviceId)
        assertTrue(runCatching {beginPair(c,raw("Stale",'e'),ConfirmedBinding.from(old))}.isFailure)
        assertNull(app.preferences.read().pending);assertEquals(replacement.peer,app.preferences.read().peer)
        val firstRaw=raw("First",'e');val secondRaw=raw("Second",'f')
        assertTrue(app.requestPairing(firstRaw,replacement.peer.pin));assertTrue(app.requestPairing(secondRaw,replacement.peer.pin))
        assertFalse(app.cancelPairing(Invitation.parse(firstRaw)));assertEquals(Invitation.parse(secondRaw),app.preferences.read().pending)
        assertTrue(app.cancelPairing(Invitation.parse(secondRaw)))
        assertTrue(app.connection.unpair());val removed=app.preferences.read();assertNull(removed.peer)
        assertTrue(app.connection.unpair());assertEquals(removed.deviceId,app.preferences.read().deviceId);assertEquals(removed.tlsAlias,app.preferences.read().tlsAlias)
    }
    @Test(timeout=30000) fun failedUnpairDoesNotNavigateOrStopReplacementService()=runBlocking<Unit> {
        fun visible(text:String)=device.findObject(UiSelector().text(text)).waitForExists(5000)
        fun click(text:String) {assertTrue(text,visible(text));device.findObject(UiSelector().text(text)).click()}
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        app.connection.beforeUnpairRemoval={entered.complete(Unit);release.await()}
        try {
            c.startActivity(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
            assertTrue(device.findObject(UiSelector().description("Settings")).waitForExists(5000));device.findObject(UiSelector().description("Settings")).click()
            UiScrollable(UiSelector().scrollable(true)).scrollTextIntoView("Unpair");click("Unpair");click("Unpair")
            withTimeout(5000) {entered.await()};bind("Replacement",'c');val row=image();val replacement=app.preferences.read()
            app.connection.beforeUnpairRemoval={};release.complete(Unit)
            assertTrue(visible("Couldn't unpair"));click("Close");assertTrue(visible("Replacement"))
            assertEquals(replacement.peer,app.preferences.read().peer);assertNotNull(app.queue.find(row.mediaId));assertTrue(File(row.path).exists())
            assertTrue("Foreground service retained",c.getSystemService(android.app.NotificationManager::class.java).activeNotifications.any {it.id==1})
        }finally {release.complete(Unit)}
    }
    @Test(timeout=15000) fun cleanupKeepsLiveThumbnailUntilRoomPublication()=runBlocking<Unit> {
        val row=image();val source=File(row.path);val entered=CompletableDeferred<File>();val release=CountDownLatch(1)
        app.staging.beforeThumbnailWrite={entered.complete(it);check(release.await(8,TimeUnit.SECONDS))}
        try {
            val completion=async(Dispatchers.IO) {app.staging.completeFile(row)}
            val target=withTimeout(5000) {entered.await()};val orphan=File(target.parentFile,"orphan.jpg").apply {writeText("orphan")}
            app.staging.cleanup();assertTrue(target.exists());assertFalse(orphan.exists());assertTrue(source.exists())
            release.countDown();assertTrue(completion.await());assertEquals(target.path,app.queue.find(row.mediaId)!!.path);assertFalse(source.exists())
            app.staging.cleanup();assertTrue(target.exists());assertTrue(target.length() in 1..65536)
            val preview=BitmapFactory.decodeFile(target.path);assertNotNull(preview);preview.recycle()
        }finally {release.countDown()}
    }
    @Test(timeout=15000) fun deliveryResetSkipsLiveOutputAndRejectsLatePublication()=runBlocking<Unit> {
        val row=image();val entered=CompletableDeferred<File>();val release=CountDownLatch(1)
        app.staging.beforeThumbnailWrite={entered.complete(it);check(release.await(8,TimeUnit.SECONDS))}
        try {
            val completion=async(Dispatchers.IO) {app.staging.completeFile(row)};val target=withTimeout(5000) {entered.await()}
            app.clearDelivery();assertTrue(target.exists());assertNull(app.queue.find(row.mediaId))
            release.countDown();assertFalse(completion.await());assertFalse(target.exists());app.staging.cleanup()
        }finally {release.countDown()}
    }
    @Test(timeout=15000) fun cancelledThumbnailWorkerCannotRecreateAnUnlinkedOutput()=runBlocking<Unit> {
        val row=image();val source=File(row.path);val entered=CompletableDeferred<File>();val release=CountDownLatch(1)
        app.staging.beforeThumbnailWrite={entered.complete(it);check(release.await(8,TimeUnit.SECONDS))}
        try {
            val completion=launch(Dispatchers.IO) {app.staging.completeFile(row)};val target=withTimeout(5000) {entered.await()}
            completion.cancelAndJoin();assertFalse(target.exists());assertTrue(source.exists());assertEquals(source.path,app.queue.find(row.mediaId)!!.path)
            app.staging.cleanup();release.countDown();delay(300);assertFalse(target.exists());assertTrue(source.exists())
        }finally {release.countDown()}
    }
    @Test(timeout=15000) fun timedOutThumbnailWorkerCannotPublishOrRecreateOutput()=runBlocking<Unit> {
        val row=image();val source=File(row.path);val entered=CompletableDeferred<File>();val release=CountDownLatch(1)
        app.staging.beforeThumbnailWrite={entered.complete(it);check(release.await(8,TimeUnit.SECONDS))}
        try {
            val completion=async(Dispatchers.IO) {app.staging.completeFile(row)};val target=withTimeout(5000) {entered.await()}
            assertTrue(withTimeout(5000) {completion.await()});assertFalse(target.exists());assertFalse(source.exists());assertEquals("",app.queue.find(row.mediaId)!!.path)
            app.staging.cleanup();release.countDown();delay(300);assertFalse(target.exists());assertEquals("",app.queue.find(row.mediaId)!!.path)
        }finally {release.countDown()}
    }

}
