package io.github.stantonmatt.pneu.features

import android.app.Application
import android.util.Log
import com.google.gson.JsonParser
import io.github.stantonmatt.pneu.connection.FileReceiveLane
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowLog
import java.io.IOException

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class FileReceiveLoggingTest {
    private val peer="b".repeat(32)
    private val id="a".repeat(32)
    private val privateValue="private-file.png content://private/provider /private/path pairing-secret"
    private fun warnings()=ShadowLog.getLogsForTag("PneuReceive").also {logs->
        for(log in logs) {
            assertEquals(Log.WARN,log.type);assertNull(log.throwable)
            for(value in listOf("private-file.png","content://private/provider","/private/path","pairing-secret"))
                assertFalse(log.msg.contains(value))
        }
    }
    @Test fun overflowLogsCountsOnlyOncePerTransferAndStillFencesEveryDroppedChunk()=runBlocking<Unit> {
        ShadowLog.clear()
        val failed=mutableListOf<String>()
        val lane=FileReceiveLane({failed.add(it)},peerId=peer)
        try {
            repeat(12) {lane.offer(Frame.Chunk(id,it.toLong(),privateValue.toByteArray()))}
            assertEquals(4,failed.size)
            val first=warnings().single().msg
            assertTrue(first.contains("peer=$peer transfer_id=$id"))
            assertTrue(first.contains("phase=lane_overflow code=storage_failed"))
            assertTrue(first.contains("pending_chunks=8 chunk_limit=8 requested_chunks=1"))
            val second="c".repeat(32)
            lane.offer(Frame.Chunk(second,0,byteArrayOf(0)))
            assertEquals(2,warnings().size);assertEquals(second,failed.last())
            lane.offer(Frame.Control(Type.FILE_END,json("transfer_id" to id)))
            repeat(8) {assertTrue(lane.receive() is Frame.Chunk)}
            assertEquals(Type.FILE_END,(lane.receive() as Frame.Control).type)
        } finally {lane.close()}
    }
    @Test fun failedChannelInsertLogsOnceWithCounts() {
        ShadowLog.clear()
        val failed=mutableListOf<String>()
        val lane=FileReceiveLane({failed.add(it)},peerId=peer)
        try {
            // Controls fill the channel without consuming the chunk budget.
            repeat(11) {lane.offer(Frame.Control(Type.FILE_BEGIN,json("transfer_id" to id)))}
            repeat(2) {lane.offer(Frame.Chunk(id,0,privateValue.toByteArray()))}
            assertEquals(listOf(id,id),failed)
            val line=warnings().single().msg
            assertTrue(line.contains("peer=$peer transfer_id=$id"))
            assertTrue(line.contains("phase=lane_insert code=storage_failed"))
            assertTrue(line.contains("pending_chunks=0 chunk_limit=8"))
        } finally {lane.close()}
    }
    @Test fun storageExceptionsLogAdmissionWriteSealAndCommitWithoutMetadataOrMessages()=runBlocking<Unit> {
        val data=privateValue.toByteArray()
        val meta=ReceivedMetadata(id,"private-file.png","image/png",data.size.toLong(),sha256(data),"clipboard_image",123)
        for(phase in listOf("admit","write","seal","commit")) {
            ShadowLog.clear()
            fun failAt(step:String) {if(step==phase)throw IOException(privateValue,IllegalStateException(privateValue))}
            var aborted=false
            val sink=object:ReceiveSink {
                override fun write(bytes:ByteArray) {failAt("write")}
                override fun seal() {failAt("seal")}
                override suspend fun commit():ReceiveReply {failAt("commit");return ReceiveReply()}
                override suspend fun abort() {aborted=true}
            }
            val machine=FileReceiveMachine(ReceiveBackend {failAt("admit");ReceiveAdmission(sink=sink)},peerId=peer)
            assertNull(machine.begin(JsonParser.parseString(meta.canonical).asJsonObject))
            machine.chunk(Frame.Chunk(id,0,data))
            val reply=machine.end(id)
            assertEquals("storage_failed",reply.body["code"].asString)
            assertTrue(reply.body["retryable"].asBoolean);assertFalse(machine.active)
            if(phase!="admit")assertTrue(aborted)
            val line=warnings().single().msg
            assertTrue(line.contains("peer=$peer transfer_id=$id kind=clipboard_image phase=$phase"))
            assertTrue(line.contains("exceptions=java.io.IOException->java.lang.IllegalStateException"))
            assertFalse(line.contains(meta.hash))
        }
    }
}
