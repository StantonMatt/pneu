package io.github.stantonmatt.pneu.data

import io.github.stantonmatt.pneu.protocol.Invitation

/** Invitations do not contain the authenticated device ID. Resolve both keys
 * together, including capacity, before requesting or committing a replacement. */
internal object PairingIdentity {
    fun prior(peers:List<Peer>,invitation:Invitation,deviceId:String,phoneId:String):Peer? {
        require(deviceId.isNotEmpty() && deviceId!=phoneId) {"Remote device ID is the phone's own ID"}
        require(peers.none {it.pin==invitation.fingerprint && it.deviceId!=deviceId}) {"Conflicting peer identity"}
        val matches=peers.filter {it.deviceId==deviceId || it.pin==invitation.fingerprint}
        require(matches.size<=1) {"Conflicting peer identity"}
        require(matches.isNotEmpty() || peers.size<PeerRegistry.MAX_PEERS) {"Computer limit reached"}
        return matches.singleOrNull()
    }
    fun changes(prior:Peer,invitation:Invitation,deviceId:String)=
        prior.deviceId!=deviceId || prior.pin!=invitation.fingerprint
}
