package io.github.stantonmatt.pneu

import android.Manifest
import android.content.ContentValues
import android.net.Uri
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ReceiveStorageDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val a="a".repeat(32);private val b="b".repeat(32);private val owned=mutableSetOf<Uri>()
    private val bytes=android.util.Base64.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==",0)
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        bind(a)
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeMutation={}
        for(row in app.receivedRecent.first())owned+=Uri.parse(row.uri)
        for(row in app.queue.unfinishedReceivedFiles())if(row.uri.isNotEmpty())owned+=Uri.parse(row.uri)
        owned.forEach {c.contentResolver.delete(it,null,null)}
        DeviceFixture.reset()
    }
    private suspend fun bind(id:String) {
        val raw="pneu://pair?v=1&name=Computer%20${id.first()}&fp=${id.first().toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(raw),id))
    }
    private fun metadata(id:String=randomId(),kind:String="file",mime:String="image/png",stamp:Long?=null)=ReceivedMetadata(id,"Pneu received ${randomId()}.png",mime,bytes.size.toLong(),sha256(bytes),kind,System.currentTimeMillis(),
        hlc=stamp?.toString(),source=if(stamp==null)null else a)
    private suspend fun receiver(id:String=a,mesh:Boolean=true)=app.receivedFiles.connection(requireNotNull(DeliveryBinding.from(app.preferences.read(),id)),mesh) {true}
    private suspend fun send(metadata:ReceivedMetadata,peer:String=a,mesh:Boolean=true):Frame.Control {
        val receiver=receiver(peer,mesh);receiver.begin(com.google.gson.JsonParser.parseString(metadata.canonical).asJsonObject)
        receiver.chunk(Frame.Chunk(metadata.id,0,bytes));return receiver.end(metadata.id)
    }
    @Test(timeout=45000) fun pendingFilesPublishOnlyAfterVerificationAndNamesAreUnique()=runBlocking<Unit> {
        val meta=metadata();val receiver=receiver()
        receiver.begin(com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject)
        val pending=app.queue.receivedFile(a,meta.id);owned+=Uri.parse(pending.uri)
        c.contentResolver.query(Uri.parse(pending.uri),arrayOf("is_pending"),null,null,null)!!.use {assertTrue(it.moveToFirst());assertEquals(1,it.getInt(0))}
        receiver.chunk(Frame.Chunk(meta.id,0,bytes));assertEquals(Type.ACK,receiver.end(meta.id).type)
        val first=app.queue.receivedFile(a,meta.id);assertEquals("complete",first.state)
        val second=meta.copy(id=randomId());assertEquals(Type.ACK,send(second).type)
        val collision=app.queue.receivedFile(a,second.id);owned+=Uri.parse(collision.uri)
        assertNotEquals(first.uri,collision.uri);assertNotEquals(first.fileName,collision.fileName)
        assertEquals("applied",app.queue.receipt(a,meta.id).state)
        val share=ReceivedFileActions.resolve(c,first,share=true)!!
        assertEquals(android.content.Intent.ACTION_SEND,share.action);assertEquals(first.mime,share.type)
        assertEquals(Uri.parse(first.uri),share.clipData!!.getItemAt(0).uri)
        assertTrue(share.flags and android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION!=0)
    }
    @Test(timeout=45000) fun automaticSharingPauseDoesNotBlockAnExplicitReceivedFile()=runBlocking<Unit> {
        app.preferences.toggle("paused",true)
        val meta=metadata(kind="file")
        assertEquals(Type.ACK,send(meta).type)
        assertEquals("complete",app.queue.receivedFile(a,meta.id).state)
    }
    @Test(timeout=45000) fun disconnectAndHashMismatchRemovePendingRows()=runBlocking<Unit> {
        val meta=metadata();val receiver=receiver();receiver.begin(com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject)
        val uri=Uri.parse(app.queue.receivedFile(a,meta.id).uri);receiver.chunk(Frame.Chunk(meta.id,0,bytes.copyOf(2)));receiver.disconnect()
        assertNull(app.queue.receivedFile(a,meta.id));assertNull(app.queue.receipt(a,meta.id))
        c.contentResolver.query(uri,arrayOf("_id"),null,null,null)?.use {assertEquals(0,it.count)}
        val corrupt=metadata().copy(hash="0".repeat(64));assertEquals("hash_mismatch",send(corrupt).body["code"].asString)
        assertNull(app.queue.receivedFile(a,corrupt.id))
    }
    @Test(timeout=45000) fun retainedReplayAndConflictPrecedeDisabledPolicy()=runBlocking<Unit> {
        val meta=metadata(kind="photo");assertEquals(Type.ACK,send(meta).type)
        app.preferences.setPairSwitch(a,"automaticMedia",false)
        // This switch controls phone -> computer, and must not disable incoming photos.
        assertEquals(Type.ACK,send(meta.copy(id=randomId())).type)
        app.preferences.toggle("paused",true)
        val replay=send(meta);assertEquals(Type.ACK,replay.type);assertTrue(replay.body["deduplicated"].asBoolean)
        val conflict=send(meta.copy(name="changed.png"));assertEquals("id_conflict",conflict.body["code"].asString)
        val refused=send(meta.copy(id=randomId()));assertEquals("unsupported_kind",refused.body["code"].asString)
        val ignored=send(meta.copy(id=randomId()),mesh=false);assertEquals(Type.ACK,ignored.type)
        assertEquals(2,app.receivedRecent.first().size)
    }
    @Test(timeout=45000) fun publishReceiptFailureRecoversTheSameMediaStoreUri()=runBlocking<Unit> {
        val meta=metadata();val db=app.database.openHelper.writableDatabase
        db.execSQL("CREATE TRIGGER fail_received_commit BEFORE UPDATE ON received_files WHEN NEW.state='complete' BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END")
        try {
            assertEquals("storage_failed",send(meta).body["code"].asString)
            val row=app.queue.receivedFile(a,meta.id);owned+=Uri.parse(row.uri);assertEquals("verified",row.state)
            c.contentResolver.query(Uri.parse(row.uri),arrayOf("is_pending"),null,null,null)!!.use {assertTrue(it.moveToFirst());assertEquals(0,it.getInt(0))}
        }finally {db.execSQL("DROP TRIGGER fail_received_commit")}
        val original=app.queue.receivedFile(a,meta.id).uri
        val replay=send(meta);assertEquals(Type.ACK,replay.type);assertTrue(replay.body["deduplicated"].asBoolean)
        assertEquals(original,app.queue.receivedFile(a,meta.id).uri)
    }
    @Test(timeout=45000) fun independentComputersRetainIndependentSlotsAndReceipts()=runBlocking<Unit> {
        bind(b);val meta=metadata();val first=receiver(a);val second=receiver(b)
        first.begin(com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject)
        second.begin(com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject)
        first.chunk(Frame.Chunk(meta.id,0,bytes.copyOf(2)));second.chunk(Frame.Chunk(meta.id,0,bytes))
        assertEquals(Type.ACK,second.end(meta.id).type)
        first.chunk(Frame.Chunk(meta.id,2,bytes.copyOfRange(2,bytes.size)));assertEquals(Type.ACK,first.end(meta.id).type)
        assertNotEquals(app.queue.receivedFile(a,meta.id).uri,app.queue.receivedFile(b,meta.id).uri)
    }
    @Test(timeout=45000) fun receivedMediaIsNeverPickedUpByAutomaticScanning()=runBlocking<Unit> {
        app.preferences.toggle("queue",true)
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,true)
        val observer=ScreenshotObserver(app)
        // Establish each collection baseline before publishing new received rows.
        repeat(4) {observer.scanOnce()}
        for(mime in listOf("image/png","video/mp4","application/pdf"))assertEquals(Type.ACK,send(metadata(mime=mime)).type)
        repeat(4) {observer.scanOnce()}
        assertTrue("Pneu receive folders must not enter the automatic outbox",app.queue.all().isEmpty())
        assertTrue(app.queue.pendingAdmissions().isEmpty())
    }
    @Test(timeout=45000) fun clipboardImageAppliesTheVerifiedUriWithMimeAndOriginAndNeverReplaysItsEffect()=runBlocking<Unit> {
        app.preferences.toggle("setup",true)
        val intent=android.content.Intent(c,MainActivity::class.java).putExtra("home",true)
        androidx.test.core.app.ActivityScenario.launch<MainActivity>(intent).use {scenario->
            val clipboard=c.getSystemService(android.content.ClipboardManager::class.java)
            try {
                val meta=metadata(kind="clipboard_image",stamp=System.currentTimeMillis())
                assertEquals(Type.ACK,send(meta).type)
                val uri=Uri.parse(app.queue.receivedFile(a,meta.id).uri)
                val clip=withTimeout(5000) {
                    while(true) {
                        val current=withContext(Dispatchers.Main) {clipboard.primaryClip}
                        if(current?.getItemAt(0)?.uri==uri)return@withTimeout current
                        delay(25)
                    }
                    error("unreachable")
                }
                assertTrue(clip.description.hasMimeType("image/png"))
                assertTrue(clip.description.extras!!.getBoolean(ClipboardDelivery.OWN_CLIP))
                assertEquals(a,clip.description.extras!!.getString(ClipboardDelivery.OWN_SOURCE))
                app.preferences.originateClipboard()
                scenario.onActivity {clipboard.setPrimaryClip(android.content.ClipData.newPlainText("local","Newer local clipboard"))}
                app.preferences.setPairSwitch(a,"receiveClipboard",false)
                val replay=send(meta);assertEquals(Type.ACK,replay.type);assertTrue(replay.body["deduplicated"].asBoolean)
                assertEquals("Newer local clipboard",withContext(Dispatchers.Main) {clipboard.primaryClip!!.getItemAt(0).text.toString()})
            }finally {scenario.onActivity {clipboard.clearPrimaryClip()}}
        }
    }
    @Test(timeout=45000) fun queuedClipboardImageRechecksTheEffectiveDeviceWideRegister()=runBlocking<Unit> {
        val order=System.currentTimeMillis()-1000
        val meta=metadata(kind="clipboard_image",stamp=order)
        // An ordinary file receipt allows exercising the deferred action without
        // the async worker: no actual clipboard effect is needed in this test.
        val saved=meta.copy(kind="file");assertEquals(Type.ACK,send(saved).type)
        val row=app.queue.receivedFile(a,saved.id)
        val effects=ReceivedFileEffects(app);var applications=0
        app.preferences.recordClipboardEffect(ClipboardStamp((order+1).toBigInteger(),b),"newer",b,1,"unfinished")
        effects.applyClipboard(row,meta,true,CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),a)))) {applications++}
        assertEquals(0,applications)
        val latest=meta.copy(hlc=(order+2).toString())
        effects.applyClipboard(row,latest,true,CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),a)))) {applications++}
        assertEquals(1,applications)
        effects.applyClipboard(row,latest,true,CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),a)))) {applications++}
        assertEquals("An equal stamp must not apply again",1,applications)
    }
}
