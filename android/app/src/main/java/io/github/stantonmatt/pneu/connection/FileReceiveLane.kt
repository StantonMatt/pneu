package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.features.ReceiveViolation
import io.github.stantonmatt.pneu.protocol.Frame
import kotlinx.coroutines.channels.Channel
import java.util.concurrent.atomic.AtomicInteger

/** Eight queued chunks (2 MiB); three control slots let BEGIN, END and CANCEL
 * pass a saturated stream. Payload admission never waits on storage. */
internal class FileReceiveLane(private val failed:(String)->Unit) {
    private val frames=Channel<Frame>(11)
    private val chunks=AtomicInteger()
    fun offer(frame:Frame) {
        if(frame is Frame.Chunk) {
            if(chunks.incrementAndGet()>8) {chunks.decrementAndGet();failed(frame.id);return}
            if(frames.trySend(frame).isFailure) {chunks.decrementAndGet();failed(frame.id)}
        } else if(frames.trySend(frame).isFailure)throw ReceiveViolation()
    }
    suspend fun receive():Frame=frames.receive().also {if(it is Frame.Chunk)chunks.decrementAndGet()}
    fun close() {frames.close()}
}
