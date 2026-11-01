package io.github.stantonmatt.pneu

import android.Manifest
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.google.gson.JsonParser
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

/** Real TLS reconnect + Room checkpoint + private stage + MediaStore publication.
 * Run only on the task-owned disposable AVD. */
@RunWith(AndroidJUnit4::class)
class ResumeTransferDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val app=i.targetContext.graph
    private val peerId="a".repeat(32)
    private var peer:LocalPeerFixture?=null
    private val published=mutableListOf<Uri>()
    @Before fun setup()=runBlocking {DeviceFixture.reset(preserveIdentity=false)}
    @After fun cleanup()=runBlocking {
        app.connection.stop();app.connection.awaitStopped();peer?.close()
        published.forEach {i.targetContext.contentResolver.delete(it,null,null)}
        DeviceFixture.reset(preserveIdentity=false)
    }
    @Test(timeout=120000) fun interruptReceiveResumeExactBytesAndReplayWithoutChunks()=runBlocking<Unit>(Dispatchers.IO) {
        i.uiAutomation.grantRuntimePermission(i.targetContext.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val controls=Channel<Frame.Control>(Channel.UNLIMITED)
        val fixture=LocalPeerFixture(app,setOf("files",CAP_FILE_RESUME),handle={frame,_->
            if(frame.type in setOf(Type.FILE_READY,Type.ACK,Type.NACK,Type.PONG)) {controls.trySend(frame);true} else false
        });peer=fixture
        val invitation=fixture.invitation(peerId,"Resume PC")
        assertTrue(app.requestPairing(invitation,ConfirmedBinding.from(app.preferences.read())))
        app.connection.start(app.scope)
        suspend fun connected()=withTimeout(15000) {while(!app.connection.connected(peerId))delay(20)}
        connected()
        suspend fun next(type:Type):Frame.Control=withTimeout(15000) {
            while(true) {val frame=controls.receive();assertFalse("Unexpected NACK: $frame",frame.type==Type.NACK);if(frame.type==type)return@withTimeout frame}
            error("unreachable")
        }
        val bytes=ByteArray(5*1024*1024+73) {(it*31).toByte()}
        val meta=ReceivedMetadata("00112233445566778899aabbccddeeff","resume-device.bin","application/octet-stream",bytes.size.toLong(),sha256(bytes),"file",System.currentTimeMillis())
        fun begin()=Frame.Control(Type.FILE_BEGIN,JsonParser.parseString(meta.canonical).asJsonObject.apply {addProperty("resume",true)})
        fixture.sendFrame(begin());assertEquals(0L,next(Type.FILE_READY).body["offset"].asLong)
        var offset=0
        suspend fun sendThrough(limit:Int) {
            while(offset<limit) {
                val end=minOf(offset+CHUNK_SIZE,limit)
                fixture.sendFrame(Frame.Chunk(meta.id,offset.toLong(),bytes.copyOfRange(offset,end)));offset=end
                // This test exercises durable resume; saturation has its own held-worker test.
                withTimeout(10000) {while(app.connection.devices.value[peerId]?.incoming?.received!=offset.toLong())delay(10)}
            }
        }
        sendThrough(4*1024*1024)
        withTimeout(10000) {while(app.queue.receivedFile(peerId,meta.id)?.checkpoint!=offset.toLong())delay(20)}
        assertNull(app.queue.receivedFile(peerId,meta.id).uri.takeIf {it.isNotEmpty()})
        app.connection.stop();app.connection.awaitStopped()
        // Simulate a crash tail and startup with a fresh catalogue owner.
        val checkpoint=app.queue.receivedFile(peerId,meta.id)
        val stage=java.io.File(checkpoint.stagePath);stage.appendBytes("uncheckpointed tail".toByteArray())
        ReceivedFiles(app).recover()
        assertEquals(offset.toLong(),stage.length())
        app.connection.start(app.scope)
        withTimeout(15000) {while(fixture.hellos.get()<2 || !app.connection.connected(peerId))delay(20)}
        fixture.sendFrame(begin())
        val retained=next(Type.FILE_READY).body["offset"].asLong
        assertEquals(offset.toLong(),retained)
        // Keepalive is served independently of the file lane.
        fixture.sendControl(Frame.Control(Type.PING,json("nonce" to "b".repeat(16))))
        next(Type.PONG)
        sendThrough(bytes.size)
        fixture.sendControl(Frame.Control(Type.FILE_END,json("transfer_id" to meta.id)))
        assertFalse(next(Type.ACK).body["deduplicated"].asBoolean)
        val row=app.queue.receivedFile(peerId,meta.id);val uri=Uri.parse(row.uri);published+=uri
        assertEquals("complete",row.state)
        assertArrayEquals(bytes,i.targetContext.contentResolver.openInputStream(uri)!!.use {it.readBytes()})
        fixture.sendFrame(begin());assertEquals(bytes.size.toLong(),next(Type.FILE_READY).body["offset"].asLong)
        fixture.sendControl(Frame.Control(Type.FILE_END,json("transfer_id" to meta.id)))
        assertTrue(next(Type.ACK).body["deduplicated"].asBoolean)
        assertEquals(row.uri,app.queue.receivedFile(peerId,meta.id).uri)
        // CANCEL an inactive partial after another receive interruption.
        val partial=meta.copy(id="f".repeat(32))
        fixture.sendFrame(Frame.Control(Type.FILE_BEGIN,JsonParser.parseString(partial.canonical).asJsonObject.apply {addProperty("resume",true)}));next(Type.FILE_READY)
        fixture.sendFrame(Frame.Chunk(partial.id,0,bytes.copyOfRange(0,CHUNK_SIZE)))
        fixture.sendControl(Frame.Control(Type.CANCEL,json("transfer_id" to partial.id)))
        withTimeout(10000) {while(app.queue.receivedFile(peerId,partial.id)!=null || java.io.File(i.targetContext.filesDir,"receive-partials").listFiles().orEmpty().isNotEmpty())delay(20)}
        assertNull(app.queue.receipt(peerId,partial.id))
        assertTrue(java.io.File(i.targetContext.filesDir,"receive-partials").listFiles().orEmpty().isEmpty())
        controls.close()
    }
    @Test(timeout=90000) fun senderUsesReadyOffsetAndCancelKeepsTheConnectionAlive()=runBlocking<Unit>(Dispatchers.IO) {
        i.uiAutomation.grantRuntimePermission(i.targetContext.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val begins=Channel<Frame.Control>(Channel.UNLIMITED);val chunks=Channel<Frame.Chunk>(Channel.UNLIMITED)
        val cancels=Channel<Frame.Control>(Channel.UNLIMITED);val pongs=Channel<Frame.Control>(Channel.UNLIMITED)
        val first="1".repeat(32);val second="2".repeat(32)
        val fixture=LocalPeerFixture(app,setOf("files",CAP_FILE_RESUME),handle={frame,send->
            when(frame.type) {
                Type.FILE_BEGIN->{
                    begins.trySend(frame)
                    if(frame.body["transfer_id"].asString==first)send(Frame.Control(Type.FILE_READY,json("transfer_id" to first,"offset" to 2)))
                    true
                }
                Type.FILE_END->{send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)));true}
                Type.CANCEL->{
                    cancels.trySend(frame)
                    // READY already in flight when the user cancels is consumed.
                    send(Frame.Control(Type.FILE_READY,json("transfer_id" to second,"offset" to 0)));true
                }
                Type.PONG->{pongs.trySend(frame);true}
                else->false
            }
        });peer=fixture
        fixture.observeFrame={if(it is Frame.Chunk)chunks.trySend(it)}
        assertTrue(app.requestPairing(fixture.invitation(peerId,"Resume PC"),ConfirmedBinding.from(app.preferences.read())))
        app.connection.start(app.scope)
        withTimeout(15000) {while(!app.connection.connected(peerId))delay(20)}
        suspend fun enqueue(id:String):Outgoing {
            val file=java.io.File(i.targetContext.filesDir,"outgoing/$id.bin").apply {parentFile!!.mkdirs();writeBytes("abc".toByteArray())}
            val row=Outgoing().apply {mediaId="manual:$id";transferId=id;peerId=this@ResumeTransferDeviceTest.peerId
                bindingGeneration=app.preferences.read().devices.single().generation;path=file.path;kind="file";automatic=false;size=3
                fileName="sender.txt";mime="text/plain";hash=sha256("abc".toByteArray());createdAt=System.currentTimeMillis()}
            DeviceFixture.insert(app,row);app.connection.kick();return row
        }
        val initial=enqueue(first)
        assertTrue(withTimeout(10000) {begins.receive()}.body["resume"].asBoolean)
        val suffix=withTimeout(10000) {chunks.receive()};assertEquals(2L,suffix.offset);assertArrayEquals("c".toByteArray(),suffix.data)
        withTimeout(10000) {while(app.queue.delivery(initial.mediaId,peerId).state!="sent")delay(20)}
        val flushed=CompletableDeferred<Unit>()
        val release=java.util.concurrent.CountDownLatch(1)
        app.connection.frameFlushedHookForTest {id->if(id==second) {
            flushed.complete(Unit)
            check(release.await(15,java.util.concurrent.TimeUnit.SECONDS))
        }}
        val pending=enqueue(second)
        assertEquals(second,withTimeout(10000) {begins.receive()}.body["transfer_id"].asString)
        // Cancel while the IO writer is held immediately after flush, before its
        // sender dispatcher can resume and release the writer lock.
        withTimeout(5000) {flushed.await()}
        val hellosBeforeCancel=fixture.hellos.get()
        try {
            val current=app.queue.delivery(pending.mediaId,peerId)
            assertTrue(app.discard(ConfirmedQueue.from(app.preferences.read(),listOf(current))))
        }finally {release.countDown();app.connection.frameFlushedHookForTest {}}
        assertEquals(second,withTimeout(10000) {cancels.receive()}.body["transfer_id"].asString)
        fixture.sendControl(Frame.Control(Type.PING,json("nonce" to "a".repeat(16))))
        assertEquals("a".repeat(16),withTimeout(10000) {pongs.receive()}.body["nonce"].asString)
        assertEquals("discarded",app.queue.delivery(pending.mediaId,peerId).state)
        assertEquals("CANCEL must not redial after the completed BEGIN boundary",hellosBeforeCancel,fixture.hellos.get())
        withTimeout(5000) {while(app.queue.pendingCancels(peerId).isNotEmpty())delay(20)}
        begins.close();chunks.close();cancels.close();pongs.close()
    }

    @Test(timeout=90000) fun disconnectDuringReceiptWaitPersistsAllSentBytes()=runBlocking<Unit>(Dispatchers.IO) {
        i.uiAutomation.grantRuntimePermission(i.targetContext.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val ended=Channel<String>(Channel.UNLIMITED)
        val fixture=LocalPeerFixture(app,setOf("files",CAP_FILE_RESUME),handle={frame,send->
            when(frame.type) {
                Type.FILE_BEGIN->{send(Frame.Control(Type.FILE_READY,json("transfer_id" to frame.body["transfer_id"].asString,"offset" to 0)));true}
                // Deterministically hold the sender in its receipt wait.
                Type.FILE_END->{ended.trySend(frame.body["transfer_id"].asString);true}
                else->false
            }
        });peer=fixture
        assertTrue(app.requestPairing(fixture.invitation(peerId,"Receipt PC"),ConfirmedBinding.from(app.preferences.read())))
        for(queue in listOf(false,true)) {
            app.preferences.toggle("queue",queue)
            app.connection.start(app.scope)
            withTimeout(15000) {while(!app.connection.connected(peerId))delay(20)}
            val waiting=CompletableDeferred<Unit>()
            app.connection.fileReceiptWaitHookForTest {waiting.complete(Unit);awaitCancellation()}
            val id=if(queue)"6".repeat(32) else "5".repeat(32)
            val bytes="abc".toByteArray()
            val file=java.io.File(i.targetContext.filesDir,"outgoing/$id.bin").apply {parentFile!!.mkdirs();writeBytes(bytes)}
            val row=Outgoing().apply {mediaId="manual:$id";transferId=id;peerId=this@ResumeTransferDeviceTest.peerId
                bindingGeneration=app.preferences.read().devices.single().generation;path=file.path;kind="file";automatic=false;size=3
                fileName="receipt.txt";mime="text/plain";hash=sha256(bytes);createdAt=System.currentTimeMillis()}
            DeviceFixture.insert(app,row);app.connection.kick()
            assertEquals(id,withTimeout(10000) {ended.receive()})
            // Hold the real sender after END returned and ended became true.
            withTimeout(10000) {waiting.await()}
            app.connection.stop();app.connection.awaitStopped()
            val saved=app.queue.delivery(row.mediaId,peerId)
            assertEquals(if(queue)"waiting" else "failed",saved.state)
            assertEquals("connection_lost",saved.error)
            assertEquals("Receipt interruption must persist even sub-checkpoint files",3L,saved.sentBytes)
        }
        ended.close()
    }

    @Test(timeout=90000) fun saturatedReceiveLaneStillProcessesPingReceiptsAndCancel()=runBlocking<Unit>(Dispatchers.IO) {
        i.uiAutomation.grantRuntimePermission(i.targetContext.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val controls=Channel<Frame.Control>(Channel.UNLIMITED)
        val fixture=LocalPeerFixture(app,setOf("files",CAP_FILE_RESUME),handle={frame,send->
            when(frame.type) {
                Type.FILE_READY,Type.NACK,Type.ACK,Type.PONG->{controls.trySend(frame);true}
                Type.FILE_BEGIN->{send(Frame.Control(Type.FILE_READY,json("transfer_id" to frame.body["transfer_id"].asString,"offset" to 0)));true}
                Type.FILE_END->{send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)));true}
                else->false
            }
        });peer=fixture
        assertTrue(app.requestPairing(fixture.invitation(peerId,"Saturated PC"),ConfirmedBinding.from(app.preferences.read())))
        app.connection.start(app.scope)
        withTimeout(15000) {while(!app.connection.connected(peerId))delay(20)}
        val id="3".repeat(32)
        val meta=ReceivedMetadata(id,"saturated.bin","application/octet-stream",10L*CHUNK_SIZE,sha256(byteArrayOf()),"file",System.currentTimeMillis())
        fixture.sendFrame(Frame.Control(Type.FILE_BEGIN,JsonParser.parseString(meta.canonical).asJsonObject.apply {addProperty("resume",true)}))
        assertEquals(Type.FILE_READY,withTimeout(10000) {controls.receive()}.type)
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        app.connection.receiveChunkHookForTest {entered.complete(Unit);release.await()}
        try {
            fixture.sendFrame(Frame.Chunk(id,0,ByteArray(CHUNK_SIZE)))
            withTimeout(5000) {entered.await()}
            // The held chunk plus eight queued chunks saturate the worker.
            for(n in 1..9)fixture.sendFrame(Frame.Chunk(id,n.toLong()*CHUNK_SIZE,ByteArray(CHUNK_SIZE)))
            val hellos=fixture.hellos.get()
            fixture.sendControl(Frame.Control(Type.PING,json("nonce" to "c".repeat(16))))
            assertEquals(Type.PONG,withTimeout(5000) {controls.receive()}.type)
            // A simultaneous outgoing transfer must consume READY and ACK while
            // the receive worker remains held on its first chunk.
            val outgoingId="4".repeat(32)
            val file=java.io.File(i.targetContext.filesDir,"outgoing/$outgoingId.bin").apply {parentFile!!.mkdirs();writeBytes("abc".toByteArray())}
            val row=Outgoing().apply {mediaId="manual:$outgoingId";transferId=outgoingId;peerId=this@ResumeTransferDeviceTest.peerId
                bindingGeneration=app.preferences.read().devices.single().generation;path=file.path;kind="file";automatic=false;size=3
                fileName="control.txt";mime="text/plain";hash=sha256("abc".toByteArray());createdAt=System.currentTimeMillis()}
            DeviceFixture.insert(app,row);app.connection.kick()
            withTimeout(10000) {while(app.queue.delivery(row.mediaId,peerId).state!="sent")delay(20)}
            fixture.sendControl(Frame.Control(Type.CANCEL,json("transfer_id" to id)))
            fixture.sendControl(Frame.Control(Type.PING,json("nonce" to "d".repeat(16))))
            assertEquals("d".repeat(16),withTimeout(5000) {controls.receive()}.body["nonce"].asString)
            withTimeout(5000) {while(app.queue.receivedFile(peerId,id)!=null)delay(20)}
            assertEquals(hellos,fixture.hellos.get())
        }finally {release.complete(Unit);app.connection.receiveChunkHookForTest {}}
        controls.close()
    }
}
