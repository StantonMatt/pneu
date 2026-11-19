package io.github.stantonmatt.pneu.connection

import java.util.concurrent.atomic.AtomicReference

/** Writer-lock-owned frame, with completion published on the IO thread before
 * returning to the sender's dispatcher. Cancellation claims only an unfinished frame. */
internal class FileFrameWriter {
    private val writing=AtomicReference<String?>()
    fun write(id:String?,output:()->Unit,afterFlushed:()->Unit={}) {
        writing.set(id)
        try {
            output()
            writing.compareAndSet(id,null)
            afterFlushed()
        }finally {writing.compareAndSet(id,null)}
    }
    fun cancel(id:String):Boolean=writing.compareAndSet(id,null)
}
