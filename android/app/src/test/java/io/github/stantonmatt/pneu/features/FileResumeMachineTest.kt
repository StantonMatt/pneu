package io.github.stantonmatt.pneu.features

import com.google.gson.JsonParser
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import java.security.MessageDigest

class FileResumeMachineTest {
    private val id="00112233445566778899aabbccddeeff"
    private val meta=ReceivedMetadata(id,"a.txt","text/plain",3,"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","file",1800000000000)
    private fun begin(resume:Boolean?=true)=JsonParser.parseString(meta.canonical).asJsonObject.apply {resume?.let {addProperty("resume",it)}}
    private class Sink:ReceiveSink {
        val bytes=java.io.ByteArrayOutputStream();var committed=false;var aborted=false;var retained=false
        override fun write(bytes:ByteArray) {this.bytes.write(bytes)}
        override fun seal() {}
        override suspend fun commit():ReceiveReply {committed=true;return ReceiveReply()}
        override suspend fun abort() {aborted=true}
        override suspend fun disconnect() {retained=true}
    }
    @Test fun section81OffsetTwoSuffixAndWholeFileDigest()=runBlocking {
        val sink=Sink()
        val machine=FileReceiveMachine(ReceiveBackend {error("legacy")},resumeCapable=true,
            resumeBackend={_,_->ReceiveAdmission(sink=sink,offset=2,digest=MessageDigest.getInstance("SHA-256").apply {update("ab".toByteArray())})})
        val ready=machine.begin(begin())!!
        assertEquals(Type.FILE_READY,ready.type);assertEquals(2L,ready.body["offset"].asLong)
        machine.chunk(Frame.Chunk(id,2,byteArrayOf(99)))
        assertEquals(Type.ACK,machine.end(id).type)
        assertEquals("c",sink.bytes.toString());assertTrue(sink.committed)
    }
    @Test fun section81RetainedCompletionReadySizeEndAndNoDigestOrDestinationLookup()=runBlocking {
        val machine=FileReceiveMachine(ReceiveBackend {error("legacy")},resumeCapable=true,
            resumeBackend={_,_->ReceiveAdmission(reply=ReceiveReply(deduplicated=true))})
        assertEquals(3L,machine.begin(begin())!!.body["offset"].asLong)
        try {machine.chunk(Frame.Chunk(id,3,byteArrayOf(99)));fail()}catch(_:ReceiveViolation) {}
        val reply=machine.end(id);assertEquals(Type.ACK,reply.type);assertTrue(reply.body["deduplicated"].asBoolean)
    }
    @Test fun section81ZeroOffsetEmptyFileAndEarlyNacks()=runBlocking {
        val sink=Sink()
        val machine=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=sink)},resumeCapable=true)
        assertEquals(0L,machine.begin(begin())!!.body["offset"].asLong)
        machine.chunk(Frame.Chunk(id,0,"abc".toByteArray()));assertEquals(Type.ACK,machine.end(id).type)
        for(code in listOf("id_conflict","disk_full")) {
            val refusal=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(reply=ReceiveReply(code))},resumeCapable=true)
            assertEquals(code,refusal.begin(begin())!!.body["code"].asString);assertFalse(refusal.active)
        }
        val empty=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=Sink())},resumeCapable=true)
        val body=begin().apply {addProperty("size",0);addProperty("sha256",sha256(byteArrayOf()))}
        assertEquals(0L,empty.begin(body)!!.body["offset"].asLong);assertEquals(Type.ACK,empty.end(id).type)
    }
    @Test fun legacyAbsentAndFalseRemainByteIdenticalAndUnnegotiatedResumeIsFatal()=runBlocking {
        for(flag in listOf(null,false)) {
            val machine=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=Sink())},resumeCapable=true)
            assertNull(machine.begin(begin(flag)));machine.chunk(Frame.Chunk(id,0,"abc".toByteArray()))
            assertEquals(Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to false)),machine.end(id))
        }
        val legacy=FileReceiveMachine(ReceiveBackend {error("must not admit")})
        try {legacy.begin(begin());fail()}catch(e:ReceiveViolation) {assertEquals("unexpected_message",e.code)}
    }
    @Test fun cancelFencesPreparationAndInactivePartialWithoutAReceipt()=runBlocking {
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>();val sink=Sink();val purged=mutableListOf<String>()
        val machine=FileReceiveMachine(ReceiveBackend {error("legacy")},resumeCapable=true,
            resumeBackend={_,cancelled->entered.complete(Unit);release.await();assertTrue(cancelled());ReceiveAdmission(sink=sink)},cancelPartial={purged+=it})
        val admission=async {machine.begin(begin())};entered.await()
        machine.requestCancel(id);machine.purgeCancelled(id);assertEquals(listOf(id),purged)
        release.complete(Unit);assertNull(admission.await());assertTrue(sink.aborted);assertFalse(machine.active)
        machine.cancel("f".repeat(32));assertEquals(2,purged.size)
        // The cancelled admission stays fenced until its queued CANCEL retires it.
        try {machine.begin(begin());fail()}catch(_:ReceiveViolation) {}
        machine.cancel(id);assertEquals(0,machine.pendingCount)
    }
    @Test fun unknownCancelsRetainNothingAndQueuedCancellationStateIsBounded()=runBlocking {
        val machine=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=Sink())},resumeCapable=true)
        repeat(10000) {n->
            val unknown=n.toString(16).padStart(32,'0')
            assertFalse(machine.requestCancel(unknown));machine.requestFailure(unknown)
        }
        assertEquals(0,machine.pendingCount)
        // A queued BEGIN is known before the file worker enters storage.
        machine.register(id);assertTrue(machine.requestCancel(id));assertTrue(machine.isCancelled(id))
        assertFalse("Duplicate CANCEL must not enqueue another retirement",machine.requestCancel(id))
        repeat(11) {n->machine.register((n+1).toString(16).padStart(32,'0'))}
        assertEquals(12,machine.pendingCount)
        try {machine.register("f".repeat(32));fail("Pending operations must be bounded")}catch(_:ReceiveViolation) {}
        machine.cancel(id);assertFalse(machine.isPending(id))
        machine.disconnect();assertEquals(0,machine.pendingCount)
        // Retiring CANCEL permits the same transfer ID in a new operation.
        assertEquals(Type.FILE_READY,machine.begin(begin())!!.type)
        machine.cancel(id);assertEquals(0,machine.pendingCount)
        repeat(100) {
            machine.begin(begin());machine.chunk(Frame.Chunk(id,0,"abc".toByteArray()))
            assertEquals(Type.ACK,machine.end(id).type);assertEquals(0,machine.pendingCount)
        }
    }
    @Test fun legacyPublicationKeepsItsOwnCancellationFenceAfterOperationRetires()=runBlocking {
        var fence:()->Boolean={false}
        val machine=FileReceiveMachine(ReceiveBackend {error("Use fenced legacy backend")},
            legacyBackend={_,cancelled->fence=cancelled;ReceiveAdmission(sink=Sink())})
        machine.begin(begin(false));assertFalse(fence())
        machine.requestCancel(id);assertTrue(fence());machine.cancel(id)
        assertEquals(0,machine.pendingCount);assertTrue(fence())
        val oldFence=fence
        machine.begin(begin(false));assertFalse(fence());assertTrue(oldFence())
        machine.disconnect()
    }
    @Test fun disconnectRelinquishesEligiblePrefixButMismatchPurgesIt()=runBlocking {
        val sink=Sink();val machine=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=sink)},resumeCapable=true)
        machine.begin(begin());machine.chunk(Frame.Chunk(id,0,"a".toByteArray()));machine.disconnect();assertTrue(sink.retained);assertFalse(sink.aborted)
        val bad=Sink();val mismatch=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=bad)},resumeCapable=true)
        mismatch.begin(begin());mismatch.chunk(Frame.Chunk(id,0,"abd".toByteArray()))
        assertEquals("hash_mismatch",mismatch.end(id).body["code"].asString);assertTrue(bad.aborted)
    }
}
