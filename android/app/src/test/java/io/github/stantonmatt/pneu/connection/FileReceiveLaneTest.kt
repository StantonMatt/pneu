package io.github.stantonmatt.pneu.connection

import com.google.gson.JsonParser
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test

class FileReceiveLaneTest {
    @Test fun saturationFailsOnlyTheTransferAndLeavesTerminalAndControlProcessingAvailable()=runBlocking {
        val id="a".repeat(32)
        var aborted=false
        val machine=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(sink=object:ReceiveSink {
            override fun write(bytes:ByteArray) {error("Saturated transfer must drain")}
            override fun seal() {error("Must not publish")}
            override suspend fun commit():ReceiveReply=error("Must not publish")
            override suspend fun abort() {aborted=true}
        })})
        val meta=ReceivedMetadata(id,"test.bin","application/octet-stream",9,sha256(ByteArray(9)),"file",123)
        machine.begin(JsonParser.parseString(meta.canonical).asJsonObject)
        val lane=FileReceiveLane(machine::requestFailure)
        // No worker runs until the queue is full: a deterministic slow-storage seam.
        withTimeout(1000) {
            repeat(9) {lane.offer(Frame.Chunk(id,it.toLong(),byteArrayOf(0)))}
            lane.offer(Frame.Control(Type.FILE_END,json("transfer_id" to id)))
            lane.offer(Frame.Control(Type.CANCEL,json("transfer_id" to id)))
        }
        repeat(8) {machine.chunk(lane.receive() as Frame.Chunk)}
        assertEquals(Type.FILE_END,(lane.receive() as Frame.Control).type)
        assertEquals("storage_failed",machine.end(id).body["code"].asString)
        assertTrue(aborted);assertFalse(machine.active)
        assertEquals(Type.CANCEL,(lane.receive() as Frame.Control).type)
        machine.cancel(id)
        lane.close()
    }
}
