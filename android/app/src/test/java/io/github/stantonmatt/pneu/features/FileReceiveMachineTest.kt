package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayOutputStream

class FileReceiveMachineTest {
    private val bytes="A streamed file".toByteArray()
    private fun metadata(id:String="a".repeat(32),data:ByteArray=bytes)=ReceivedMetadata(id,"file.pdf","application/pdf",data.size.toLong(),sha256(data),"file",10)
    private class Sink:ReceiveSink {
        val bytes=ByteArrayOutputStream();var sealed=false;var commits=0;var aborted=false;var failure:String?=null
        override fun write(bytes:ByteArray) {failure?.let {throw ReceiveStorageException(it)};this.bytes.write(bytes)}
        override fun seal() {sealed=true}
        override suspend fun commit():ReceiveReply {check(sealed);commits++;return ReceiveReply()}
        override suspend fun abort() {aborted=true}
    }
    private fun machine(sink:Sink)=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=sink)}) {if(it is ReceiveStorageException)it.code else "storage_failed"}
    private fun body(m:ReceivedMetadata)=com.google.gson.JsonParser.parseString(m.canonical).asJsonObject
    @Test fun validStreamingAndEmptyFiles()=runBlocking {
        for(data in listOf(bytes,byteArrayOf())) {
            val sink=Sink();val machine=machine(sink);val m=metadata(data=data)
            machine.begin(body(m))
            data.forEachIndexed {offset,byte->machine.chunk(Frame.Chunk(m.id,offset.toLong(),byteArrayOf(byte)))}
            val ack=machine.end(m.id)
            assertEquals(Type.ACK,ack.type);assertFalse(ack.body["deduplicated"].asBoolean)
            assertArrayEquals(data,sink.bytes.toByteArray());assertEquals(1,sink.commits);assertFalse(machine.active)
        }
    }
    @Test fun offsetWrongIdAndChunkBeyondDeclaredSizeAreFatal()=runBlocking {
        for(chunk in listOf(Frame.Chunk("a".repeat(32),1,bytes),Frame.Chunk("b".repeat(32),0,bytes),Frame.Chunk("a".repeat(32),0,bytes+1))) {
            val sink=Sink();val machine=machine(sink);machine.begin(body(metadata()))
            try {machine.chunk(chunk);fail("Invalid chunk accepted")}catch(e:ReceiveViolation) {assertEquals("unexpected_message",e.code)}
            machine.disconnect();assertTrue(sink.aborted);assertEquals(0,sink.commits)
        }
    }
    @Test fun wireOversizeAndSecondBeginCannotAcquireAnotherSlot()=runBlocking {
        val sink=Sink();val machine=machine(sink)
        try {machine.begin(body(metadata().copy(size=MAX_FILE+1)));fail("Oversize accepted")}catch(e:ReceiveViolation) {assertEquals("malformed_message",e.code)}
        machine.begin(body(metadata()))
        try {machine.begin(body(metadata("b".repeat(32))));fail("Second BEGIN accepted")}catch(_:ReceiveViolation) {}
        machine.disconnect();assertTrue(sink.aborted)
    }
    @Test fun hashAndShortSizeMismatchNeverCommit()=runBlocking {
        for(short in listOf(false,true)) {
            val sink=Sink();val machine=machine(sink);val m=metadata().let {if(short)it else it.copy(hash="0".repeat(64))}
            machine.begin(body(m));machine.chunk(Frame.Chunk(m.id,0,if(short)bytes.copyOf(2) else bytes))
            val result=machine.end(m.id)
            assertEquals(if(short)"size_mismatch" else "hash_mismatch",result.body["code"].asString)
            assertTrue(result.body["retryable"].asBoolean);assertEquals(0,sink.commits);assertTrue(sink.aborted)
        }
    }
    @Test fun disconnectDeletesPartialAndNextConnectionRestartsAtZero()=runBlocking {
        val sink=Sink();val machine=machine(sink);machine.begin(body(metadata()));machine.chunk(Frame.Chunk(metadata().id,0,bytes.copyOf(2)))
        machine.disconnect();assertTrue(sink.aborted);assertFalse(machine.active);assertEquals(0,sink.commits)
    }
    @Test fun twoComputersMayTransferTheSameIdConcurrently()=runBlocking {
        val a=Sink();val b=Sink();val first=machine(a);val second=machine(b);val m=metadata()
        first.begin(body(m));second.begin(body(m));first.chunk(Frame.Chunk(m.id,0,bytes.copyOf(2)))
        second.chunk(Frame.Chunk(m.id,0,bytes));assertEquals(Type.ACK,second.end(m.id).type)
        first.chunk(Frame.Chunk(m.id,2,bytes.copyOfRange(2,bytes.size)));assertEquals(Type.ACK,first.end(m.id).type)
        assertArrayEquals(a.bytes.toByteArray(),b.bytes.toByteArray())
    }
    @Test fun diskFailureDrainsAndRepliesOnlyAtEnd()=runBlocking {
        val sink=Sink().apply {failure="disk_full"};val machine=machine(sink);val m=metadata()
        machine.begin(body(m));machine.chunk(Frame.Chunk(m.id,0,bytes.copyOf(2)));assertTrue(machine.active)
        machine.chunk(Frame.Chunk(m.id,2,bytes.copyOfRange(2,bytes.size)))
        assertEquals("disk_full",machine.end(m.id).body["code"].asString);assertEquals(0,sink.commits)
    }
    @Test fun replayAndConflictsStillValidateDrainOffsetsWithoutEffects()=runBlocking {
        for(reply in listOf(ReceiveReply(deduplicated=true),ReceiveReply("id_conflict"),ReceiveReply("unsupported_kind"))) {
            val machine=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(reply=reply)})
            val m=metadata();machine.begin(body(m));machine.chunk(Frame.Chunk(m.id,0,bytes))
            assertEquals(reply.frame(m.id),machine.end(m.id))
        }
    }
    @Test fun slowStorageBackpressuresTheNextChunk()=runBlocking {
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        val sink=object:ReceiveSink {
            override fun write(bytes:ByteArray) {}
            override fun seal() {}
            override suspend fun commit():ReceiveReply {entered.complete(Unit);release.await();return ReceiveReply()}
            override suspend fun abort() {}
        }
        val machine=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=sink)});val m=metadata()
        machine.begin(body(m));machine.chunk(Frame.Chunk(m.id,0,bytes))
        val ending=async {machine.end(m.id)};entered.await();assertFalse(ending.isCompleted)
        release.complete(Unit);assertEquals(Type.ACK,ending.await().type)
    }
}
