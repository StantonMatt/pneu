package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.features.ReceiveDiagnostics
import io.github.stantonmatt.pneu.features.ReceiveViolation
import io.github.stantonmatt.pneu.protocol.Type
import io.github.stantonmatt.pneu.protocol.Frame
import kotlinx.coroutines.channels.Channel
import java.util.concurrent.atomic.AtomicInteger

/** Eight queued chunks (2 MiB); three control slots let BEGIN, END and CANCEL
 * pass a saturated stream. Payload admission never waits on storage. */
internal class FileReceiveLane(private val failed:(String)->Unit,private val peerId:String="unknown") {
    private val frames=Channel<Frame>(11)
    private val chunks=AtomicInteger()
    private val warned=java.util.concurrent.ConcurrentHashMap.newKeySet<String>()
    private fun overflow(id:String,pending:Int,phase:String) {
        if(warned.add(id))ReceiveDiagnostics.warn(peerId,id,"file",phase,
            counts=" pending_chunks=${pending-1} chunk_limit=8 requested_chunks=1")
        failed(id)
    }
    fun offer(frame:Frame) {
        if(frame is Frame.Chunk) {
            val pending=chunks.incrementAndGet()
            if(pending>8) {chunks.decrementAndGet();overflow(frame.id,pending,"lane_overflow");return}
            if(frames.trySend(frame).isFailure) {chunks.decrementAndGet();overflow(frame.id,pending,"lane_insert")}
        } else if(frames.trySend(frame).isFailure)throw ReceiveViolation()
    }
    suspend fun receive():Frame=frames.receive().also {
        if(it is Frame.Chunk)chunks.decrementAndGet()
        else if(it is Frame.Control && it.type in setOf(Type.FILE_END,Type.CANCEL))warned.remove(it.body["transfer_id"].asString)
    }
    fun close() {frames.close()}
}
