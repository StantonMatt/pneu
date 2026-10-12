package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.net.Uri
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.RecentItems
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.util.concurrent.atomic.AtomicBoolean

@RunWith(AndroidJUnit4::class)
class TenthReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i);private val sources=mutableListOf<Uri>()
    private val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
    private var observer:ScreenshotObserver?=null
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(200);app.forgetBinding()
        for(permission in listOf(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("screenshots",true);app.preferences.toggle("photos",false);app.preferences.toggle("recordings",false);app.preferences.toggle("queue",true);app.preferences.toggle("paused",false)
        val raw="pneu://pair?v=1&name=Tenth&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeRead={};observer?.stop();scope.cancel();device.pressBack();device.pressHome()
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
        sources.forEach {c.contentResolver.delete(it,null,null)};app.staging.cleanup()
    }
    private fun image():Uri {
        val uri=c.contentResolver.insert(MediaStore.Images.Media.getContentUri(MediaCatalog.VOLUME),ContentValues().apply {
            put("_display_name","Pneu_tenth_${randomId()}.png");put("mime_type","image/png");put("relative_path","Pictures/Screenshots/");put("is_pending",1)
        })!!;sources.add(uri)
        c.contentResolver.openOutputStream(uri)!!.use {out->Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,out);recycle()}}
        c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null);return uri
    }
    private fun key(uri:Uri)="${MediaCatalog.VOLUME}:${MediaStore.getVersion(c,MediaCatalog.VOLUME)}:images:${ContentUris.parseId(uri)}"
    private fun row(uri:Uri)=Outgoing().apply {state="sent";automatic=true;this.uri=uri.toString();mediaId=key(uri);mime="image/jpeg";kind="screenshot"}
    @Test(timeout=15000) fun realPreferenceDeadlineRecoversWithoutCancellingCaller()=runBlocking<Unit> {
        app.preferences.beforeRead={awaitCancellation()}
        try {app.preferences.read();fail("Expected storage deadline")}
        catch(_:StorageTimeoutException) {currentCoroutineContext().ensureActive()}
        app.preferences.beforeRead={};assertNotNull(app.preferences.read().peer)
        app.preferences.beforeRead={awaitCancellation()}
        try {withTimeout(100) {app.preferences.read()};fail("Expected caller deadline")}
        catch(_:TimeoutCancellationException) {}
        app.preferences.beforeRead={};assertNotNull(app.preferences.read().peer)
    }
    @Test(timeout=25000) fun observerRetriesAfterFiveSecondReadDeadlineAndAdmitsFreshMedia()=runBlocking<Unit> {
        val first=AtomicBoolean(true);val entered=CompletableDeferred<Unit>();val recovered=CompletableDeferred<Unit>()
        app.preferences.beforeRead={if(first.compareAndSet(true,false)) {entered.complete(Unit);awaitCancellation()}}
        observer=ScreenshotObserver(app,latest={video->recovered.complete(Unit);MediaCatalog.latest(c,video)})
        observer!!.start(scope);withTimeout(2000) {entered.await()};withTimeout(12000) {recovered.await()}
        delay(300);val uri=image()
        withTimeout(7000) {while(app.queue.find(key(uri))==null)delay(30)}
        assertEquals(1,app.queue.all().count {it.mediaId==key(uri)})
    }
    @Test(timeout=15000) fun recentGrantsReadToViewerWithoutBroadMediaPermissionAndUsesOriginalMime()=runBlocking<Unit> {
        val uri=image();val item=row(uri)
        val testPackage=i.context.packageName
        assertEquals(PackageManager.PERMISSION_DENIED,c.packageManager.checkPermission(Manifest.permission.READ_MEDIA_IMAGES,testPackage))
        var opened=false
        RecentItems.openOriginal(c,item) {intent->
            opened=true
            assertEquals("image/png",intent.type)
            assertEquals(Intent.FLAG_GRANT_READ_URI_PERMISSION,intent.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION)
            try {c.startActivity(intent.setComponent(ComponentName(testPackage,FixtureViewerActivity::class.java.name)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))}
            catch(e:Exception) {throw AssertionError("Viewer launch failed",e)}
        }
        assertTrue("Original view intent not offered: ${item.uri} / ${item.mediaId}",opened)
        assertTrue("Viewer did not read original",device.wait(Until.hasObject(By.text("Original readable: image/png")),5000))
        device.pressBack()
    }
    @Test(timeout=15000) fun recentRejectsOtherCatalogNamespacesReusedIdsAndMissingSources()=runBlocking<Unit> {
        val uri=image();val item=row(uri);val opened=mutableListOf<Intent>()
        for(id in listOf(item.mediaId.replace(":${MediaStore.getVersion(c,MediaCatalog.VOLUME)}:",":old-catalog:"),item.mediaId.replace(":images:",":video:"),item.mediaId.replace("external_primary:","external:"),"manual:${randomId()}",item.mediaId+"0")) {
            item.mediaId=id;RecentItems.openOriginal(c,item) {fail("Mismatched source opened: $id")}
        }
        item.mediaId=key(uri);RecentItems.openOriginal(c,item) {opened.add(it)};assertEquals(1,opened.size)
        c.contentResolver.delete(uri,null,null);sources.remove(uri);RecentItems.openOriginal(c,item) {opened.add(it)};assertEquals(1,opened.size)
        item.automatic=false;RecentItems.openOriginal(c,item) {fail("Manual source opened")}
    }
}
