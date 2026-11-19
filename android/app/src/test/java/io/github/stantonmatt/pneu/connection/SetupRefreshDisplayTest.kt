package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.Peer
import org.junit.Assert.*
import org.junit.Test

class SetupRefreshDisplayTest {
    private val peer=Peer("PC","b".repeat(64),"b".repeat(32),emptyList(),generation=7)
    private val connected=LinkState("Connected","Wi-Fi")
    @Test fun plannedReconnectHasBoundedGraceAndNeverKeepsProgressOrAuthority() {
        val display=SetupRefreshDisplay()
        assertTrue(display.begin(peer,connected,100))
        val offline=LinkState("PC unreachable")
        val held=display.project(peer,offline,101)
        assertEquals("Connected",held.status);assertEquals("Wi-Fi",held.route)
        assertTrue(held.capabilities.isEmpty());assertNull(held.incoming)
        assertFalse(display.begin(peer,connected,4000))
        assertEquals("Connected",display.project(peer,LinkState("Connecting…"),5099).status)
        assertEquals(offline,display.project(peer,offline,5100))
    }
    @Test fun realFailuresRevocationAndReplacementBypassGrace() {
        for(changed in listOf(peer.copy(pairingLost=true),peer.copy(generation=8))) {
            val display=SetupRefreshDisplay();display.begin(peer,connected,0)
            assertEquals("PC unreachable",display.project(changed,LinkState(),1).status)
        }
        for(status in listOf("Pairing lost","No network","Waiting for Wi-Fi","Fingerprint mismatch")) {
            val display=SetupRefreshDisplay();display.begin(peer,connected,0)
            assertEquals(status,display.project(peer,LinkState(status),1).status)
        }
        val display=SetupRefreshDisplay()
        assertFalse(display.begin(peer,LinkState(),0))
        assertEquals("PC unreachable",display.project(peer,LinkState(),1).status)
    }
    @Test fun readyEndsGraceSoAnotherUnplannedDisconnectIsVisible() {
        val display=SetupRefreshDisplay();display.begin(peer,connected,0)
        display.project(peer,LinkState(),1)
        assertEquals(connected,display.project(peer,connected,2))
        assertEquals("PC unreachable",display.project(peer,LinkState(),3).status)
    }
}
