package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test

class FileSendPhaseTest {
    private val id="00112233445566778899aabbccddeeff"
    private fun ready(offset:Long=2,id:String=this.id)=Frame.Control(Type.FILE_READY,json("transfer_id" to id,"offset" to offset))
    private fun ack()=Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to false))
    private fun invalid(code:String="unexpected_message",block:()->Unit) {try {block();fail()}catch(e:FilePhaseViolation) {assertEquals(code,e.code)}}
    @Test fun readyThenReceiptWithRangeAndPhaseChecks()=runBlocking {
        val phase=FileSendPhase(id,3,true)
        invalid {phase.response(ack())};invalid {phase.response(ready(id="f".repeat(32)))}
        invalid("malformed_message") {phase.response(ready(4))}
        phase.response(ready());assertEquals(2L,phase.readiness.await().body["offset"].asLong)
        invalid {phase.response(ready())};invalid {phase.response(ack())}
        phase.ending();phase.response(ack());assertEquals(Type.ACK,phase.receipt.await().type)
        invalid {phase.response(ack())}
    }
    @Test fun earlyNackAndLegacyNeverWaitForReady()=runBlocking {
        val resume=FileSendPhase(id,3,true)
        resume.response(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "disk_full","retryable" to true)))
        assertEquals(Type.NACK,resume.readiness.await().type);invalid {resume.ending()}
        val legacy=FileSendPhase(id,3,false);invalid {legacy.response(ready())}
        legacy.ending();legacy.response(ack());assertTrue(legacy.receipt.isCompleted)
    }
    @Test fun cancelConsumesOnlyItsSingleExpectedInflightResponse() {
        val beforeReady=FileSendPhase(id,3,true);beforeReady.cancel();beforeReady.response(ready());invalid {beforeReady.response(ready())};invalid {beforeReady.response(ack())}
        val ending=FileSendPhase(id,3,true);ending.response(ready());ending.ending();ending.cancel();ending.response(ack());invalid {ending.response(ack())}
        val sending=FileSendPhase(id,3,true);sending.response(ready());sending.cancel();invalid {sending.response(ack())}
    }
}
