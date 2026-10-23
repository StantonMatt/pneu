package io.github.stantonmatt.pneu.data

import org.junit.Test
import org.junit.Assert.*

class CapturedAuthorityTest {
    private val peer=Peer("Desktop","a".repeat(64),"a".repeat(32),emptyList(),generation=2)
    private val p=Prefs(deviceId="0".repeat(32),peers=listOf(peer))
    private val binding=DeliveryBinding.from(p,peer.deviceId)!!
    @Test fun capturedPairingNeverUpgradesAfterPickerOrProviderWait() {
        val frozen=CapturedAuthority(binding)
        assertTrue(frozen.stillValid(p,null))
        assertFalse(frozen.stillValid(p.copy(peers=listOf(peer.copy(generation=3))),null))
        assertFalse(frozen.stillValid(p.copy(peers=listOf(peer.copy(pin="b".repeat(64)))),null))
        assertFalse(frozen.stillValid(p.copy(peers=listOf(peer.copy(pairingLost=true))),null))
        assertFalse(frozen.stillValid(p.copy(resetPeers=setOf(peer.deviceId)),null))
    }
    @Test fun originatingConnectionSurvivesDisconnectButNotNewAdmissionOrProcess() {
        val captured=CapturedAuthority(binding,"connection-1")
        assertTrue(captured.stillValid(p,"connection-1"))
        assertFalse(captured.stillValid(p,"connection-2"))
        assertFalse(captured.stillValid(p,null))
        // Durable file sends are binding scoped and can wait offline.
        assertTrue(CapturedAuthority(binding).stillValid(p,"connection-2"))
    }
    @Test fun pickerSavedStateRetainsEveryAuthorityFieldAndCannotBindANewPairing() {
        val captured=CapturedAuthority(binding.copy(sessionId="session"),"connection")
        assertEquals(captured,CapturedAuthority.restore(captured.saved()))
        assertFalse(CapturedAuthority.restore(captured.saved())!!.stillValid(p.copy(peers=listOf(peer.copy(generation=3))),"connection"))
        assertNull(CapturedAuthority.restore(listOf("device-id-only")))
    }
}
