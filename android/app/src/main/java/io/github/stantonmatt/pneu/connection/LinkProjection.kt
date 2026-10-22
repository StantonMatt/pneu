package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.Prefs

internal object LinkProjection {
    fun aggregate(p:Prefs,states:Map<String,LinkState>,outcome:LinkState?,pairingPin:String?=null,attempt:PairingAttempt?=null):LinkState {
        val available=p.devices.filterNot {it.pairingLost}.mapNotNull {peer->states[peer.pin]?.let {state->
            if(peer.deviceId !in p.resetPeers)state else state.copy(
                status=if(state.status=="Connected")"PC unreachable" else state.status,
                route="",sent=0,total=0,transferMediaId="",transferKind="",transferFileName="")
        }}
        val base=available.firstOrNull {it.status=="Connected" && it.transferMediaId.isNotEmpty()}
            ?: available.firstOrNull {it.status=="Connected"}
            ?: states[attempt?.invitation?.fingerprint]
            ?: available.firstOrNull {it.status=="Connecting…"}
            ?: available.firstOrNull()
            ?: LinkState(if(p.pairingLost)"Pairing lost" else "PC unreachable")
        val feedback=outcome ?: states[pairingPin]?.takeIf {it.pairingSuccess(p)}
        return if(attempt==null && feedback!=null)base.copy(pairing=feedback.pairing,pairingPin=feedback.pairingPin,
            pairingGeneration=feedback.pairingGeneration,pairingFeedback=feedback.pairingFeedback) else base
    }
}
