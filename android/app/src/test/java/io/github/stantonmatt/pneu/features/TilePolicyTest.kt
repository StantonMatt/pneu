package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.test.runTest
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
    @Test fun clipboardTileUsesRecipientEligibilityIndependentlyOfTheDisplayedStatus() {
        val a=Peer("A","a","a",emptyList());val b=a.copy(name="B",pin="b",deviceId="b")
        val p=Prefs(deviceId="phone",peers=listOf(a,b))
        assertTrue(TilePolicy.present(p,"PC unreachable","",clipboard=true,clipboardAvailable=true).available)
        assertFalse(TilePolicy.present(p,"Connected","Wi-Fi",clipboard=true,clipboardAvailable=false).available)
        assertFalse(TilePolicy.present(p.copy(quickSettings=false),"Connected","Wi-Fi",clipboard=true,clipboardAvailable=true).available)
    }
    private val a=Peer("A","a","a",emptyList());private val b=a.copy(name="B",pin="b",deviceId="b")
    private val prefs=Prefs(peer=a,peers=listOf(a,b))
    private val connected=LinkState("Connected","Wi-Fi")
    private val links=mapOf(a.deviceId to connected,b.deviceId to LinkState())
    @Test fun refreshEqualityTracksPairingPreferencesRoutesCountsAndClipboardEligibility() {
        val original=TilePolicy.inputs(prefs,connected,links,true)
        for(p in listOf(prefs.copy(loaded=false),prefs.copy(peer=b),prefs.copy(peers=listOf(a)),
            prefs.copy(peers=listOf(a,b.copy(deviceId="replacement"))),prefs.copy(paused=true),prefs.copy(quickSettings=false),prefs.copy(pairingLost=true))) {
            assertNotEquals(original,TilePolicy.inputs(p,connected,links,true))
        }
        assertNotEquals(original,TilePolicy.inputs(prefs,connected.copy(status="Connecting…"),links,true))
        assertNotEquals(original,TilePolicy.inputs(prefs,connected.copy(route="Tailscale"),links,true))
        assertNotEquals(original,TilePolicy.inputs(prefs,connected,links+(b.deviceId to connected),true))
        assertNotEquals(original,TilePolicy.inputs(prefs,connected,links,false))
        assertNotEquals(TilePolicy.inputs(Prefs(loaded=false),LinkState(),emptyMap(),false),TilePolicy.inputs(Prefs(),LinkState(),emptyMap(),false))
        assertNotEquals(TilePolicy.inputs(Prefs(),LinkState(),emptyMap(),false),TilePolicy.inputs(Prefs(peer=a),LinkState(),emptyMap(),false))
    }
    @Test fun refreshDedupesTransferProgressAndUnrelatedSettingsButKeepsEligibilityChanges()=runTest {
        val original=TilePolicy.inputs(prefs,connected,links,true)
        val progress=connected.copy(sent=20,total=100,transferMediaId="transfer",transferFileName="file",incoming=IncomingTransfer("a","incoming","file",10,100))
        val unrelated=prefs.copy(lastReceivedHash="hash",photos=true,mediaSince=12,peers=listOf(b,a.copy(lastConnected=123,lastLan="192.168.1.2")))
        val unchanged=TilePolicy.inputs(unrelated,progress,links+(a.deviceId to progress),true)
        assertEquals(original,unchanged)
        val unavailable=TilePolicy.inputs(unrelated,progress,links,false)
        val emissions=flowOf(original,unchanged,unavailable,unavailable,original).distinctUntilChanged().toList()
        assertEquals(listOf(original,unavailable,original),emissions)
    }
    @Test fun presentationAndCountsRetainTheCombinedSnapshotAndIgnoreRemovedConnections() {
        val devices=MutableStateFlow(links+("removed" to connected))
        val snapshot=TilePolicy.inputs(prefs,connected,devices.value,true)
        devices.value=mapOf(a.deviceId to LinkState())
        assertEquals(1,snapshot.connectedCount);assertEquals(2,snapshot.devices.size)
        assertTrue(TilePolicy.present(snapshot).active);assertTrue(TilePolicy.present(snapshot,clipboard=true).available)
        val later=TilePolicy.inputs(prefs,LinkState(),devices.value,false)
        assertEquals(0,later.connectedCount);assertFalse(TilePolicy.present(later,clipboard=true).available)
        val single=TilePolicy.inputs(prefs.copy(peer=b,peers=listOf(b)),connected,links,true)
        assertEquals(0,single.connectedCount);assertEquals(1,single.devices.size)
    }

}
