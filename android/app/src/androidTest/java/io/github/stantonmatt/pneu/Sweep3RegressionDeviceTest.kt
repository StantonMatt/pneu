package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Application
import android.content.*
import android.net.Uri
import android.provider.MediaStore
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.runner.lifecycle.ActivityLifecycleMonitorRegistry
import androidx.test.runner.lifecycle.Stage
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Permanent race regressions for Android receive/UI/security sweep 3. */
@RunWith(AndroidJUnit4::class)
class Sweep3RegressionDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val a="a".repeat(32);private val b="b".repeat(32)
    private val owned=mutableSetOf<Uri>()
    private val bytes=android.util.Base64.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==",0)
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.CAMERA,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true);app.preferences.toggle("screenshots",false);app.preferences.toggle("queue",true)
        bind(a)
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeReadStart={}
        c.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.review"),"share-control","release",null)
        for(row in app.receivedRecent.first()+app.queue.unfinishedReceivedFiles())if(row.uri.isNotEmpty())owned+=Uri.parse(row.uri)
        DeviceFixture.reset()
        owned.forEach {c.contentResolver.delete(it,null,null)}
    }
    private suspend fun bind(id:String) {
        val raw="pneu://pair?v=1&name=Computer%20${id.first()}&fp=${id.first().toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"c".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(raw),id))
        val binding=requireNotNull(DeliveryBinding.from(app.preferences.read(),id))
        withTimeout(5000) {app.prefs.first {binding.matches(it)}}
    }
    private suspend fun authority(peer:String=a,connection:String?=null):CapturedAuthority {
        val binding=requireNotNull(DeliveryBinding.from(app.preferences.read(),peer))
        if(connection!=null)app.withBinding(binding) {app.connection.admitted(binding,connection)}
        return CapturedAuthority(binding,connection)
    }
    private fun metadata(id:String=randomId(),size:Long=bytes.size.toLong(),kind:String="file",mime:String="image/png")=
        ReceivedMetadata(id,"Sweep3-${randomId()}.png",mime,size,sha256(if(size==0L)byteArrayOf() else bytes),kind,System.currentTimeMillis())
    private suspend fun send(meta:ReceivedMetadata,files:ReceivedFiles=app.receivedFiles,peer:String=a):Frame.Control {
        val receiver=files.connection(authority(peer),false) {true}
        receiver.begin(com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject)
        if(meta.size>0)receiver.chunk(Frame.Chunk(meta.id,0,bytes))
        val result=receiver.end(meta.id)
        app.queue.receivedFile(peer,meta.id)?.let {if(it.uri.isNotEmpty())owned+=Uri.parse(it.uri)}
        return result
    }
    private suspend fun saved():ReceivedFile {
        val meta=metadata();assertEquals(Type.ACK,send(meta).type)
        return app.queue.receivedFile(a,meta.id)
    }
    @Test(timeout=45000) fun pickerSelectionOnlyNarrowsOriginalBindingsAfterRePairAndLeavesOtherRecipient()=runBlocking<Unit> {
        bind(b);app.preferences.toggle("chooseDevicesEachTime",true)
        val captured=app.captureAdmission().await()
        val operation=ManualEntryViewModel(c.applicationContext as Application).apply {targetPickerAvailable=true}
        val uri=Uri.parse("content://io.github.stantonmatt.pneu.test.review/image")
        i.runOnMainSync {operation.share(ShareRequest(CompletableDeferred(captured.binding),listOf(uri),emptyList(),CompletableDeferred(captured)))}
        withTimeout(5000) {operation.targetSelection.filterNotNull().first()}
        val old=captured.fileChoices.first {it.pcId==b}
        assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),b)));bind(b)
        val newer=requireNotNull(DeliveryBinding.from(app.preferences.read(),b));assertNotEquals(old.generation,newer.generation)
        i.runOnMainSync {operation.selectTargets(setOf(a,b))}
        withTimeout(10000) {operation.result.filterNotNull().first()}
        assertEquals(1,operation.completed.filterNotNull().size)
        val rows=app.queue.deliveries(operation.completed.filterNotNull().single())
        assertEquals(listOf(a),rows.map {it.peerId});assertFalse(rows.any {it.peerId==b})
    }
    @Test(timeout=45000) fun devicePickerSavedAuthorityCannotSendToReplacementOrEditItsSwitches()=runBlocking<Unit> {
        val original=authority();val restored=CapturedAuthority.restore(original.saved())!!
        assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),a)));bind(a)
        assertFalse(app.setPairSwitch(restored,"sendClipboard",false))
        assertTrue(app.preferences.read().devices.single().clipboardSend)
        ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("home",true)).use {scenario->
            var rejected=false
            scenario.onActivity {activity->try {sendFilesTo(activity,restored,listOf(Uri.parse("content://io.github.stantonmatt.pneu.test.review/image")))}catch(_:IllegalStateException) {rejected=true}}
            assertTrue(rejected);assertTrue(app.queue.all().isEmpty())
        }
    }
    @Test(timeout=45000) fun devicePageAdmissionOwnsTemporaryGrantAfterMainTaskIsRemoved()=runBlocking<Unit> {
        val provider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
        fun control(method:String)=c.contentResolver.call(provider,"share-control",method,null)!!
        control("reset")
        val source=Uri.parse("content://io.github.stantonmatt.pneu.test.sources/slow-image")
        val frozen=authority()
        c.startActivity(Intent().setComponent(ComponentName(c.packageName+".test",FixtureCallerActivity::class.java.name))
            .putExtra("uri",source.toString()).putExtra("main",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        var activity:MainActivity?=null
        withTimeout(5000) {
            while(activity==null) {
                i.runOnMainSync {activity=ActivityLifecycleMonitorRegistry.getInstance().getActivitiesInStage(Stage.RESUMED).filterIsInstance<MainActivity>().firstOrNull()}
                delay(25)
            }
        }
        lateinit var operation:ManualEntryViewModel
        try {
            i.runOnMainSync {operation=sendFilesTo(activity!!,frozen,listOf(source))}
            withTimeout(5000) {while(!control("status").getBoolean("entered"))delay(20)}
            i.runOnMainSync {activity!!.finishAndRemoveTask()};i.waitForIdleSync()
            control("release")
            withTimeout(10000) {operation.result.filterNotNull().first()}
            val row=app.queue.find(operation.completed.filterNotNull().single())
            assertEquals(a,row.peerId);assertEquals(frozen.binding.generation,row.bindingGeneration)
            assertTrue(java.io.File(row.path).isFile);assertEquals("",row.uri)
        }finally {control("release");i.runOnMainSync {activity?.finishAndRemoveTask()}}
    }
    @Test(timeout=45000) fun queuedClipboardEffectKeepsOriginalConnectionAcrossDisconnectButRejectsNewAdmission()=runBlocking<Unit> {
        val row=saved();val original=authority(connection="old-${randomId()}")
        val meta=ReceivedMetadata.parse(com.google.gson.JsonParser.parseString(row.metadata).asJsonObject).copy(kind="clipboard_image",hlc=System.currentTimeMillis().toString(),source=a)
        val effects=ReceivedFileEffects(app);var writes=0
        app.connection.stop();app.connection.awaitStopped()
        effects.applyClipboard(row,meta,true,original) {writes++}
        assertEquals("Ordinary disconnect preserves a completed action",1,writes)
        val newer=meta.copy(hlc=(meta.hlc!!.toLong()+1).toString())
        app.withBinding(original.binding) {app.connection.admitted(original.binding,"new-${randomId()}")}
        effects.applyClipboard(row,newer,true,original) {writes++}
        assertEquals("A newer connection must fence the queued image",1,writes)
    }
    @Test(timeout=45000) fun notificationRechecksCapturedAuthorityAfterThumbnailAndCannotNotifyFollowingUnpair()=runBlocking<Unit> {
        val row=saved();val original=authority(connection="notification-${randomId()}")
        val entered=CountDownLatch(1);val release=CountDownLatch(1);var notifications=0
        val posting=async(Dispatchers.IO) {ReceivedFileNotifications.post(app,row,original,thumbnail={entered.countDown();check(release.await(10,TimeUnit.SECONDS));null},notify={notifications++})}
        try {
            assertTrue(entered.await(5,TimeUnit.SECONDS))
            assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),a)))
            release.countDown();withTimeout(5000) {posting.await()}
            assertEquals(0,notifications)
        }finally {release.countDown();posting.cancelAndJoin()}
    }
    @Test(timeout=45000) fun staleReceiveDisconnectCannotEraseReplacementBindingsPartialAndReceipt()=runBlocking<Unit> {
        val meta=metadata();val old=app.receivedFiles.connection(authority(),false) {true}
        old.begin(com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject)
        assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),a)));bind(a)
        val current=app.receivedFiles.connection(authority(),false) {true}
        current.begin(com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject)
        val row=app.queue.receivedFile(a,meta.id);owned+=Uri.parse(row.uri)
        old.disconnect()
        assertEquals(row.uri,app.queue.receivedFile(a,meta.id).uri);assertNotNull(app.queue.receipt(a,meta.id))
        current.chunk(Frame.Chunk(meta.id,0,bytes));assertEquals(Type.ACK,current.end(meta.id).type)
    }
    @Test(timeout=45000) fun receivedActionsValidateCatalogOwnerPublicationAndExactIdentityAtEveryTap()=runBlocking<Unit> {
        val row=saved();val cached=ReceivedFileNotifications.shareIntent(c,row)
        assertEquals(ReceivedFileActionActivity::class.java.name,cached.component!!.className)
        assertEquals(0,cached.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION)
        val expected=ReceivedFileActions.expected(cached)!!
        val resolved=ReceivedFileActions.resolve(c,expected,true)!!
        assertEquals(Intent.ACTION_SEND,resolved.action);assertEquals(Uri.parse(row.uri),resolved.clipData!!.getItemAt(0).uri)
        val real=ReceivedMediaStore(c)
        class Catalog:ReceiveStorage by real {
            var catalog=row.catalogVersion;var state:Boolean?=false
            override fun version()=catalog
            override fun pending(uri:String)=state
        }
        val fake=Catalog()
        fake.catalog="rebuilt";assertNull(ReceivedFileActions.resolve(c,expected,false,fake));assertNull(ReceivedFileActions.resolve(c,expected,true,fake))
        fake.catalog=row.catalogVersion;fake.state=true;assertNull(ReceivedFileActions.resolve(c,expected,false,fake))
        fake.state=null;assertNull("A reused ID owned by another package is not ours",ReceivedFileActions.resolve(c,expected,true,fake))
        fake.state=false;assertNotNull(ReceivedFileActions.resolve(c,expected,false,fake))
        app.queue.updateReceivedFile(row.apply {generation++})
        assertNull("Cached actions cannot alias a replacement journal",ReceivedFileActions.resolve(c,expected,true,fake))
        app.queue.updateReceivedFile(row.apply {generation--})
        assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),a)))
        assertNull(ReceivedFileActions.resolve(c,expected,true))
    }
    @Test(timeout=45000) fun emptyFreshTransfersHitDurablePeerAndGlobalCapsButRetainedReplayStillAcks()=runBlocking<Unit> {
        bind(b)
        val files=ReceivedFiles(app,peerLimit=2,globalLimit=3)
        val first=metadata(size=0,mime="application/octet-stream")
        assertEquals(Type.ACK,send(first,files).type)
        assertEquals(Type.ACK,send(first.copy(id=randomId()),files).type)
        val rejected=send(first.copy(id=randomId()),files)
        assertEquals(Type.NACK,rejected.type);assertEquals("storage_failed",rejected.body["code"].asString);assertTrue(rejected.body["retryable"].asBoolean)
        assertEquals(Type.ACK,send(first.copy(id=randomId()),files,b).type)
        assertEquals("storage_failed",send(first.copy(id=randomId()),files,b).body["code"].asString)
        val replay=send(first,files);assertEquals(Type.ACK,replay.type);assertTrue(replay.body["deduplicated"].asBoolean)
        assertEquals("id_conflict",send(first.copy(name="changed"),files).body["code"].asString)
        assertEquals(3,app.queue.receivedFileCount());assertEquals(3,app.queue.receiptCount())
    }
}
