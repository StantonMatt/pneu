package io.github.stantonmatt.pneu.data

/** Recipient selected before any provider lookup or asynchronous admission work.
 * The phone identity also changes on unpair, including re-pairing the same PC. */
data class DeliveryBinding(val phoneId: String, val pin: String, val pcId: String) {
    fun matches(p: Prefs) = p.loaded && !p.deliveryReset && !p.pairingLost && p.deviceId == phoneId &&
        p.peer?.pin == pin && p.peer.deviceId == pcId
    companion object {
        fun from(p: Prefs): DeliveryBinding? = p.peer?.takeIf {p.loaded && !p.deliveryReset && !p.pairingLost && p.deviceId.isNotEmpty()}?.let {DeliveryBinding(p.deviceId,it.pin,it.deviceId)}
    }
}
