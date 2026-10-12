package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*
import org.junit.Assert.*
import org.junit.Test

class BindingStateTest {
    @Test fun successRequiresMatchingLiveBinding() {
        val peer=Peer("PC","pin","id",emptyList())
        val link=LinkState(pairing="Paired with PC",pairingPin="pin")
        assertTrue(link.pairingSuccess(Prefs(peer=peer)))
        assertFalse(link.pairingSuccess(Prefs()))
        assertFalse(link.pairingSuccess(Prefs(peer=peer.copy(pin="replacement"))))
        assertFalse(link.pairingSuccess(Prefs(peer=peer,pairingLost=true)))
        assertFalse(LinkState(pairing="Paired with PC").pairingSuccess(Prefs(peer=peer)))
    }
}
