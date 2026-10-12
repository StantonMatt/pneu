package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import android.graphics.Bitmap
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.IOException
import java.util.concurrent.atomic.*

@RunWith(AndroidJUnit4::class)
class RoundTwoAdvisoryDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
    private var previous:Prefs?=null
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        UiDevice.getInstance(i).pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250)
        previous=app.preferences.flow.first()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.forgetBinding();app.preferences.toggle("photos",false);app.preferences.toggle("recordings",false);app.preferences.toggle("screenshots",true);app.preferences.toggle("queue",true);app.preferences.toggle("mobile",true)
        val raw="pneu://pair?v=1&name=Advisory%20PC&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeRead={};app.preferences.beforeMutation={};app.preferences.readBoundary={MediaCatalog.latest(c,it)};app.preferences.readSnapshot={MediaCatalog.snapshot(c)}
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();scope.cancel();scope.coroutineContext[Job]!!.join();app.forgetBinding()
        previous?.let {app.preferences.toggle("clipboardButton",it.clipboardButton);app.preferences.toggle("originals",it.originals)}
        c.getSharedPreferences("service_evidence",Context.MODE_PRIVATE).edit().clear().commit()
    }
    @Test(timeout=15000) fun preferenceCollectionPreservesAuthorityAndRecoversFromReadFailure()=runBlocking<Unit> {
        app.preferences.toggle("clipboardButton",true);app.prefs.first {it.clipboardButton}
        val before=app.prefs.value;val failed=AtomicBoolean(true);val calls=AtomicInteger()
        app.preferences.beforeRead={if(failed.get()) {calls.incrementAndGet();throw IOException("Synthetic DataStore read failure")}}
        try {app.preferences.toggle("clipboardButton",false);fail("Command must report read failure")}catch(_:IOException) {}
        withTimeout(5000) {while(calls.get()==0)delay(20)}
        assertEquals(before,app.prefs.value) // no defaults, unpair or unsafe policy fallback
        try {app.preferences.toggle("originals",!before.originals);fail("Command must report read failure")}catch(_:IOException) {}
        failed.set(false)
        app.preferences.toggle("clipboardButton",false);app.preferences.toggle("originals",!before.originals)
        withTimeout(6000) {app.prefs.first {!it.clipboardButton && it.originals!=before.originals}}
        assertEquals(before.peer,app.prefs.value.peer);assertEquals(before.deviceId,app.prefs.value.deviceId)
        app.preferences.beforeRead={}
    }
    @Test(timeout=15000) fun automaticHealthWritesFailSafelyThenRecoverFromUiAndService()=runBlocking<Unit> {
        val evidence=c.getSharedPreferences("service_evidence",Context.MODE_PRIVATE)
        evidence.edit().putLong("kill",100).putLong("acknowledged",0).putBoolean("running",false).commit()
        app.preferences.toggle("neverSleeping",true);app.prefs.first {it.neverSleeping}
        val calls=AtomicInteger()
        app.preferences.beforeMutation={if(it=="neverSleeping") {calls.incrementAndGet();throw IOException("Synthetic health write failure")}}
        assertTrue(c.health().killed)
        withTimeout(5000) {while(calls.get()==0)delay(20)}
        PneuService.start(c)
        withTimeout(5000) {while(calls.get()<2)delay(20)}
        assertTrue(app.preferences.flow.first().neverSleeping);assertTrue(c.health().killed)
        app.preferences.beforeMutation={};c.health()
        withTimeout(5000) {app.prefs.first {!it.neverSleeping}}
        assertTrue(ServiceEvidence.killed(c))
    }
    @Test(timeout=15000) fun failedMediaBaselinesLeavePauseAndEnablePolicyUnchanged()=runBlocking<Unit> {
        app.preferences.toggle("paused",true);val paused=app.preferences.read()
        app.preferences.readSnapshot={throw IOException("Synthetic generation failure")}
        try {app.preferences.toggle("paused",false);fail("Failed generation saved Resume")}catch(_:IOException) {}
        assertTrue(app.preferences.read().paused);assertEquals(paused.imageGeneration,app.preferences.read().imageGeneration)
        for(key in listOf("photos","recordings")) {
            try {app.preferences.toggle(key,true);fail("Failed baseline enabled $key")}catch(_:IOException) {}
        }
        app.preferences.readSnapshot={throw CancellationException("Cancelled generation")}
        try {app.preferences.toggle("paused",false);fail("Suppressed cancellation")}catch(_:CancellationException) {}
        assertTrue(app.preferences.read().paused)
        app.preferences.readSnapshot={CatalogSnapshot(paused.imageVersion,paused.imageGeneration+10)}
        app.preferences.toggle("paused",false);val resumed=app.preferences.read()
        assertFalse(resumed.paused);assertEquals(paused.imageGeneration+11,resumed.imageFloor)
        assertEquals(resumed.imageFloor,resumed.videoFloor)
        app.preferences.readSnapshot={MediaCatalog.snapshot(c)}
    }
    @Test(timeout=15000) fun reverseConcurrentTextAdmissionNeverPromotesOlderOrEqualText()=runBlocking<Unit> {
        fun row(time:Long)=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="text";text="Synthetic $time";automatic=false;createdAt=time}
        val older=row(1000);val newer=row(2000);val release=CompletableDeferred<Unit>()
        val oldAdmission=async(Dispatchers.IO) {release.await();app.enqueueFixture(older)}
        assertTrue(app.enqueueFixture(newer));release.complete(Unit);assertTrue(oldAdmission.await())
        val equal=row(2000);assertTrue(app.enqueueFixture(equal))
        assertEquals("superseded",app.queue.find(older.mediaId)!!.state);assertEquals(1000,older.createdAt)
        assertEquals("superseded",app.queue.find(equal.mediaId)!!.state)
        assertEquals(listOf(newer.mediaId),app.queue.pending().map {it.mediaId});assertEquals(2000,app.queue.latestTextTime())
        newer.state="sending";assertTrue(app.updateDelivery(newer))
        val latest=row(3000);assertTrue(app.enqueueFixture(latest));assertEquals("sending",app.queue.find(newer.mediaId)!!.state)
        newer.state="failed";assertTrue(app.updateDelivery(newer));assertEquals("superseded",app.queue.find(newer.mediaId)!!.state)
        val timestamps=List(20) {app.newTextTimestamp()};assertTrue(timestamps.zipWithNext().all {(a,b)->b>a})
    }
    @Test(timeout=20000) fun observerRetriesStartupCleanupBeforeScanningAndStopsCleanly()=runBlocking<Unit> {
        val failed=AtomicBoolean(true);val calls=AtomicInteger();val baseline=app.prefs.value.imageGeneration
        val observer=ScreenshotObserver(app,cleanup={calls.incrementAndGet();if(failed.get())throw IOException("Synthetic Room cleanup failure");app.staging.cleanup()})
        val uri=c.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME,"Advisory-${randomId()}.png");put(MediaStore.MediaColumns.MIME_TYPE,"image/png")
            put(MediaStore.MediaColumns.RELATIVE_PATH,"Pictures/Screenshots/");put(MediaStore.MediaColumns.IS_PENDING,1)
        })!!
        try {
            val bitmap=Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888)
            c.contentResolver.openOutputStream(uri)!!.use {assertTrue(bitmap.compress(Bitmap.CompressFormat.PNG,100,it))};bitmap.recycle()
            c.contentResolver.update(uri,ContentValues().apply {put(MediaStore.MediaColumns.IS_PENDING,0)},null,null)
            val id="external_primary:${MediaStore.getVersion(c,"external_primary")}:images:${ContentUris.parseId(uri)}"
            observer.start(scope)
            withTimeout(6000) {while(calls.get()<2)delay(20)}
            assertNull(app.queue.find(id));assertEquals(baseline,app.prefs.value.imageGeneration)
            failed.set(false);app.requestMediaScan()
            withTimeout(6000) {while(app.preferences.read().imageGeneration<=baseline)delay(20)}
            assertNull(app.queue.find(id))
            observer.stop();app.requestMediaScan();delay(100);assertEquals(0,app.queue.all().count {it.mediaId==id})
        } finally {failed.set(false);observer.stop();c.contentResolver.delete(uri,null,null)}
    }
}
