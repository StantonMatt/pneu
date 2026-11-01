package io.github.stantonmatt.pneu

import android.app.Application
import android.content.*
import android.os.PersistableBundle
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.datastore.preferences.core.edit
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.filterNotNull
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class ClipboardHistoryIntegrationDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val pc="b".repeat(32)
    private var activity:ActivityScenario<MainActivity>?=null
    private var peer:LocalPeerFixture?=null
    private val owned=mutableSetOf<android.net.Uri>()
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset();app.savedClips.setEnabled(false);app.savedClips.setEnabled(true)
        app.preferences.store.edit {p->PeerRegistry.write(p,listOf(Peer("Studio PC","a".repeat(64),pc,listOf(Endpoint("127.0.0.1",1)))))}
        app.preferences.toggle("setup",true)
        withTimeout(5000) {app.prefs.first {it.setup && it.devices.isNotEmpty()}}
        activity=ActivityScenario.launch(Intent(c,MainActivity::class.java).putExtra("home",true))
    }
    @After fun cleanup()=runBlocking<Unit> {activity?.close();peer?.close();owned.forEach {c.contentResolver.delete(it,null,null)};app.savedClips.setEnabled(false);java.io.File(c.filesDir,"clipboard-snapshot").deleteRecursively();DeviceFixture.reset()}
    @Test fun receivedTextRecordsSenderAndSensitiveTextAppliesFlagWithoutSaving()=runBlocking<Unit> {
        val delivery=ClipboardDelivery(app)
        fun message(text:String,sensitive:Boolean,time:Long)=json("transfer_id" to randomId(),"text" to text,"created_at" to time).apply {if(sensitive)addProperty("sensitive",true)}
        val normal=message("received normal",false,10)
        assertEquals(Type.ACK,delivery.receive(pc,normal).type)
        assertEquals("Studio PC",app.savedClips.state.value.items.single().sender)
        val secret=message("received secret",true,20)
        assertEquals(Type.ACK,delivery.receive(pc,secret).type)
        val clipboard=c.getSystemService(ClipboardManager::class.java)
        val clip=withContext(Dispatchers.Main) {clipboard.primaryClip!!}
        assertEquals("received secret",clip.getItemAt(0).text.toString());assertTrue(clip.description.extras!!.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE))
        assertEquals(listOf("received normal"),app.savedClips.state.value.items.map {it.text})
        assertEquals(Type.ACK,delivery.receive(pc,secret).type);assertEquals(1,app.savedClips.state.value.items.size)
        val conflicting=secret.deepCopy().apply {remove("sensitive")}
        assertEquals("id_conflict",delivery.receive(pc,conflicting).body["code"].asString)
    }
    @Test fun sendClipboardAndSharedTextUseTheWireFlagAndOnlyNonSensitiveAcksEnterHistory()=runBlocking<Unit> {
        val received=LinkedBlockingQueue<com.google.gson.JsonObject>()
        val fixture=LocalPeerFixture(app,handle={frame,send->
            if(frame.type==Type.CLIPBOARD) {received.offer(frame.body);send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)));true}else false
        });peer=fixture;fixture.connect(holdFirstFrame=false)
        val clipboard=c.getSystemService(ClipboardManager::class.java)
        suspend fun send(text:String,sensitive:Boolean) {
            val clip=ClipData.newPlainText("fixture",text).apply {if(sensitive)description.extras=PersistableBundle().apply {putBoolean(ClipDescription.EXTRA_IS_SENSITIVE,true)}}
            withContext(Dispatchers.Main) {clipboard.setPrimaryClip(clip)}
            val operation=ManualEntryViewModel(c.applicationContext as Application)
            operation.captureClipboardAdmission(app.captureAdmission());operation.clipboard(clip)
            withTimeout(5000) {operation.result.filterNotNull().first()}
            val wire=withContext(Dispatchers.IO) {received.poll(5,TimeUnit.SECONDS)}
            assertNotNull("Text should reach the loopback peer",wire);assertEquals(sensitive,wire!!["sensitive"]?.asBoolean==true);assertEquals(sensitive,wire.has("sensitive"))
            withTimeout(5000) {while(app.queue.find(operation.completed.single())?.state!="sent")delay(20)}
        }
        send("normal from phone",false);assertEquals("",app.savedClips.state.value.items.single().sender)
        send("secret from phone",true);assertEquals(listOf("normal from phone"),app.savedClips.state.value.items.map {it.text})
        val row=ManualEntries.text(app,"shared text","share")
        withTimeout(5000) {while(app.queue.find(row.mediaId)?.state!="sent")delay(20)}
        assertEquals("shared text",app.savedClips.state.value.items.first().text)
        received.poll(5,TimeUnit.SECONDS)
        val sharedSecret=ManualEntryViewModel(c.applicationContext as Application)
        val admission=app.captureAdmission()
        sharedSecret.share(ShareRequest(app.scope.async {admission.await().binding},emptyList(),listOf("secret shared text"),admission,sensitive=true))
        withTimeout(5000) {sharedSecret.result.filterNotNull().first()}
        val wire=received.poll(5,TimeUnit.SECONDS);assertNotNull(wire);assertTrue(wire!!["sensitive"].asBoolean)
        withTimeout(5000) {while(app.queue.find(sharedSecret.completed.single())?.state!="sent")delay(20)}
        assertEquals(listOf("shared text","normal from phone"),app.savedClips.state.value.items.map {it.text})
    }
    @Test fun sensitiveClipboardImageIsAppliedWithAndroidFlagAndNeverSaved()=runBlocking<Unit> {
        val bitmap=android.graphics.Bitmap.createBitmap(12,12,android.graphics.Bitmap.Config.ARGB_8888)
        val bytes=java.io.ByteArrayOutputStream().also {bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG,100,it);bitmap.recycle()}.toByteArray()
        val binding=requireNotNull(DeliveryBinding.from(app.preferences.read(),pc))
        suspend fun receive(sensitive:Boolean):Pair<ReceivedMetadata,ClipData> {
            val stamp=app.preferences.originateClipboard(apply=false)
            val meta=ReceivedMetadata(randomId(),"history-fixture-${randomId()}.png","image/png",bytes.size.toLong(),sha256(bytes),"clipboard_image",System.currentTimeMillis(),hlc=(stamp.hlc+java.math.BigInteger.ONE).toString(),source=pc,sensitive=sensitive)
            val receiver=app.receivedFiles.connection(binding,true) {true}
            receiver.begin(com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject);receiver.chunk(Frame.Chunk(meta.id,0,bytes));assertEquals(Type.ACK,receiver.end(meta.id).type)
            val row=app.queue.receivedFile(pc,meta.id);owned+=android.net.Uri.parse(row.uri)
            val clipboard=c.getSystemService(ClipboardManager::class.java)
            val clip=withTimeout(5000) {
                while(true) {
                    val current=withContext(Dispatchers.Main) {clipboard.primaryClip}
                    if(current?.getItemAt(0)?.uri?.toString()==row.uri)return@withTimeout current
                    delay(20)
                }
                error("unreachable")
            }
            return meta to clip
        }
        val (_,normal)=receive(false);assertFalse(normal.description.extras!!.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE))
        withTimeout(5000) {while(app.savedClips.state.value.items.isEmpty())delay(20)}
        assertEquals("Studio PC",app.savedClips.state.value.items.single().sender)
        app.savedClips.clear()
        val (secret,clip)=receive(true);assertTrue(clip.description.extras!!.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE))
        // The completion marker follows the history decision in this effect worker.
        withTimeout(5000) {while(app.preferences.clipboardWins(ClipboardStamp(secret.hlc!!.toBigInteger(),pc)))delay(20)}
        assertTrue(app.savedClips.state.value.items.isEmpty())
    }
    @Test fun copiedHistoryImageCanBeReadAndExplicitlySentThroughThePrivateProvider()=runBlocking<Unit> {
        val began=LinkedBlockingQueue<com.google.gson.JsonObject>()
        // FILE chunks are consumed by the fixture; ACK the matching end.
        val fixture=LocalPeerFixture(app,handle={frame,send->when(frame.type) {
            Type.FILE_BEGIN->{began.offer(frame.body);true}
            Type.FILE_END->{send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)));true}
            else->false
        }});peer=fixture;fixture.connect(holdFirstFrame=false)
        val bitmap=android.graphics.Bitmap.createBitmap(12,12,android.graphics.Bitmap.Config.ARGB_8888)
        val bytes=java.io.ByteArrayOutputStream().also {bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG,100,it);bitmap.recycle()}.toByteArray()
        app.savedClips.recordImage("image/png","Another PC",source="other-pc") {bytes.inputStream()}
        var copied:ClipData?=null
        assertTrue(app.savedClips.copy(app.savedClips.state.value.items.single().id) {copied=it;withContext(Dispatchers.Main) {c.getSystemService(ClipboardManager::class.java).setPrimaryClip(it)}})
        val clip=requireNotNull(copied);val uri=requireNotNull(clip.getItemAt(0).uri)
        assertArrayEquals(bytes,c.contentResolver.openInputStream(uri)!!.use {it.readBytes()});assertEquals("image/png",c.contentResolver.getType(uri))
        val operation=ManualEntryViewModel(c.applicationContext as Application)
        operation.captureClipboardAdmission(app.captureAdmission());operation.clipboard(clip)
        withTimeout(5000) {operation.result.filterNotNull().first()}
        val wire=began.poll(10,TimeUnit.SECONDS);assertNotNull(wire);assertEquals("clipboard_image",wire!!["kind"].asString);assertFalse(wire.has("sensitive"))
        withTimeout(10000) {while(app.savedClips.state.value.items.first().sender.isNotEmpty())delay(20)}
        assertEquals(1,app.savedClips.state.value.items.size)
        val before=app.savedClips.state.value.items.single().copiedAt
        clip.description.extras=PersistableBundle().apply {putBoolean(ClipDescription.EXTRA_IS_SENSITIVE,true)}
        val secret=ManualEntryViewModel(c.applicationContext as Application)
        secret.captureClipboardAdmission(app.captureAdmission());secret.clipboard(clip)
        withTimeout(5000) {secret.result.filterNotNull().first()}
        val secretWire=began.poll(10,TimeUnit.SECONDS);assertNotNull(secretWire);assertTrue(secretWire!!["sensitive"].asBoolean)
        withTimeout(10000) {while(app.queue.find(secret.completed.single())?.state!="sent")delay(20)}
        assertEquals(before,app.savedClips.state.value.items.single().copiedAt)
        app.savedClips.delete(app.savedClips.state.value.items.single().id)
        assertEquals(uri,withContext(Dispatchers.Main) {c.getSystemService(ClipboardManager::class.java).primaryClip!!.getItemAt(0).uri})
        assertArrayEquals(bytes,c.contentResolver.openInputStream(uri)!!.use {it.readBytes()})
        for(cleanup in listOf<suspend ()->Unit>(
            {app.savedClips.clear()},
            {app.savedClips.setEnabled(false)},
            {repeat(SavedClips.CAPACITY) {app.savedClips.recordText("eviction $it")}}
        )) {
            app.savedClips.setEnabled(true)
            app.savedClips.recordImage("image/png","Another PC",source="other-pc") {bytes.inputStream()}
            lateinit var selected:ClipData
            assertTrue(app.savedClips.copy(app.savedClips.state.value.items.first().id) {
                selected=it;withContext(Dispatchers.Main) {c.getSystemService(ClipboardManager::class.java).setPrimaryClip(it)}
            })
            val extras=selected.description.extras!!
            assertTrue(extras.getBoolean(ClipboardDelivery.OWN_CLIP));assertEquals("other-pc",extras.getString(ClipboardDelivery.OWN_SOURCE));assertEquals(sha256(bytes),extras.getString(ClipboardDelivery.OWN_HASH))
            assertFalse(extras.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE))
            cleanup()
            val onClipboard=withContext(Dispatchers.Main) {c.getSystemService(ClipboardManager::class.java).primaryClip!!.getItemAt(0).uri!!}
            assertEquals(selected.getItemAt(0).uri,onClipboard)
            assertArrayEquals(bytes,c.contentResolver.openInputStream(onClipboard)!!.use {it.readBytes()})
            assertTrue(app.savedClips.ownsImage(onClipboard))
        }
    }
    @Test fun compressedLargeImageRecordsOnlyASampledThumbnail()=runBlocking<Unit> {
        // Build a real 96-megapixel PNG a row at a time: no large fixture bitmap.
        val compressed=java.io.ByteArrayOutputStream()
        java.util.zip.DeflaterOutputStream(compressed).use {output->
            val row=ByteArray(1+12000*4)
            repeat(8000) {output.write(row)}
        }
        val png=java.io.ByteArrayOutputStream()
        java.io.DataOutputStream(png).use {output->
            output.write(byteArrayOf(137.toByte(),80,78,71,13,10,26,10))
            fun chunk(type:String,data:ByteArray) {
                val tag=type.toByteArray(Charsets.US_ASCII)
                output.writeInt(data.size);output.write(tag);output.write(data)
                val crc=java.util.zip.CRC32().apply {update(tag);update(data)}
                output.writeInt(crc.value.toInt())
            }
            val header=java.nio.ByteBuffer.allocate(13).putInt(12000).putInt(8000).put(8.toByte()).put(6.toByte()).put(0.toByte()).put(0.toByte()).put(0.toByte()).array()
            chunk("IHDR",header);chunk("IDAT",compressed.toByteArray());chunk("IEND",byteArrayOf())
        }
        val bytes=png.toByteArray();assertTrue(bytes.size<SavedClips.MAX_IMAGE)
        app.savedClips.recordImage("image/png") {bytes.inputStream()}
        val item=app.savedClips.state.value.items.single()
        assertEquals("image:${sha256(bytes)}",item.id)
        val thumb=android.graphics.BitmapFactory.decodeFile(app.savedClips.thumbnail(item).path)!!
        try {assertTrue(thumb.width in 1..128);assertTrue(thumb.height in 1..128)}finally {thumb.recycle()}
        app.savedClips.recordImage("image/png") {bytes.inputStream()}
        assertEquals(1,app.savedClips.state.value.items.size)
    }
}
