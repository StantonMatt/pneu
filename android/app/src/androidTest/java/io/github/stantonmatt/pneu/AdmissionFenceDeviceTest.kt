package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Application
import android.content.*
import android.graphics.Bitmap
import android.net.Uri
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.Connectivity
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class AdmissionFenceDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val a="a".repeat(32);private val b="b".repeat(32)
    private val fixtures=mutableListOf<LocalPeerFixture>();private val sources=mutableListOf<Uri>()
    private val provider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
    private fun control(method:String)=c.contentResolver.call(provider,method,null,null)!!
    private suspend fun bind(id:String,pin:Char) {
        val raw="pneu://pair?v=1&name=$pin&fp=${pin.toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"d".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(raw),id))
        withTimeout(5000) {app.prefs.first {it.devices.any {peer->peer.deviceId==id}}}
    }
    private suspend fun online(id:String)=LocalPeerFixture(app,setOf("files","clipboard_text","clipboard_image","mesh"))
        .also {fixtures+=it;it.connect(id)}
    private suspend fun reconnect(old:DeliveryBinding) {
        app.connection.retryPeer(old.pin)
        withTimeout(15000) {while(app.recipients(app.preferences.read(),"text").none {it.pcId==old.pcId && it.sessionId!=old.sessionId})delay(20)}
        assertFalse(app.connection.ownsSession(old))
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset();app.initialized.await()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true);app.preferences.toggle("queue",true);app.preferences.toggle("originals",true)
        bind(a,'a');online(a)
    }
    @After fun cleanup()=runBlocking<Unit> {
        control("release");app.preferences.beforeReadStart={};app.staging.beforeAutomaticPayloadWrite={};app.staging.beforePayloadWrite={}
        app.connection.stop();app.connection.awaitStopped();fixtures.forEach {it.close()}
        sources.forEach {c.contentResolver.delete(it,null,null)};DeviceFixture.reset()
    }
    @Test(timeout=45000) fun delayedClipboardMimeCannotJoinTheNextSessionOrANewRecipient()=runBlocking<Unit> {
        control("reset")
        val admission=app.captureAdmission();val old=admission.await().images.single()
        val operation=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.captureClipboardAdmission(admission);it.clipboard(ClipData.newRawUri("image",provider.buildUpon().appendPath("slow-type").build()))
        }}
        try {
            assertTrue(withContext(Dispatchers.IO) {control("entered").getBoolean("entered")})
            reconnect(old);bind(b,'b');online(b)
            control("release");withTimeout(5000) {operation.result.filterNotNull().first()}
            assertTrue(operation.completed.isEmpty());assertTrue(app.queue.all().isEmpty())
        }finally {control("release")}
    }
    @Test(timeout=45000) fun serializedClipboardActionsRetainTheirSessionAndLegacyActionsCannotAcquireOne()=runBlocking<Unit> {
        val p=app.preferences.read();val intent=ManualEntries.clipboardIntent(c,p)
        val frozen=ManualEntries.captureClipboard(app,intent).await();val old=frozen.text.single()
        assertNotNull(old.sessionId);reconnect(old)
        assertEquals(old,ManualEntries.captureClipboard(app,intent).await().text.single())
        assertFalse(app.connection.ownsSession(old))
        val binding=DeliveryBinding.from(p)!!
        val legacy=Intent().putExtra("pneu.admission.targets",com.google.gson.Gson().toJson(FrozenAdmission(binding,listOf(binding),listOf(binding),emptyList())))
        val rejected=ManualEntries.captureClipboard(app,legacy).await()
        assertTrue(rejected.text.isEmpty());assertTrue(rejected.images.isEmpty())
    }
    @Test(timeout=45000) fun clipboardAndSharedTextRevalidateAfterSuspendingSettingsReads()=runBlocking<Unit> {
        for(origin in listOf("clipboard","share")) {
            val frozen=app.captureAdmission().await();val old=frozen.text.single()
            val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>();val once=AtomicBoolean()
            app.preferences.beforeReadStart={if(currentCoroutineContext()[CoroutineName]?.name=="fenced-text" && once.compareAndSet(false,true)) {entered.complete(Unit);release.await()}}
            val sending=async(Dispatchers.IO+CoroutineName("fenced-text")) {runCatching {
                ManualEntries.text(app,"Old $origin",origin,frozen.binding,frozen.text)
            }}
            try {
                withTimeout(5000) {entered.await()};reconnect(old);release.complete(Unit)
                assertTrue(withTimeout(5000) {sending.await()}.isFailure);assertTrue(app.queue.all().isEmpty())
            }finally {release.complete(Unit);app.preferences.beforeReadStart={};sending.cancelAndJoin()}
        }
    }
    @Test(timeout=45000) fun finalPublicationRejectsExpiredClipboardAndRetainsSurvivingRecipients()=runBlocking<Unit> {
        bind(b,'b');online(b)
        val p=app.preferences.read();val targets=app.recipients(p,"clipboard_image");val old=targets.first {it.pcId==a}
        val row=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();kind="clipboard_image";automatic=false;recipients=targets}
        // Both disconnect and READY cleanup finish before this delayed producer publishes.
        reconnect(old)
        assertTrue(app.enqueue(row,DeliveryBinding.from(p)))
        assertEquals(listOf(b),app.queue.deliveries(row.mediaId).map {it.peerId})
        val stale=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();kind="text";automatic=false;recipients=listOf(old)}
        assertFalse(app.enqueue(stale,old));assertNull(app.queue.find(stale.mediaId))
    }
    private fun media(kind:String,pending:Boolean=false):Uri {
        val video=kind=="recording"
        val collection=if(video)MediaStore.Video.Media.getContentUri("external_primary") else MediaStore.Images.Media.getContentUri("external_primary")
        val uri=c.contentResolver.insert(collection,ContentValues().apply {
            put("_display_name","Pneu_admission_${randomId()}.${if(video)"mp4" else "png"}")
            put("mime_type",if(video)"video/mp4" else "image/png")
            put("relative_path",when(kind) {"recording"->"Movies/Screen recordings/";"photo"->"DCIM/Camera/";else->"Pictures/Screenshots/"})
            put("is_pending",1)
        })!!;sources+=uri
        c.contentResolver.openOutputStream(uri)!!.use {out->
            if(video)out.write(byteArrayOf(1,2,3,4)) else Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,out);recycle()}
        }
        if(!pending)c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)
        return uri
    }
    private fun key(uri:Uri,kind:String)="external_primary:${MediaStore.getVersion(c,"external_primary")}:${if(kind=="recording")"video" else "images"}:${ContentUris.parseId(uri)}"
    private suspend fun observer():ScreenshotObserver {
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,true)
        return ScreenshotObserver(app,connectivity={Connectivity.UNMETERED}).also {it.scanOnce();it.scanOnce()}
    }
    @Test(timeout=60000) fun automaticImageAndVideoRetriesKeepDiscoveryMetadataAcrossObserverRestartAndPairing()=runBlocking<Unit> {
        var observer=observer()
        for(kind in listOf("screenshot","photo","recording")) {
            val uri=media(kind);val id=key(uri,kind)
            app.staging.beforeAutomaticPayloadWrite={if(it.mediaId==id)throw java.io.IOException("Transient provider failure")}
            observer.scanOnce()
            val frozen=app.queue.pendingAdmission(id)!!.row();assertNull(app.queue.find(id))
            val newer=app.newTextOrder().first
            if(kind=="screenshot") {bind(b,'b');online(b)}
            // A fresh observer must load the journal rather than rediscover targets/order.
            observer=ScreenshotObserver(app,connectivity={Connectivity.UNMETERED})
            // Fail through both startup baselines too: neither may abandon a
            // pending operation or change its discovery authority.
            observer.scanOnce();observer.scanOnce()
            assertEquals(frozen.hlc,app.queue.pendingAdmission(id)!!.row().hlc)
            app.staging.beforeAutomaticPayloadWrite={};observer.scanOnce();observer.scanOnce();observer.scanOnce()
            val admitted=app.queue.find(id)!!
            assertEquals(frozen.hlc,admitted.hlc)
            assertEquals(frozen.sourceDevice,admitted.sourceDevice)
            assertEquals(frozen.recipients.map {it.pcId}.toSet(),app.queue.deliveries(id).map {it.peerId}.toSet())
            if(kind!="recording")assertTrue(admitted.hlc.toBigInteger()<newer.hlc)
            if(kind=="screenshot")assertEquals(listOf(a),app.queue.deliveries(id).map {it.peerId})
            assertNull(app.queue.pendingAdmission(id))
        }
    }
    @Test(timeout=45000) fun overlappingCaptureRetriesCannotReplaceOrDeleteTheAdmittedPayload()=runBlocking<Unit> {
        val observer=observer();val uri=media("screenshot");val id=key(uri,"screenshot")
        app.staging.beforeAutomaticPayloadWrite={if(it.mediaId==id)throw java.io.IOException("Transient provider failure")}
        observer.scanOnce();assertNotNull(app.queue.pendingAdmission(id))
        val entered=CountDownLatch(1);val release=CountDownLatch(1);val writes=AtomicInteger()
        app.staging.beforeAutomaticPayloadWrite={if(it.mediaId==id && writes.getAndIncrement()==0) {
            entered.countDown();check(release.await(10,TimeUnit.SECONDS))
        }}
        val old=async(Dispatchers.IO) {observer.scanOnce()}
        try {
            assertTrue(withContext(Dispatchers.IO) {entered.await(5,TimeUnit.SECONDS)})
            ScreenshotObserver(app,connectivity={Connectivity.UNMETERED}).scanOnce()
            val accepted=app.queue.find(id)!!;val payload=java.io.File(accepted.path)
            assertTrue(payload.isFile);assertEquals(accepted.hash,sha256(payload.readBytes()))
            release.countDown();withTimeout(5000) {old.await()}
            assertEquals(accepted.transferId,app.queue.find(id)!!.transferId)
            assertTrue(payload.isFile);assertEquals(accepted.hash,sha256(payload.readBytes()))
            assertEquals(1,app.queue.deliveries(id).size)
        }finally {release.countDown();old.cancelAndJoin();app.staging.beforeAutomaticPayloadWrite={}}
    }
    @Test(timeout=45000) fun featureOffOnRevokesPendingDiscoveryEvenAcrossObserverRestart()=runBlocking<Unit> {
        val observer=observer();val uri=media("screenshot");val id=key(uri,"screenshot")
        app.staging.beforeAutomaticPayloadWrite={if(it.mediaId==id)throw java.io.IOException("Transient provider failure")}
        observer.scanOnce();assertNotNull(app.queue.pendingAdmission(id))
        app.preferences.toggle("screenshots",false);app.preferences.toggle("screenshots",true)
        app.staging.beforeAutomaticPayloadWrite={}
        ScreenshotObserver(app,connectivity={Connectivity.UNMETERED}).scanOnce()
        assertNull(app.queue.find(id));assertNull(app.queue.pendingAdmission(id));assertEquals(1,app.queue.seen(id))
    }
    @Test(timeout=60000) fun expiredAutomaticRetrySkipsWithoutQueueButExplicitQueueRetainsOriginalRecipient()=runBlocking<Unit> {
        val observer=observer()
        for(queueOffline in listOf(false,true)) {
            app.preferences.toggle("queue",queueOffline)
            val uri=media("screenshot");val id=key(uri,"screenshot")
            app.staging.beforeAutomaticPayloadWrite={if(it.mediaId==id)throw java.io.IOException("Transient provider failure")}
            observer.scanOnce()
            val frozen=app.queue.pendingAdmission(id)!!.row()
            val old=app.recipients(app.preferences.read(),"text").single();reconnect(old)
            app.staging.beforeAutomaticPayloadWrite={};observer.scanOnce()
            assertNull(app.queue.pendingAdmission(id));assertEquals(1,app.queue.seen(id))
            if(queueOffline) {
                val row=app.queue.find(id)!!;assertEquals(frozen.hlc,row.hlc);assertEquals(a,row.peerId)
            } else assertNull(app.queue.find(id))
        }
    }
    @Test(timeout=45000) fun pendingMediaStoreRowKeepsItsOriginalAdmissionWhileWaitingForPublication()=runBlocking<Unit> {
        val observer=observer();val uri=media("screenshot",pending=true);val id=key(uri,"screenshot")
        observer.scanOnce();val frozen=app.queue.pendingAdmission(id)!!.row()
        val newer=app.newTextOrder().first;bind(b,'b');online(b)
        c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)
        observer.scanOnce()
        val row=app.queue.find(id)!!
        assertEquals(frozen.hlc,row.hlc);assertTrue(row.hlc.toBigInteger()<newer.hlc)
        assertEquals(listOf(a),app.queue.deliveries(id).map {it.peerId})
    }
    @Test(timeout=45000) fun deliberateSharedFileSurvivesReconnectWithoutExpandingRecipients()=runBlocking<Unit> {
        val frozen=app.captureAdmission().await();val old=frozen.text.single()
        reconnect(old);bind(b,'b');online(b)
        val row=ManualEntries.file(app,provider.buildUpon().appendPath("image").build(),"file",frozen.binding,targets=frozen.files)
        assertEquals(listOf(a),app.queue.deliveries(row.mediaId).map {it.peerId})
    }
}
