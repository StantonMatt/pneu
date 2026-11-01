package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.features.ReceiveViolation
import io.github.stantonmatt.pneu.protocol.Frame

/** Reader validation is installed before READY can expose the stream to the peer. */
internal class FileReceiveState {
    private data class Phase(val id:String,val size:Long,val ready:Boolean,val ended:Boolean=false,val offset:Long=0)
    private var phase:Phase?=null
    @Synchronized fun begin(id:String,size:Long,resumable:Boolean) {
        if(phase!=null)throw ReceiveViolation()
        phase=Phase(id,size,!resumable)
    }
    @Synchronized fun chunk(frame:Frame.Chunk) {
        val current=phase ?: throw ReceiveViolation()
        if(!current.ready || current.ended || current.id!=frame.id || frame.offset!=current.offset ||
            frame.data.size.toLong()>current.size-current.offset)throw ReceiveViolation()
        phase=current.copy(offset=current.offset+frame.data.size)
    }
    @Synchronized fun end(id:String) {
        val current=phase ?: throw ReceiveViolation()
        if(!current.ready || current.ended || current.id!=id)throw ReceiveViolation()
        phase=current.copy(ended=true)
    }
    @Synchronized fun clear(id:String):Boolean {
        if(phase?.id==id)phase=null
        return phase==null
    }
    private fun installReady(id:String,offset:Long):Phase?=synchronized(this) {
        val current=phase?.takeIf {it.id==id} ?: return@synchronized null
        check(offset in 0..current.size)
        current.copy(ready=true,offset=offset).also {phase=it}
    }
    suspend fun ready(id:String,offset:Long,emit:suspend ()->Unit) {
        val installed=installReady(id,offset) ?: return
        try {emit()}catch(e:Throwable) {
            synchronized(this) {if(phase?.id==installed.id)phase=null}
            throw e
        }
    }
}
