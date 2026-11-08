package io.github.stantonmatt.pneu.features

/** Receive diagnostics must never include metadata, content, exception messages or
 * throwables (logcat would print their messages and stack traces). */
internal object ReceiveDiagnostics {
    fun warn(peer:String,id:String,kind:String,phase:String,error:Exception?=null,
        code:String="storage_failed",counts:String="") {
        // Diagnostics must not change the reply, even if logging itself fails.
        runCatching {
            val classes=mutableListOf<String>()
            val seen=java.util.Collections.newSetFromMap(java.util.IdentityHashMap<Throwable,Boolean>())
            var cause:Throwable?=error
            while(cause!=null && classes.size<8 && seen.add(cause)) {
                classes.add(cause.javaClass.name)
                cause=cause.cause
            }
            android.util.Log.w(if(kind=="text")"PneuClipboard" else "PneuReceive",
                "receive_rejected peer=$peer transfer_id=$id kind=$kind phase=$phase code=$code"+
                    (if(classes.isEmpty())"" else " exceptions=${classes.joinToString("->")}")+counts)
        }
    }
}
