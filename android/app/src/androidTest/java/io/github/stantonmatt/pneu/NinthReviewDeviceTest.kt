package io.github.stantonmatt.pneu

import android.Manifest
import android.app.*
import android.content.*
import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.net.Uri
import android.os.Bundle
import android.os.Process
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
import kotlinx.coroutines.flow.filterNotNull
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.IOException
import java.util.concurrent.atomic.*

@RunWith(AndroidJUnit4::class)
class NinthReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i);private val sources=mutableListOf<Uri>()
    private val current=AtomicReference<ShareActivity?>()
    private val clipboardCreated=CompletableDeferred<SendClipboardActivity>()
    private var injectEarlyFocus=false
    private val callbacks=object:Application.ActivityLifecycleCallbacks {
        override fun onActivityCreated(a:Activity,s:Bundle?) {
            if(a is ShareActivity)current.set(a)
            if(a is SendClipboardActivity && injectEarlyFocus) {
                assertNull(a.operation)
                a.onWindowFocusChanged(true) // Reentrant callback inside super.onCreate.
                clipboardCreated.complete(a)
            }
        }
        override fun onActivityStarted(a:Activity) {} override fun onActivityResumed(a:Activity) {}
        override fun onActivityPaused(a:Activity) {} override fun onActivityStopped(a:Activity) {}
        override fun onActivitySaveInstanceState(a:Activity,s:Bundle) {} override fun onActivityDestroyed(a:Activity) {}
    }
    private fun control(method:String)=c.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.review"),"share-control",method,null)!!
    private suspend fun startedOperation(activity:ManualEntryActivity):ManualEntryViewModel=withTimeout(4000) {
        activity.operationReady.await().also {it.startedSignal.await()}
    }
    private suspend fun currentShare():ShareActivity=withTimeout(4000) {
        while(current.get()==null)delay(10)
        requireNotNull(current.get())
    }
    private suspend fun entered()=withTimeout(3500) {while(!control("stats").getBoolean("entered"))delay(10)}
    private suspend fun queued(n:Int)=withTimeout(5000) {while(app.queue.pending().count {it.kind=="file"}<n)delay(20)}
    private fun caller(vararg paths:String,keepAlive:Boolean=false,newTask:Boolean=false) {
        val uris=ArrayList(paths.map {"content://io.github.stantonmatt.pneu.test.sources/$it"})
        c.startActivity(Intent().setComponent(ComponentName(c.packageName+".test",FixtureCallerActivity::class.java.name)).putStringArrayListExtra("uris",uris).putExtra("keepAlive",keepAlive).putExtra("newTask",newTask).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
    }
    private fun image():Uri {
        val uri=c.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
            put("_display_name","Pneu_ninth_${randomId()}.png");put("mime_type","image/png");put("relative_path","Pictures/Screenshots/");put("is_pending",1)
        })!!;sources.add(uri)
        c.contentResolver.openOutputStream(uri)!!.use {Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,it);recycle()}}
        c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null);return uri
    }
    private fun key(uri:Uri)="external_primary:${MediaStore.getVersion(c,"external_primary")}:images:${ContentUris.parseId(uri)}"
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();device.setOrientationNatural();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(200);app.forgetBinding()
        for(permission in listOf(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("screenshots",true);app.preferences.toggle("photos",false);app.preferences.toggle("recordings",false);app.preferences.toggle("queue",true);app.preferences.toggle("shareSheet",true);app.preferences.toggle("wifi",true);app.preferences.toggle("tailscale",true)
        val raw="pneu://pair?v=1&name=Ninth&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
        (c.applicationContext as Application).registerActivityLifecycleCallbacks(callbacks);control("reset")
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeRead={};app.preferences.beforeReadStart={};control("release");device.setOrientationNatural();device.unfreezeRotation();device.pressHome()
        (c.applicationContext as Application).unregisterActivityLifecycleCallbacks(callbacks)
        i.runOnMainSync {current.get()?.finish()}
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding();sources.forEach {c.contentResolver.delete(it,null,null)};app.staging.cleanup()
    }
    @Test(timeout=15000) fun earlyClipboardFocusWaitsForOperationAndCapturedBinding()=runBlocking<Unit> {
        injectEarlyFocus=true
        c.startActivity(Intent(c,SendClipboardActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        val activity=withTimeout(4000) {clipboardCreated.await()}
        val operation=startedOperation(activity)
        val result=withTimeout(4000) {operation.result.filterNotNull().first()}
        assertEquals("PC unreachable. Not sent.",result.message)
        assertNotNull(operation.clipboardBinding);assertTrue(app.queue.pending().isEmpty())
    }
    @Test(timeout=15000) fun serviceRecoversInitialReadFailureWithoutAnotherStart()=runBlocking<Unit> {
        val failed=AtomicBoolean(true);val reads=AtomicInteger();val epoch=app.preferences.read().policyEpoch
        app.preferences.beforeRead={if(failed.get()) {reads.incrementAndGet();throw IOException("Synthetic initial read")}}
        PneuService.start(c);withTimeout(4000) {while(reads.get()<2)delay(20)}
        assertEquals("Storage unavailable",app.connection.state.value.status)
        failed.set(false)
        withTimeout(7000) {while(app.preferences.read().policyEpoch<epoch+2)delay(20)}
        val uri=image();withTimeout(4000) {while(app.queue.find(key(uri))==null)delay(20)}
        assertNotEquals("Storage unavailable",app.connection.state.value.status)
    }
    @Test(timeout=15000) fun serviceStartCommandRekicksBackoffAndStartsExactlyOneObserver()=runBlocking<Unit> {
        val failed=AtomicBoolean(true);val reads=AtomicInteger();val epoch=app.preferences.read().policyEpoch
        app.preferences.beforeRead={if(failed.get()) {reads.incrementAndGet();throw IOException("Synthetic initial read")}}
        PneuService.start(c);withTimeout(11000) {while(reads.get()<4)delay(20)}
        failed.set(false);PneuService.start(c)
        withTimeout(3000) {while(app.preferences.read().policyEpoch<epoch+2)delay(10)}
        val recovered=app.preferences.read().policyEpoch
        repeat(4) {PneuService.start(c)};delay(300)
        assertEquals(recovered,app.preferences.read().policyEpoch)
        val uri=image();withTimeout(4000) {while(app.queue.find(key(uri))==null)delay(20)}
        assertEquals(1,app.queue.all().count {it.mediaId==key(uri)})
    }
    private suspend fun replace():Prefs {
        val old=app.preferences.read()
        val raw="pneu://pair?v=1&name=Replacement&fp=${randomId(32)}&addr=127.0.0.1%3A1&token=${randomId(32)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,old.peer!!.pin));assertTrue(app.installBinding(Invitation.parse(raw),randomId()))
        return app.preferences.read()
    }
    private suspend fun replacementBeforeShareRead(vararg paths:String,text:String?=null) {
        val original=DeliveryBinding.from(app.prefs.value)!!
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>();val first=AtomicBoolean(true)
        app.preferences.beforeReadStart={if(first.compareAndSet(true,false)) {entered.complete(Unit);release.await()}}
        try {
            if(text==null)caller(*paths,newTask=true)
            else c.startActivity(Intent().setComponent(ComponentName(c.packageName+".test",FixtureCallerActivity::class.java.name)).putExtra("text",text).putExtra("newTask",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
            withTimeout(3000) {entered.await()}
            val operation=startedOperation(currentShare());assertTrue(operation.started)
            val replacement=replace();assertFalse(original.matches(replacement))
            release.complete(Unit)
            withTimeout(3000) {operation.result.first {it!=null}}
            assertTrue(operation.completed.isEmpty());assertTrue(app.queue.pending().isEmpty())
            assertEquals(0,control("stats").getInt("opens"))
            assertEquals(replacement.peer,app.preferences.read().peer)
        } finally {release.complete(Unit);app.preferences.beforeReadStart={}}
    }
    @Test(timeout=15000) fun singleFileShareCannotAcquireReplacementDuringFirstSettingsRead()=runBlocking<Unit> {replacementBeforeShareRead("image")}
    @Test(timeout=15000) fun multiFileShareCannotAcquireReplacementDuringFirstSettingsRead()=runBlocking<Unit> {replacementBeforeShareRead("image","third-image")}
    @Test(timeout=15000) fun textShareCannotAcquireReplacementDuringFirstSettingsRead()=runBlocking<Unit> {replacementBeforeShareRead(text="Original recipient text")}
    @Test(timeout=15000) fun replacementDuringMultiShareCannotRetargetLaterItems()=runBlocking<Unit> {
        caller("image","slow-image","third-image");entered();queued(1)
        val operation=startedOperation(currentShare());val replacement=replace()
        control("release");withTimeout(5000) {operation.result.first {it!=null}}
        assertEquals(1,operation.completed.filterNotNull().size);assertEquals(3,operation.completed.size)
        assertTrue(app.queue.pending().isEmpty());assertEquals(replacement.peer,app.preferences.read().peer)
    }
    @Test(timeout=90000) fun immediateHomeKeeps200MiBShare()=runBlocking<Unit> {
        val uri=c.contentResolver.insert(MediaStore.Downloads.getContentUri("external_primary"),ContentValues().apply {
            put("_display_name","Pneu_immediate_${randomId()}.bin");put("mime_type","application/octet-stream");put("is_pending",1)
        })!!;sources.add(uri)
        withContext(Dispatchers.IO) {c.contentResolver.openOutputStream(uri)!!.use {out->val bytes=ByteArray(1048576);repeat(200) {out.write(bytes)}}}
        c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)
        val command="am start -a android.intent.action.SEND -t application/octet-stream -d $uri --eu android.intent.extra.STREAM $uri --grant-read-uri-permission -n ${c.packageName}/.features.ShareActivity"
        val launchAt=android.os.SystemClock.elapsedRealtime()
        i.uiAutomation.executeShellCommand(command).use {android.os.ParcelFileDescriptor.AutoCloseInputStream(it).readBytes()}
        delay((100-(android.os.SystemClock.elapsedRealtime()-launchAt)).coerceAtLeast(0))
        val homeAt=android.os.SystemClock.elapsedRealtime()-launchAt
        assertTrue("Home sent at $homeAt ms",homeAt<=200)
        i.sendStatus(0,Bundle().apply {putString("stream","Home requested $homeAt ms after launching 200 MiB SEND\n")})
        i.uiAutomation.executeShellCommand("input keyevent KEYCODE_HOME").use {android.os.ParcelFileDescriptor.AutoCloseInputStream(it).readBytes()}
        withTimeout(30000) {while(app.queue.pending().none {it.kind=="file"})delay(50)}
        val row=app.queue.pending().single {it.kind=="file"};assertEquals(209715200L,row.size)
        assertEquals("72abf2ca8f36943ebe2e49ca3a51d409ca5f0bfcffab6c9d25643c17c32889da",row.hash)
        assertEquals("",row.uri)
        assertEquals(1,app.queue.pending().count {it.kind=="file"})
    }
    @Test(timeout=20000) fun removalBeforePreferencesCompleteKeepsAllShareGrants()=runBlocking<Unit> {
        val release=CompletableDeferred<Unit>()
        app.preferences.beforeRead={release.await()}
        try {
            caller("image","third-image",newTask=true)
            val activity=currentShare();val operation=startedOperation(activity)
            assertTrue(activity.isTaskRoot)
            i.runOnMainSync {activity.finishAndRemoveTask()}
            withTimeout(3000) {while(!activity.isDestroyed)delay(10)}
            val uri=Uri.parse("content://io.github.stantonmatt.pneu.test.sources/image")
            assertEquals(PackageManager.PERMISSION_GRANTED,c.checkUriPermission(uri,Process.myPid(),Process.myUid(),Intent.FLAG_GRANT_READ_URI_PERMISSION))
            release.complete(Unit);app.preferences.beforeRead={}
            queued(2);withTimeout(3000) {operation.result.first {it!=null}}
            assertEquals(2,operation.completed.filterNotNull().size)
            withTimeout(3000) {while(c.checkUriPermission(uri,Process.myPid(),Process.myUid(),Intent.FLAG_GRANT_READ_URI_PERMISSION)!=PackageManager.PERMISSION_DENIED)delay(10)}
            assertTrue(c.getSystemService(ActivityManager::class.java).getRunningServices(100).none {it.service.className==ShareAdmissionService::class.java.name})
        } finally {release.complete(Unit);app.preferences.beforeRead={}}
    }
    @Test(timeout=15000) fun homeKeepsSlowShareAndTemporaryGrantUntilAllItemsAdmit()=runBlocking<Unit> {
        caller("slow-image","image");entered()
        val activity=current.get()!!;assertFalse(activity.isFinishing)
        assertEquals(0,c.packageManager.getActivityInfo(ComponentName(c,ShareActivity::class.java),0).flags and ActivityInfo.FLAG_NO_HISTORY)
        device.pressHome();delay(200);assertFalse(activity.isDestroyed);assertFalse(activity.isFinishing)
        val uri=Uri.parse("content://io.github.stantonmatt.pneu.test.sources/image")
        assertEquals(PackageManager.PERMISSION_GRANTED,c.checkUriPermission(uri,Process.myPid(),Process.myUid(),Intent.FLAG_GRANT_READ_URI_PERMISSION))
        control("release");queued(2);delay(200)
        assertEquals(2,app.queue.pending().size);assertEquals(2,control("stats").getInt("opens"))
        withTimeout(3000) {while(!activity.isDestroyed)delay(20)}
        withTimeout(3000) {while(c.checkUriPermission(uri,Process.myPid(),Process.myUid(),Intent.FLAG_GRANT_READ_URI_PERMISSION)!=PackageManager.PERMISSION_DENIED)delay(10)}
    }
    @Test(timeout=15000) fun rotationMidMultiShareRetainsRecipientProgressAndReadGrant()=runBlocking<Unit> {
        caller("image","slow-image","third-image",keepAlive=true);entered();queued(1)
        val before=app.queue.pending().single();val original=currentShare();val operation=startedOperation(original)
        device.setOrientationLeft()
        withTimeout(3000) {while(current.get()==original || current.get()==null)delay(10)}
        i.waitForIdleSync();assertSame(operation,startedOperation(currentShare()))
        assertEquals(before.transferId,app.queue.find(before.mediaId)!!.transferId)
        val uri=Uri.parse("content://io.github.stantonmatt.pneu.test.sources/third-image")
        assertEquals(PackageManager.PERMISSION_GRANTED,c.checkUriPermission(uri,Process.myPid(),Process.myUid(),Intent.FLAG_GRANT_READ_URI_PERMISSION))
        control("release");queued(3);delay(250)
        assertEquals(3,app.queue.pending().size);assertEquals(3,control("stats").getInt("opens"))
        assertEquals(before.transferId,app.queue.find(before.mediaId)!!.transferId);assertEquals(3,operation.completed.size)
        assertEquals(3,operation.completed.filterNotNull().distinct().size)
    }
    @Test(timeout=15000) fun stalePermissionBaselineMustRetryRatherThanAdmitGapMedia()=runBlocking<Unit> {
        var readable=true;var intercept=false
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        val observer=ScreenshotObserver(app,latest={video->if(intercept && !video) {entered.complete(Unit);release.await();intercept=false};MediaCatalog.latest(c,video)},canRead={readable && !it})
        observer.scanOnce();val before=app.preferences.read()
        readable=false;observer.scanOnce();val gap=image();readable=true;intercept=true
        val scan=async(Dispatchers.IO) {observer.scanOnce()};entered.await()
        app.preferences.toggle("recordings",true);release.complete(Unit);scan.await()
        assertEquals(before.imageGeneration,app.preferences.read().imageGeneration)
        assertFalse(app.preferences.checkpoint(false,before.imageVersion,999999,expected=AdmissionToken.from(before,false)))
        assertFalse(app.checkpoint(DeliveryBinding("wrong", "wrong", "wrong"),false,before.imageVersion,999999))
        observer.scanOnce();observer.scanOnce();assertNull(app.queue.find(key(gap)))
        assertTrue(app.preferences.read().imageFloor>before.imageFloor)
        val fresh=image();observer.scanOnce();assertNotNull(app.queue.find(key(fresh)))
    }
}
