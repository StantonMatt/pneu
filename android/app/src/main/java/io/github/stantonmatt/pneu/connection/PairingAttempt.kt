package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.Invitation

/** One process-local scan. The scan snapshot and lifecycle revisions are frozen:
 * HELLO only selects a target from that snapshot, never refreshes its fence. */
@ConsistentCopyVisibility
data class PairingAttempt internal constructor(
    val id:String,
    val invitation:Invitation,
    internal val scanned:Prefs,
    internal val revisions:Map<String,Long>,
    val deviceId:String?=null,
    val setupEnabled:Boolean=true,
    val replaceOld:ConfirmedBinding?=null
) {
    val targetId get()=deviceId ?: scanned.devices.find {it.pin==invitation.fingerprint}?.deviceId
    internal val prior get()=deviceId?.let {id->scanned.devices.find {it.deviceId==id}}
        ?: scanned.devices.find {it.pin==invitation.fingerprint}
    internal fun matches(p:Prefs,current:Map<String,Long>):Boolean {
        if(!p.loaded || scanned.deviceId!=p.deviceId || scanned.membership.withdrawalRevision!=p.membership.withdrawalRevision ||
            scanned.membership.barriers["${deviceId.orEmpty()}/${invitation.fingerprint}"]?.pairingFence()!=p.membership.barriers["${deviceId.orEmpty()}/${invitation.fingerprint}"]?.pairingFence())return false
        fun same(a:Peer?,b:Peer?)=a?.let {Triple(it.deviceId,it.pin,it.generation)}==b?.let {Triple(it.deviceId,it.pin,it.generation)}
        fun revision(key:String)=revisions.getOrDefault(key,0)==current.getOrDefault(key,0)
        val pin=invitation.fingerprint
        if(scanned.membership.barriers.filterKeys {it.endsWith("/$pin")}.mapValues {it.value.pairingFence()}!=p.membership.barriers.filterKeys {it.endsWith("/$pin")}.mapValues {it.value.pairingFence()})return false
        if(!same(scanned.devices.find {it.pin==pin},p.devices.find {it.pin==pin}) || !revision("pin:$pin"))return false
        val id=deviceId ?: return true
        return same(scanned.devices.find {it.deviceId==id},p.devices.find {it.deviceId==id}) && revision("id:$id")
    }
}

/** Captured under AppGraph's binding lock, checked under that same lock. */
data class ConnectionOutcome(val peerId:String,val revision:Long)

internal data class ConnectionView(val prefs:Prefs,val attempt:PairingAttempt?,val outcome:ConnectionOutcome?)
