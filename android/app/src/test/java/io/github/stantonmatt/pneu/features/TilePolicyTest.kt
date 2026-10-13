package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.*
import org.junit.Assert.*
import org.junit.Test

class TilePolicyTest {
    @Test fun lostPairingWithNoPeerWinsOverUnpairedPausedAndStaleConnectedState() {
        for(clipboard in listOf(false,true)) {
            val p=Prefs(peer=null,pairingLost=true,paused=true,lostPcName="Lost PC")
            val tile=TilePolicy.present(p,"Connected","Wi-Fi",clipboard)
            assertTrue(tile.available);assertFalse(tile.active);assertTrue(tile.recovery)
            assertEquals(R.string.status_pairing_lost,tile.subtitle)
            assertFalse(TilePolicy.present(p.copy(quickSettings=false),"Connected","Wi-Fi",clipboard).available)
            assertFalse(TilePolicy.present(p.copy(loaded=false),"Connected","Wi-Fi",clipboard).available)
        }
    }
    @Test fun trulyUnpairedAndDisabledTilesRemainUnavailable() {
        for(clipboard in listOf(false,true)) {
            val p=Prefs(peer=Peer("PC","pin","pc",emptyList()))
            assertFalse(TilePolicy.present(p.copy(peer=null),"Connected","Wi-Fi",clipboard).available)
            assertEquals(R.string.not_paired,TilePolicy.present(p.copy(peer=null),"Offline","",clipboard).subtitle)
            assertFalse(TilePolicy.present(p.copy(quickSettings=false),"Connected","Wi-Fi",clipboard).available)
            assertTrue(TilePolicy.present(p,"Connected","Wi-Fi",clipboard).available)
            assertFalse(TilePolicy.present(p,"Connected","Wi-Fi",clipboard).recovery)
        }
    }
}
