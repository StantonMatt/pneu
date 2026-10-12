package io.github.stantonmatt.pneu

import android.Manifest
import android.app.NotificationManager
import android.content.*
import android.content.pm.ShortcutInfo
import android.content.pm.ShortcutManager
import android.graphics.Bitmap
import android.net.Uri
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.*
import java.util.concurrent.*
import java.util.concurrent.atomic.AtomicBoolean

@RunWith(AndroidJUnit4::class)
class EighthReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private val sources=mutableListOf<Uri>()
    private val provider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
    private fun control(command:String)=c.contentResolver.call(provider,command,null,null)!!
    private fun row(uri:Uri)=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();this.uri=uri.toString();automatic=false;kind="file";fileName="Fixture.png";mime="image/png";createdAt=System.currentTimeMillis()}
    private fun image(pending:Boolean=false):Uri {
        val uri=c.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
            put("_display_name","Pneu_eighth_${randomId()}.png");put("mime_type","image/png");put("relative_path","Pictures/Screenshots/");put("is_pending",1)
        })!!;sources.add(uri)
        c.contentResolver.openOutputStream(uri)!!.use {output->Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,output);recycle()}}
        if(!pending)publish(uri)
        return uri
    }
    private fun publish(uri:Uri) {c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)}
    private fun key(uri:Uri)="external_primary:${MediaStore.getVersion(c,"external_primary")}:images:${ContentUris.parseId(uri)}"
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding();app.staging.cleanup()
        for(permission in listOf(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("screenshots",true);app.preferences.toggle("photos",false);app.preferences.toggle("recordings",false);app.preferences.toggle("queue",true);app.preferences.toggle("shareSheet",true);app.preferences.toggle("mobile",true)
        val raw="pneu://pair?v=1&name=Eighth&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeRead={};app.staging.beforePayloadWrite={};control("release")
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding();sources.forEach {c.contentResolver.delete(it,null,null)};app.staging.cleanup()
        File(c.filesDir,"outgoing/quota-fixture").delete()
        ManualEntries.clearLegacyShortcuts(c)
    }
    @Test(timeout=15000) fun resumeFloorExcludesPendingRowsAndCheckpointsCannotRegressIt()=runBlocking<Unit> {
        val observer=ScreenshotObserver(app);observer.scanOnce();observer.scanOnce()
        app.preferences.toggle("paused",true);val before=image(true)
        observer.scanOnce();assertNull(app.queue.find(key(before)))
        app.preferences.toggle("paused",false);val resumed=app.preferences.read()
        assertTrue(resumed.imageFloor>c.contentResolver.query(before,arrayOf("generation_added"),null,null,null)!!.use {it.moveToFirst();it.getLong(0)})
        app.preferences.checkpoint(false,resumed.imageVersion,0,floor=0)
        assertEquals(resumed.imageGeneration,app.preferences.read().imageGeneration);assertEquals(resumed.imageFloor,app.preferences.read().imageFloor)
        publish(before);observer.scanOnce();assertNull(app.queue.find(key(before)))
        val after=image();observer.scanOnce();assertNotNull(app.queue.find(key(after)))
    }
    @Test(timeout=15000) fun permissionRestorationBaselinesTheGapAndLateCaptureCannotPublishAcrossResume()=runBlocking<Unit> {
        var readable=false;val observer=ScreenshotObserver(app,canRead={readable && !it})
        val unseen=image();observer.scanOnce();assertNull(app.queue.find(key(unseen)))
        readable=true;observer.scanOnce();observer.scanOnce();assertNull(app.queue.find(key(unseen)))
        control("reset");val binding=DeliveryBinding.from(app.preferences.read())!!;val token=AdmissionToken.from(app.preferences.read(),false)
        val blocked=row(provider.buildUpon().appendPath("proxy-stall").build()).apply {automatic=true;kind="screenshot"}
        val capture=async(Dispatchers.IO) {runCatching {app.staging.capture(blocked,binding,token)}}
        assertTrue(ProviderReads.call {control("entered").getBoolean("entered")})
        app.preferences.toggle("paused",true);app.preferences.toggle("paused",false);control("release")
        assertFalse(capture.await().getOrThrow());assertNull(app.queue.find(blocked.mediaId));assertFalse(File(blocked.path).exists())
    }
    @Test(timeout=15000) fun jpegAndCaptureReserveTheSameQuotaWhileOriginalRemainsDurable()=runBlocking<Unit> {
        val original=row(provider.buildUpon().appendPath("image").build());app.staging.capture(original)
        val root=File(c.filesDir,"outgoing");val filler=File(root,"quota-fixture")
        val entered=CountDownLatch(1);val release=CountDownLatch(1);val first=AtomicBoolean(true)
        app.staging.beforePayloadWrite={length->if(first.compareAndSet(true,false)) {
            // Leave only two bytes beyond the compressor's active reservation.
            RandomAccessFile(filler,"rw").use {it.setLength(512L*1024*1024-root.listFiles()!!.sumOf {f->f.length()}-length-2)}
            entered.countDown();check(release.await(5,TimeUnit.SECONDS))
        }}
        try {
            val encoding=async(Dispatchers.IO) {app.staging.optimize(original)}
            assertTrue(withContext(Dispatchers.IO) {entered.await(4,TimeUnit.SECONDS)})
            assertTrue(File(original.path).exists());assertEquals(original.transferId,app.queue.find(original.mediaId)!!.transferId)
            val competing=row(provider.buildUpon().appendPath("complete").build())
            try {app.staging.capture(competing);fail("Concurrent write bypassed JPEG reservation")}catch(_:LocalCapacityException) {}
            assertNull(app.queue.find(competing.mediaId));assertTrue(root.listFiles()!!.sumOf {it.length()}<=512L*1024*1024)
            release.countDown();val optimized=encoding.await()
            assertTrue(optimized.optimized);assertFalse(File(original.path).exists());assertTrue(File(optimized.path).exists())
            assertEquals(optimized.hash,sha256(File(optimized.path).readBytes()));assertTrue(root.listFiles()!!.sumOf {it.length()}<=512L*1024*1024)
        } finally {release.countDown();app.staging.beforePayloadWrite={};filler.delete()}
    }
    @Test(timeout=15000) fun bootDoesNotReadPreferencesAndUnpairedServiceStops()=runBlocking<Unit> {
        val starts=mutableListOf<Intent>();val wrapper=object:ContextWrapper(c) {
            override fun startForegroundService(intent:Intent):ComponentName {starts.add(intent);return ComponentName(c,PneuService::class.java)}
        }
        app.preferences.beforeRead={throw IOException("Permanent storage failure")}
        val receiver=BootReceiver();receiver.onReceive(wrapper,Intent("unrelated"));assertTrue(starts.isEmpty())
        receiver.onReceive(wrapper,Intent(Intent.ACTION_BOOT_COMPLETED));receiver.onReceive(wrapper,Intent(Intent.ACTION_MY_PACKAGE_REPLACED));assertEquals(2,starts.size)
        assertTrue(starts.all {it.component==ComponentName(c,PneuService::class.java)})
        app.preferences.beforeRead={};app.forgetBinding();PneuService.start(c)
        delay(300)
        withTimeout(4000) {while(c.getSystemService(NotificationManager::class.java).activeNotifications.any {it.id==1})delay(50)}
    }
    @Test(timeout=15000) fun cachedDirectShareIsRejectedAndLegacyShortcutsAreRemoved()=runBlocking<Unit> {
        val manager=c.getSystemService(ShortcutManager::class.java)
        manager.addDynamicShortcuts(listOf(ShortcutInfo.Builder(c,"pc").setShortLabel("Old PC").setLongLived(true).setIntent(Intent(c,MainActivity::class.java).setAction(Intent.ACTION_VIEW)).build()))
        ManualEntries.clearLegacyShortcuts(c);assertTrue(manager.dynamicShortcuts.isEmpty());assertTrue(manager.getShortcuts(ShortcutManager.FLAG_MATCH_CACHED).none {it.id=="pc"})
        val before=app.queue.nextOrder()
        c.startActivity(Intent(c,ShareActivity::class.java).setAction(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT,"Stale target").putExtra(Intent.EXTRA_SHORTCUT_ID,"pc").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        delay(500);assertEquals(before,app.queue.nextOrder())
        c.startActivity(Intent(c,ShareActivity::class.java).setAction(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT,"Ordinary share").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        withTimeout(4000) {while(app.queue.nextOrder()==before)delay(30)}
        assertEquals("Ordinary share",app.queue.pending().single().text)
    }
    @Test(timeout=15000) fun shareReadFailureIsVisibleAndNeverInventsTrust()=runBlocking<Unit> {
        val before=app.queue.nextOrder();app.preferences.beforeRead={throw IOException("Storage failure")}
        val event=i.uiAutomation.executeAndWaitForEvent({
            c.startActivity(Intent(c,ShareActivity::class.java).setAction(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT,"Must not be sent").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        },{event->event.text.any {it.toString().contains("Couldn't load Pneu settings")}},4000)
        assertTrue(event.text.any {it.toString().contains("Couldn't load Pneu settings")});event.recycle();assertEquals(before,app.queue.nextOrder())
    }
    @Test(timeout=15000) fun completedHistoryRetainsThumbnailAndOnlyOpensExistingOriginal()=runBlocking<Unit> {
        val uri=image();val original=row(uri).apply {mediaId=key(uri);automatic=true;kind="screenshot"};app.staging.capture(original)
        val payload=File(original.path);original.state="sent";assertTrue(app.staging.completeFile(original));assertFalse(payload.exists())
        val thumbnail=File(original.path);assertEquals(File(c.cacheDir,"thumbnails"),thumbnail.parentFile);assertTrue(thumbnail.length() in 1..65536)
        val opened=mutableListOf<Intent>();RecentItems.openOriginal(c,original) {opened.add(it)}
        assertEquals(1,opened.size);assertEquals(Intent.ACTION_VIEW,opened.single().action);assertEquals(uri,opened.single().data)
        c.contentResolver.delete(uri,null,null);sources.remove(uri);RecentItems.openOriginal(c,original) {opened.add(it)};assertEquals(1,opened.size)
        val shared=row(provider.buildUpon().appendPath("image").build());app.staging.capture(shared);val sharedPayload=File(shared.path);shared.state="sent";assertTrue(app.staging.completeFile(shared));assertFalse(sharedPayload.exists());assertEquals("",shared.uri)
        RecentItems.openOriginal(c,shared) {fail("Cannot reopen sender URI")}
    }
}
