package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.Peer

/** Presentation only: READY and session authority always come from the live worker.
 * Scope changes still reconnect with a fresh HELLO. Never extend a pending grace. */
internal class SetupRefreshDisplay {
    companion object {const val GRACE_MS=5000L}
    private data class Pending(val generation:Long,val until:Long,val prior:LinkState,var interrupted:Boolean=false)
    private val pending=mutableMapOf<String,Pending>()
    @Synchronized fun begin(peer:Peer,state:LinkState,now:Long):Boolean {
        if(peer.pin in pending || state.connectionStatus!=ConnectionStatus.CONNECTED || peer.pairingLost)return false
        pending[peer.pin]=Pending(peer.generation,now+GRACE_MS,state)
        return true
    }
    @Synchronized fun project(peer:Peer,state:LinkState,now:Long):LinkState {
        val held=pending[peer.pin] ?: return state
        if(peer.generation!=held.generation || peer.pairingLost || now>=held.until) {pending.remove(peer.pin);return state}
        // A real failure must be visible immediately. Only ordinary close/dial states get grace.
        if(state.status !in setOf("Connected","PC unreachable","Connecting…")) {pending.remove(peer.pin);return state}
        if(state.connectionStatus==ConnectionStatus.CONNECTED) {
            if(held.interrupted)pending.remove(peer.pin)
            return state
        }
        held.interrupted=true
        return state.copy(status="Connected",route=held.prior.route,cellular=held.prior.cellular)
    }
    @Synchronized fun clear()=pending.clear()
}
