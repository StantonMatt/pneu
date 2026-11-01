package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.CompletableDeferred
import java.io.IOException

/** Readiness and receipt phases are registered independently before emission. */
internal class FileSendPhase(val id:String,val size:Long,val resume:Boolean) {
    enum class Phase { READY, SENDING, RECEIPT, DONE }
    private var phase=if(resume)Phase.READY else Phase.SENDING
    private var cancelReady=false
    private var cancelReceipt=false
    @Volatile var cancelled=false
        private set
    val readiness=CompletableDeferred<Frame.Control>()
    val receipt=CompletableDeferred<Frame.Control>()
    @Synchronized fun response(frame:Frame.Control) {
        if(frame.body["transfer_id"].asString!=id)throw FilePhaseViolation()
        if(cancelled) {
            if(frame.type==Type.FILE_READY && cancelReady) {validateReady(frame);cancelReady=false;cancelReceipt=false;return}
            if(frame.type in setOf(Type.ACK,Type.NACK) && cancelReceipt) {cancelReceipt=false;cancelReady=false;return}
            throw FilePhaseViolation()
        }
        when {
            phase==Phase.READY && frame.type==Type.FILE_READY->{validateReady(frame);phase=Phase.SENDING;check(readiness.complete(frame))}
            phase==Phase.READY && frame.type==Type.NACK->{phase=Phase.DONE;check(readiness.complete(frame))}
            phase==Phase.RECEIPT && frame.type in setOf(Type.ACK,Type.NACK)->{phase=Phase.DONE;check(receipt.complete(frame))}
            else->throw FilePhaseViolation()
        }
    }
    private fun validateReady(frame:Frame.Control) {
        if(frame.body["offset"].asLong !in 0..size)throw FilePhaseViolation("malformed_message")
    }
    @Synchronized fun ending() {if(phase!=Phase.SENDING || cancelled)throw FilePhaseViolation();phase=Phase.RECEIPT}
    @Synchronized fun cancel() {
        if(cancelled)return
        cancelled=true;cancelReady=phase==Phase.READY;cancelReceipt=phase in setOf(Phase.READY,Phase.RECEIPT)
    }
}
internal class FilePhaseViolation(val code:String="unexpected_message"):IOException(code)
