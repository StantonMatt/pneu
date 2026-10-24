package io.github.stantonmatt.pneu.data

/** A frozen recipient and binding generation, independent of other pairings. */
data class DeliveryBinding(val phoneId:String,val pin:String,val pcId:String,val generation:Long=0,val sessionId:String?=null) {
    fun matches(p:Prefs)=p.loaded && !p.deliveryReset && p.deviceId==phoneId && pcId !in p.resetPeers &&
        p.devices.any {it.deviceId==pcId && it.pin==pin && it.generation==generation && !it.pairingLost} &&
        !(p.peers.isEmpty() && p.pairingLost)
    companion object {
        fun from(p:Prefs,peerId:String?=p.devices.firstOrNull {!it.pairingLost && it.deviceId !in p.resetPeers}?.deviceId):DeliveryBinding? =
            p.devices.find {it.deviceId==peerId}?.let {DeliveryBinding(p.deviceId,it.pin,it.deviceId,it.generation)}
                ?.takeIf {p.deviceId.isNotEmpty() && it.matches(p)}
    }
}
