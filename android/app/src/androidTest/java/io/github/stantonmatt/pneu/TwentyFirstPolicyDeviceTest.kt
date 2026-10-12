package io.github.stantonmatt.pneu

import android.Manifest
import android.content.ContentValues
import android.content.ContentUris
import android.graphics.Bitmap
import android.provider.MediaStore
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class TwentyFirstPolicyDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val provider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
    @Before fun reset()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        val raw="pneu://pair?v=1&name=Policy%20PC&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"c".repeat(32)))
        c.contentResolver.call(provider,"reset",null,null)
    }
    @After fun cleanup()=runBlocking<Unit> {
        c.contentResolver.call(provider,"release",null,null)
        app.preferences.readSnapshot={MediaCatalog.snapshot(c)};app.preferences.readCatalogVersion={MediaCatalog.version(c)}
        app.connection.stop();app.forgetBinding()
    }
    @Test(timeout=20000) fun delayedResumeSerializesSnapshotsAndFloorsNeverRegress()=runBlocking<Unit> {
        app.preferences.toggle("paused",true);val before=app.preferences.read()
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>();val calls=AtomicInteger()
        app.preferences.readSnapshot={
            if(calls.incrementAndGet()==1) {entered.complete(Unit);release.await()}
            CatalogSnapshot(before.imageVersion,before.imageGeneration+10)
        }
        val old=async(Dispatchers.IO) {app.preferences.toggle("paused",false)}
        try {
            withTimeout(5000) {entered.await()}
            val newer=async(Dispatchers.IO) {app.preferences.toggle("paused",false);app.preferences.toggle("paused",true)}
            delay(200);assertFalse("A newer mutation overtook the snapshot owner",newer.isCompleted)
            release.complete(Unit);old.await();newer.await()
            assertTrue(app.preferences.read().paused);assertEquals(1,calls.get())
            app.preferences.readSnapshot={CatalogSnapshot(before.imageVersion,before.imageGeneration+50)}
            app.preferences.toggle("paused",false);val fresh=app.preferences.read()
            assertEquals(before.imageGeneration+51,fresh.imageFloor)
            app.preferences.toggle("paused",true)
            app.preferences.readSnapshot={CatalogSnapshot(before.imageVersion,before.imageGeneration+5)}
            app.preferences.toggle("paused",false);val final=app.preferences.read()
            assertEquals(fresh.imageFloor,final.imageFloor);assertEquals(fresh.imageGeneration,final.imageGeneration)
            assertTrue(final.policyEpoch>fresh.policyEpoch)
            app.preferences.toggle("paused",true)
            app.preferences.readCatalogVersion={"rebuilt-on-resume"}
            app.preferences.readSnapshot={CatalogSnapshot("rebuilt-on-resume",1)}
            app.preferences.toggle("paused",false);val rebuilt=app.preferences.read()
            assertEquals("rebuilt-on-resume",rebuilt.imageVersion);assertEquals(2L,rebuilt.imageFloor)
            assertEquals(0L,rebuilt.imagePairId);assertEquals(0L,rebuilt.videoPairId)
            assertEquals(0L,rebuilt.screenshotsAfterId);assertEquals(0L,rebuilt.photosAfterId);assertEquals(0L,rebuilt.recordingsAfterId)
        } finally {release.complete(Unit);old.cancelAndJoin()}
    }
    @Test(timeout=20000) fun catalogRebuildDuringCopyRejectsTheOldNamespaceAndRequestsRescan()=runBlocking<Unit> {
        val p=app.preferences.read();val token=AdmissionToken.from(p,false);val binding=DeliveryBinding.from(p)!!
        val row=Outgoing().apply {mediaId="external_primary:${token.version}:images:42";transferId=randomId();uri=provider.buildUpon().appendPath("proxy-stall").build().toString();kind="screenshot";fileName="Old screenshot.png";mime="image/png"}
        val refresh=app.mediaRefresh.value
        val capture=async(Dispatchers.IO) {runCatching {app.staging.capture(row,binding,token)}}
        try {
            withTimeout(5000) {while(c.contentResolver.call(provider,"entered",null,null)!!.getBoolean("entered")!=true)delay(20)}
            // Simulate a live MediaStore rebuild while the old preference namespace
            // and policy epoch remain unchanged. Never destructively rebuild the device catalog.
            app.preferences.readCatalogVersion={"rebuilt-fixture"}
            c.contentResolver.call(provider,"release",null,null)
            assertFalse(capture.await().getOrThrow())
            assertEquals(p.policyEpoch,app.preferences.read().policyEpoch)
            assertNull(app.queue.find(row.mediaId));assertEquals(0,app.queue.seen(row.mediaId))
            assertFalse(File(row.path).exists());assertTrue(app.mediaRefresh.value>refresh)
            val skipped=Outgoing().apply {mediaId=row.mediaId;transferId=randomId();state="skipped";kind="photo"}
            assertFalse(app.enqueue(skipped,binding,token));assertNull(app.queue.find(skipped.mediaId))
        } finally {c.contentResolver.call(provider,"release",null,null);capture.cancelAndJoin()}
    }
    @Test(timeout=25000) fun startupBaselineCompletesBeforeMidCopyPolicyFixtureCreatesMedia()=runBlocking<Unit> {
        val baseline=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        val created=CompletableDeferred<Unit>();val baselines=AtomicInteger()
        val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val observer=ScreenshotObserver(app,latest={video->
            if(baselines.incrementAndGet()==1) {baseline.complete(Unit);release.await()}
            MediaCatalog.latest(c,video)
        })
        val sources=mutableListOf<Uri>()
        fun image():Uri {
            val uri=c.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
                put("_display_name","Pneu_baseline_${randomId()}.png");put("mime_type","image/png");put("relative_path","Pictures/Screenshots/");put("is_pending",1)
            })!!
            sources.add(uri)
            c.contentResolver.openOutputStream(uri)!!.use {output->
                Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,output);recycle()}
            }
            c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)
            return uri
        }
        var capture:Deferred<Uri>?=null
        try {
            observer.start(scope)
            withTimeout(5000) {baseline.await()}
            val backlog=image() // A photo here is correctly excluded by startup.
            capture=async(Dispatchers.IO) {
                DeviceFixture.changePolicyDuringCapture({
                    assertEquals(2,baselines.get());created.complete(Unit);image()
                }) {app.preferences.toggle("originals",true)}
            }
            assertFalse(created.isCompleted)
            release.complete(Unit)
            val uri=capture.await()
            val version=MediaStore.getVersion(c,"external_primary")
            val key="external_primary:$version:images:${ContentUris.parseId(uri)}"
            withTimeout(5000) {while(app.queue.find(key)==null)delay(20)}
            val admitted=app.queue.find(key)!!
            assertEquals("waiting",admitted.state);assertTrue(File(admitted.path).isFile)
            assertEquals(sha256(File(admitted.path).readBytes()),admitted.hash)
            assertNull(app.queue.find("external_primary:$version:images:${ContentUris.parseId(backlog)}"))
        }finally {
            release.complete(Unit);capture?.cancelAndJoin();observer.stop();scope.cancel()
            sources.forEach {c.contentResolver.delete(it,null,null)}
        }
    }
    @Test(timeout=20000) fun settingChangedDuringObserverCopyDropsNormallyAndRescanCapturesTheSameMedia()=runBlocking<Unit> {
        val observer=ScreenshotObserver(app)
        observer.scanOnce();observer.scanOnce()
        val entered=CountDownLatch(1);val release=CountDownLatch(1);val first=AtomicBoolean(true)
        val refresh=app.mediaRefresh.value;val before=app.preferences.read()
        val uri=c.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
            put("_display_name","Pneu_rescan_${randomId()}.png");put("mime_type","image/png");put("relative_path","Pictures/Screenshots/");put("is_pending",1)
        })!!
        var scan:Job?=null
        try {
            c.contentResolver.openOutputStream(uri)!!.use {output->
                Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,output);recycle()}
            }
            c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)
            val key="external_primary:${MediaStore.getVersion(c,"external_primary")}:images:${ContentUris.parseId(uri)}"
            app.staging.beforePayloadWrite={if(first.compareAndSet(true,false)) {entered.countDown();check(release.await(8,TimeUnit.SECONDS))}}
            // No exception wrapper: a dispatcher escape fails this test.
            scan=launch(Dispatchers.IO) {observer.scanOnce()}
            assertTrue(withContext(Dispatchers.IO) {entered.await(5,TimeUnit.SECONDS)})
            app.preferences.toggle("originals",true)
            release.countDown();scan.join()
            assertNull(app.queue.find(key));assertEquals(0,app.queue.seen(key));assertTrue(app.mediaRefresh.value>refresh)
            assertEquals(before.imageGeneration,app.preferences.read().imageGeneration)
            assertTrue(File(c.filesDir,"outgoing").listFiles().orEmpty().none {it.extension in setOf("part","bin")})
            app.staging.beforePayloadWrite={}
            observer.scanOnce()
            val admitted=app.queue.find(key) ?: error("Rescan lost the screenshot")
            assertEquals("waiting",admitted.state);assertTrue(File(admitted.path).isFile)
            assertEquals(sha256(File(admitted.path).readBytes()),admitted.hash)
        }finally {release.countDown();scan?.cancelAndJoin();app.staging.beforePayloadWrite={};c.contentResolver.delete(uri,null,null)}
    }
    @Test(timeout=15000) fun catalogValidationWaitsForTheQueueAndRejectsARebuildDuringThatWait()=runBlocking<Unit> {
        val p=app.preferences.read();val token=AdmissionToken.from(p,false);val binding=DeliveryBinding.from(p)!!
        val live=java.util.concurrent.atomic.AtomicReference(token.version);val reads=AtomicInteger()
        app.preferences.readCatalogVersion={reads.incrementAndGet();live.get()}
        val row=Outgoing().apply {mediaId="external_primary:${token.version}:images:99";transferId=randomId();kind="screenshot";state="skipped"}
        app.queueMutex.lock();var held=true
        val admission=async(Dispatchers.IO) {app.enqueue(row,binding,token)}
        try {
            delay(200);assertFalse(admission.isCompleted);assertEquals(0,reads.get())
            live.set("rebuilt-while-waiting");app.queueMutex.unlock();held=false
            assertFalse(admission.await());assertEquals(1,reads.get());assertNull(app.queue.find(row.mediaId))
        } finally {if(held)app.queueMutex.unlock();admission.cancelAndJoin()}
    }
    @Test(timeout=15000) fun everyAutomaticPublicationNeedsAStampButManualSendsWorkWhilePaused()=runBlocking<Unit> {
        val p=app.preferences.read();val binding=DeliveryBinding.from(p)!!
        val automatic=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="screenshot"}
        assertFalse(app.enqueue(automatic,binding));assertNull(app.queue.find(automatic.mediaId))
        val token=AdmissionToken.from(p,false)
        app.preferences.toggle("photos",true)
        assertFalse(app.enqueue(automatic,binding,token))
        app.preferences.toggle("paused",true)
        val manual=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="text";this.automatic=false;text="Manual while paused"}
        assertTrue(app.enqueue(manual,binding));assertNotNull(app.queue.find(manual.mediaId))
    }
}
