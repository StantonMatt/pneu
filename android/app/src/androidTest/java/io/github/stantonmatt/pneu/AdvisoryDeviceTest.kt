package io.github.stantonmatt.pneu

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.*
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
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class AdvisoryDeviceTest {
    private val instrumentation=InstrumentationRegistry.getInstrumentation()
    private val context=instrumentation.targetContext
    private val app=context.graph
    private val device=UiDevice.getInstance(instrumentation)
    private val args=InstrumentationRegistry.getArguments()
    private val sources=mutableListOf<Uri>()
    private fun uri(pin:String="a".repeat(64),name:String="Trusted%20PC",port:String=args.getString("pcPort") ?: "47861")="pneu://pair?v=1&name=$name&fp=$pin&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private suspend fun seed(pin:String="a".repeat(64)) {
        val raw=uri(pin);assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
    }
    private fun launch(extra:String="home") {context.startActivity(Intent(context,MainActivity::class.java).putExtra(extra,true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK));Thread.sleep(650)}
    private fun caller(uri:String,grant:Boolean=true,action:String=Intent.ACTION_SEND) {
        context.startActivity(Intent().setComponent(ComponentName(context.packageName+".test",FixtureCallerActivity::class.java.name)).putExtra("uri",uri).putExtra("grant",grant).putExtra("action",action).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));Thread.sleep(400)
    }
    private fun image(folder:String="Pictures/Screenshots/"):Uri {
        val source=context.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
            put("_display_name","Pneu_advisory_${System.nanoTime()}.png");put("mime_type","image/png");put("relative_path",folder);put("is_pending",1)
        })!!
        context.contentResolver.openOutputStream(source)!!.use {out->Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {eraseColor(android.graphics.Color.CYAN);compress(Bitmap.CompressFormat.PNG,100,out);recycle()}}
        context.contentResolver.update(source,ContentValues().apply {put("is_pending",0)},null,null);sources.add(source);return source
    }
    private fun key(source:Uri)="external_primary:${MediaStore.getVersion(context,"external_primary")}:images:${ContentUris.parseId(source)}"
    private suspend fun sent(id:String)=withTimeout(10000) {while(app.queue.find(id)?.state!="sent")delay(50)}
    private suspend fun row(source:Uri)=withTimeout(7000) {while(app.queue.find(key(source))==null)delay(50);app.queue.find(key(source))!!}
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();context.stopService(Intent(context,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.POST_NOTIFICATIONS))instrumentation.uiAutomation.grantRuntimePermission(context.packageName,permission)
        app.preferences.toggle("setup",true);app.preferences.toggle("queue",true);app.preferences.toggle("screenshots",true);app.preferences.toggle("shareSheet",true);app.preferences.toggle("mobile",true);app.preferences.toggle("wifi",true);app.preferences.toggle("tailscale",true);app.preferences.toggle("paused",false)
        app.preferences.toggle("photos",false);app.preferences.toggle("recordings",false);app.preferences.toggle("clipboardFromPc",true)
    }
    @After fun cleanup()=runBlocking {
        device.pressHome();context.stopService(Intent(context,PneuService::class.java));app.connection.stop();app.forgetBinding()
        sources.forEach {context.contentResolver.delete(it,null,null)}
    }
    @Test(timeout=45000) fun externalPairingRequiresOwnConfirmationAndPreservesOldTrust()=runBlocking {
        seed();val old=app.preferences.flow.first().peer
        context.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(uri(name="New%20PC")),context,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        val title=device.findObject(UiSelector().text("Replace Trusted PC with New PC?"));assertTrue(title.waitForExists(5000))
        val screen=File(context.filesDir,"advisory-screens/replace-confirmation.png").apply {parentFile!!.mkdirs()}
        instrumentation.uiAutomation.takeScreenshot().let {bitmap->screen.outputStream().use {bitmap.compress(Bitmap.CompressFormat.PNG,100,it)};bitmap.recycle()}
        delay(700);assertEquals(old,app.preferences.flow.first().peer);assertNull(app.preferences.flow.first().pending)
        device.findObject(UiSelector().text("Cancel")).click();assertNull(app.preferences.flow.first().pending)
    }
    @Test(timeout=45000) fun cancelledAndSupersededResultsCannotCommitOrDeleteOldQueue()=runBlocking {
        seed();val old=app.preferences.flow.first().peer;val queued=ManualEntries.text(app,"Keep old queue","share")
        val first=uri(pin="c".repeat(64),name="First");app.requestPairing(first,old!!.pin)
        app.cancelPairing();assertFalse(app.installBinding(Invitation.parse(first),"e".repeat(32)))
        assertEquals(old,app.preferences.flow.first().peer);assertNotNull(app.queue.find(queued.mediaId))
        app.requestPairing(first,old.pin);val second=uri(pin="f".repeat(64),name="Second");app.requestPairing(second,old.pin)
        assertFalse(app.installBinding(Invitation.parse(first),"e".repeat(32)));assertEquals(Invitation.parse(second),app.preferences.flow.first().pending)
    }
    @Test(timeout=45000) fun durableReplacementJournalBlocksDeliveryUntilCleanup()=runBlocking {
        seed();val row=ManualEntries.text(app,"Retained until durable replacement","share")
        val next=uri(pin="c".repeat(64));app.requestPairing(next,app.preferences.flow.first().peer!!.pin)
        assertTrue(app.preferences.paired(Invitation.parse(next),"e".repeat(32),Invitation.parse(next)))
        assertNotNull(app.queue.find(row.mediaId));assertTrue(app.preferences.flow.first().deliveryReset)
        app.reconcileDelivery();assertNull(app.queue.find(row.mediaId));assertFalse(app.preferences.flow.first().deliveryReset)
    }
    @Test(timeout=45000) fun separateCallerUsesReadableMediaButCannotUsePrivateUris()=runBlocking {
        seed();val source=image("Pictures/Pneu advisory/");val before=app.queue.nextOrder()
        for(uri in listOf("file://${context.filesDir}/datastore/pneu.preferences_pb","content://${context.packageName}.files/shared/private","content://io.github.stantonmatt.pneu.test.sources/data/user/0/private"))caller(uri)
        caller("content://io.github.stantonmatt.pneu.test.sources/image",true,"unsupported.action")
        assertEquals(before,app.queue.nextOrder())
        caller(source.toString(),false) // Pneu can read this media without caller proof.
        withTimeout(5000) {while(app.queue.nextOrder()<before+1)delay(50)}
        caller("content://io.github.stantonmatt.pneu.test.sources/image")
        withTimeout(5000) {while(app.queue.pending().size<2)delay(50)}
        assertTrue(app.queue.pending().all {it.kind=="file"})
    }
    @Test(timeout=45000) fun multipleSharedTextsKeepLatestUnsentItem()=runBlocking<Unit> {
        seed();app.preferences.toggle("wifi",false)
        val before=app.queue.nextOrder()
        context.startActivity(Intent(context,ShareActivity::class.java).setAction(Intent.ACTION_SEND_MULTIPLE).setType("text/plain")
            .putCharSequenceArrayListExtra(Intent.EXTRA_TEXT,arrayListOf<CharSequence>("First shared text","Latest shared text")).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        withTimeout(5000) {while(app.queue.nextOrder()<before+2)delay(30)}
        assertEquals("Latest shared text",app.queue.pending().single().text)
        assertEquals("share",app.queue.pending().single().origin)
    }
    @Test(timeout=45000) fun stalledProviderCannotBlockLocalUnpair()=runBlocking {
        seed();caller("content://io.github.stantonmatt.pneu.test.sources/stalled")
        delay(200);val start=android.os.SystemClock.elapsedRealtime();app.connection.unpair()
        assertTrue(android.os.SystemClock.elapsedRealtime()-start<2000);assertNull(app.preferences.flow.first().peer)
        withTimeout(3000) {app.staging.idle {Unit}}
    }
    @Test(timeout=45000) fun routeInvariantIsAtomicUnderConcurrentToggles()=runBlocking {
        val results=awaitAll(async {runCatching {app.preferences.toggle("wifi",false)}},async {runCatching {app.preferences.toggle("tailscale",false)}})
        assertEquals(1,results.count {it.isFailure});val p=app.preferences.flow.first();assertTrue(p.wifi || p.tailscale)
    }
    @Test(timeout=45000) fun latestTextSupersedesFailedOlderTextAndOldTimestampHasNoBarrier()=runBlocking {
        seed();val first=ManualEntries.text(app,"Old pending","clipboard")
        app.queue.find(first.mediaId)!!.also {it.state="failed";it.error="clipboard_unavailable";app.queue.update(it)}
        val second=ManualEntries.text(app,"Latest pending","clipboard");assertEquals(listOf(second.mediaId),app.queue.pending().map {it.mediaId});assertEquals("superseded",app.queue.find(first.mediaId)!!.state);app.retry(first.mediaId);assertEquals("superseded",app.queue.find(first.mediaId)!!.state)
        fun incoming(id:String,time:Long,text:String)=json("transfer_id" to id.repeat(32),"created_at" to time,"text" to text,"origin" to "clipboard")
        val blocked=Receipt().apply {peerId="d".repeat(32);transferId="1".repeat(32);hash=sha256("Older OS failure".toByteArray());createdAt=1;state="pending"};app.queue.insertReceipt(blocked)
        assertEquals(Type.ACK,app.clipboard.receive("d".repeat(32),incoming("2",20,"New PC text")).type)
        val old=app.clipboard.receive("d".repeat(32),incoming("1",1,"Older OS failure"));assertTrue(old.body["deduplicated"].asBoolean)
        assertEquals(20L,app.queue.lastApplied("d".repeat(32)))
        assertTrue(app.clipboard.receive("d".repeat(32),incoming("3",20,"Equal timestamp")).body["deduplicated"].asBoolean)
        val receivedHash=app.preferences.flow.first().lastReceivedHash
        launch()
        instrumentation.runOnMainSync {
            val clip=context.getSystemService(ClipboardManager::class.java).primaryClip!!
            assertTrue(clip.description.extras!!.getBoolean(ClipboardDelivery.OWN_CLIP))
            assertEquals(receivedHash,clip.description.extras!!.getString(ClipboardDelivery.OWN_HASH))
        }
        app.preferences.toggle("paused",true)
        app.clipboard.receive("d".repeat(32),incoming("4",30,"Ignored immediately while paused"))
        assertEquals(receivedHash,app.preferences.flow.first().lastReceivedHash)
        app.preferences.toggle("paused",false);app.preferences.toggle("clipboardFromPc",false)
        app.clipboard.receive("d".repeat(32),incoming("5",40,"Ignored immediately while disabled"))
        assertEquals(receivedHash,app.preferences.flow.first().lastReceivedHash)
    }
    @Test(timeout=45000) fun retryRevisionRejectsEveryStaleCompletionAndError()=runBlocking {
        seed();val row=ManualEntries.text(app,"Retry reset","share");val stale=app.queue.find(row.mediaId)!!
        app.retry(row.mediaId);stale.state="failed";stale.mismatches=99;stale.nextAttempt=Long.MAX_VALUE
        assertFalse(app.updateDelivery(stale));stale.state="sent";assertFalse(app.updateDelivery(stale));assertEquals(0,app.queue.find(row.mediaId)!!.mismatches);assertEquals(0L,app.queue.find(row.mediaId)!!.nextAttempt)
    }
    @Test(timeout=45000) fun terminalStagingAndOrphanPreviewsAreReclaimed()=runBlocking {
        seed();val root=File(context.filesDir,"outgoing").apply {mkdirs()}
        for(state in listOf("sent","discarded","waiting")) {
            val file=File(root,"${randomId()}.bin").apply {writeText("Fixture")}
            val row=Outgoing().apply {mediaId=randomId();transferId=randomId();this.state=state;path=file.path;kind="file"};app.enqueueFixture(row)
        }
        val orphan=File(context.cacheDir,"thumbnails/orphan.part").apply {parentFile!!.mkdirs();writeText("Fixture")}
        app.staging.cleanup();assertFalse(orphan.exists())
        assertEquals(1,root.listFiles().orEmpty().count {it.extension=="bin"})
    }
    @Test(timeout=45000) fun completeCollectionCheckpointsIgnoreClocksAndSkipPrePairMedia()=runBlocking {
        val old=image();seed();val oldGeneration=context.contentResolver.query(old,arrayOf("generation_added"),null,null,null)!!.use {it.moveToFirst();it.getLong(0)};assertTrue(app.preferences.flow.first().imageGeneration>oldGeneration);val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val observer=ScreenshotObserver(app) {video->!video};observer.scanOnce();observer.start(scope)
        try {
            val before=app.preferences.flow.first().videoGeneration
            val source=image();context.contentResolver.update(source,ContentValues().apply {put("date_added",1)},null,null)
            app.requestMediaScan();assertNotNull(row(source));assertNull(app.queue.find(key(old)))
            val p=withTimeout(4000) {app.preferences.flow.first {it.imageGeneration>before}}
            assertEquals(before,p.videoGeneration)
            app.preferences.checkpoint(false,"rebuilt-fixture",0,999999)
            app.requestMediaScan();withTimeout(4000) {app.preferences.flow.first {it.imageVersion==MediaStore.getVersion(context,"external_primary")}}
            val afterRebuild=image();app.requestMediaScan();assertNotNull(row(afterRebuild))
        } finally {observer.stop();scope.cancel()}
    }
    @Test(timeout=45000) fun shareVisibilityHasNoDirectShareMetadata()=runBlocking {
        seed();launch();ManualEntries.configureShareTarget(context,true)
        val pm=context.packageManager;val info=pm.getActivityInfo(ComponentName(context,MainActivity::class.java),android.content.pm.PackageManager.GET_META_DATA)
        assertFalse(info.metaData?.containsKey("android.app.shortcuts")==true)
        app.preferences.toggle("shareSheet",false);ManualEntries.configureShareTarget(context,false)
        assertEquals(android.content.pm.PackageManager.COMPONENT_ENABLED_STATE_DISABLED,pm.getComponentEnabledSetting(ComponentName(context,io.github.stantonmatt.pneu.features.ShareActivity::class.java)))
        app.preferences.toggle("shareSheet",true);ManualEntries.configureShareTarget(context,true)
    }
    @Test(timeout=45000) fun toolbarAndExternalHomeNavigationAgreeAndQueueCountIsComplete()=runBlocking {
        seed();repeat(60) {app.enqueueFixture(Outgoing().apply {mediaId=randomId();transferId=randomId();kind="file";fileName="Fixture"})}
        withTimeout(5000) {app.waitingCount.first {it==60}};launch()
        device.findObject(UiSelector().description("Settings")).click();assertTrue(device.findObject(UiSelector().text("Settings")).waitForExists(4000));delay(350)
        val scroll=UiScrollable(UiSelector().scrollable(true));scroll.scrollTextIntoView("Permissions and battery")
        device.findObject(UiSelector().text("Permissions and battery")).click();device.findObject(UiSelector().description("Back")).click()
        assertTrue(device.findObject(UiSelector().text("Settings")).waitForExists(3000))
        context.startActivity(Intent(context,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        assertTrue(device.findObject(UiSelector().text("Pneu")).waitForExists(3000))
        device.findObject(UiSelector().description("Settings")).click();assertTrue(device.findObject(UiSelector().text("Settings")).waitForExists(4000));delay(350);UiScrollable(UiSelector().scrollable(true)).scrollTextIntoView("Unpair")
        device.findObject(UiSelector().text("Unpair")).click();assertTrue(device.findObject(UiSelector().textContains("60")).waitForExists(3000))
    }
    @Test(timeout=45000) fun permanentFailureRemainsVisibleOnMobile()=runBlocking<Unit> {
        seed();app.preferences.toggle("originals",false)
        val failed=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="photo";automatic=true;attempted=true;state="failed";error="id_conflict";nextAttempt=Long.MAX_VALUE}
        assertTrue(app.enqueueFixture(failed));app.connection.state.value=io.github.stantonmatt.pneu.connection.LinkState(cellular=true)
        launch();assertTrue(device.findObject(UiSelector().text("Failed")).waitForExists(4000))
        assertTrue(device.findObject(UiSelector().description("Retry")).exists())
        assertTrue(device.findObject(UiSelector().description("Remove")).exists())
        val retained=app.queue.find(failed.mediaId)!!
        assertEquals("failed",retained.state);assertEquals("id_conflict",retained.error);assertEquals(Long.MAX_VALUE,retained.nextAttempt)
    }
    @Test(timeout=45000) fun blockedStatusChannelOpensChannelSettingsImmediately()=runBlocking {
        val nm=context.getSystemService(NotificationManager::class.java);nm.deleteNotificationChannel("status");nm.createNotificationChannel(NotificationChannel("status","Status",NotificationManager.IMPORTANCE_NONE))
        try {launch("permissions");device.findObject(UiSelector().text("Settings")).click();assertTrue(device.wait(Until.hasObject(By.pkg("com.android.settings")),3000))}
        finally {device.pressHome();nm.deleteNotificationChannel("status");nm.createNotificationChannel(NotificationChannel("status","Status",NotificationManager.IMPORTANCE_LOW))}
    }
    @Test(timeout=45000) fun killEvidenceSurvivesReconnectAndRepairAcknowledgesIt()=runBlocking {
        seed();val evidence=context.getSharedPreferences("service_evidence",Context.MODE_PRIVATE)
        evidence.edit().putLong("kill",100).putLong("acknowledged",0).putBoolean("running",false).commit()
        app.preferences.toggle("neverSleeping",true);app.prefs.first {it.neverSleeping};context.health();withTimeout(3000) {app.prefs.first {!it.neverSleeping}}
        assertTrue(ServiceEvidence.killed(context));app.preferences.setLong("lastConnected",Long.MAX_VALUE);assertTrue(ServiceEvidence.killed(context))
        ServiceEvidence.acknowledge(context);assertFalse(ServiceEvidence.killed(context))
    }
}
