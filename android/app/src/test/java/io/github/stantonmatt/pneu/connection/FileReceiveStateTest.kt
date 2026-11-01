package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.features.ReceiveViolation
import io.github.stantonmatt.pneu.protocol.Frame
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import java.io.IOException

class FileReceiveStateTest {
    private val id="00112233445566778899aabbccddeeff"
    @Test fun readyAcceptsImmediateSuffixAndCompletionReplayBeforeWriterReturns()=runBlocking {
        val state=FileReceiveState()
        state.begin(id,3,true)
        state.ready(id,2) {
            // The peer's response can run as soon as READY is flushed, while
            // the writer's dispatcher/callback is still held.
            state.chunk(Frame.Chunk(id,2,"c".toByteArray()))
            state.end(id)
        }
        assertTrue(state.clear(id))
        state.begin(id,3,true)
        state.ready(id,3) {state.end(id)}
    }
    @Test fun failedReadyWriteRollsBackAdmissionAndCancelDoesNotEmitReady()=runBlocking {
        val state=FileReceiveState()
        state.begin(id,3,true)
        val failure=IOException("failed flush")
        try {state.ready(id,2) {throw failure};fail()}catch(e:IOException) {assertSame(failure,e)}
        try {state.chunk(Frame.Chunk(id,2,"c".toByteArray()));fail()}catch(_:ReceiveViolation) {}
        state.begin(id,3,true);state.clear(id)
        state.ready(id,2) {fail("Cancelled admission must not emit READY")}
    }
}
