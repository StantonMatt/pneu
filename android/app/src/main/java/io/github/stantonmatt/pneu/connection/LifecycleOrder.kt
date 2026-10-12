package io.github.stantonmatt.pneu.connection

/** Never revoke peer trust unless the local durable removal succeeded. */
object LifecycleOrder {
    suspend fun revoke(persist:suspend ()->Boolean,notify:suspend ()->Unit):Boolean {
        if(!persist())return false
        try {notify()} catch(_:Exception) { /* Best effort after durable removal. */ }
        return true
    }
}
