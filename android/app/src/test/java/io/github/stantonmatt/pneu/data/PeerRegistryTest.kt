package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import org.junit.Assert.*
import org.junit.Test

class PeerRegistryTest {
    private val phone="a".repeat(32)
    private val pc="b".repeat(32)
    private fun legacy()=mutablePreferencesOf(
        stringPreferencesKey("deviceId") to phone, stringPreferencesKey("tlsAlias") to "existing-key",
        stringPreferencesKey("pin") to "c".repeat(64), stringPreferencesKey("pcId") to pc,
        stringPreferencesKey("pcName") to "Office", stringPreferencesKey("addresses") to "192.168.1.2:47821",
        stringPreferencesKey("lastLan") to "192.168.1.3:47821", longPreferencesKey("lastConnected") to 123L,
        booleanPreferencesKey("clipboardFromPc") to false)

    @Test fun migrationPreservesIdentityEndpointPolicyAndIsIdempotent() {
        val data=legacy();PeerRegistry.migrate(data)
        val peer=PeerRegistry.read(data).single()
        assertEquals(pc,peer.deviceId);assertEquals("Office",peer.name);assertEquals("computer",peer.role)
        assertEquals("192.168.1.3:47821",peer.lastLan);assertEquals(123L,peer.lastConnected)
        assertFalse(peer.clipboardReceive);assertTrue(peer.clipboardSend);assertTrue(peer.automaticMedia)
        assertEquals(phone,data[stringPreferencesKey("deviceId")]);assertEquals("existing-key",data[stringPreferencesKey("tlsAlias")])
        val once=data.toPreferences();PeerRegistry.migrate(data);assertEquals(once,data.toPreferences())
    }
    @Test fun interruptedEditCanBeRetriedWithoutDuplicatingOrLosingThePeer() {
        val durable=legacy().toPreferences();val interrupted=durable.toMutablePreferences()
        PeerRegistry.migrate(interrupted) // simulate failure before DataStore commits the edit
        assertNull(durable[stringPreferencesKey("peers")])
        val retry=durable.toMutablePreferences();PeerRegistry.migrate(retry)
        assertEquals(PeerRegistry.read(interrupted),PeerRegistry.read(retry))
        assertEquals(1,PeerRegistry.read(retry).size)
    }
    @Test fun addingRepairingAndRemovingOnePeerDoesNotChangeAnotherBindingOrIdentity() {
        val data=legacy();PeerRegistry.migrate(data)
        val first=PeerRegistry.read(data).single();val second=PairSettings.initialize(first.copy(deviceId="d".repeat(32),pin="e".repeat(64),settings=emptyMap()),phone)
        PeerRegistry.write(data,listOf(first,second))
        val p=Prefs(deviceId=phone,peers=PeerRegistry.read(data))
        val firstBinding=DeliveryBinding.from(p,pc)!!;val secondBinding=DeliveryBinding.from(p,second.deviceId)!!
        val repaired=p.copy(peers=listOf(first.copy(generation=first.generation+1),second))
        assertFalse(firstBinding.matches(repaired));assertTrue(secondBinding.matches(repaired))
        PeerRegistry.remove(data,pc)
        assertEquals(listOf(second),PeerRegistry.read(data));assertTrue(secondBinding.matches(p.copy(peers=PeerRegistry.read(data))))
        assertEquals(phone,data[stringPreferencesKey("deviceId")]);assertEquals("existing-key",data[stringPreferencesKey("tlsAlias")])
    }
    @Test fun corruptPeerStorageFailsClosedInsteadOfFallingBackToLegacyTrust() {
        val data=legacy();data[stringPreferencesKey("peers")]="broken"
        assertThrows(Exception::class.java) {PeerRegistry.migrate(data)}
    }
    @Test fun migrationKeepsLegacyFullResetObligationsWithOrWithoutPeers() {
        for(hasPeer in listOf(true,false)) {
            val data=legacy();if(!hasPeer)data.remove(stringPreferencesKey("pin"))
            data[booleanPreferencesKey("deliveryReset")]=true
            PeerRegistry.migrate(data);PeerRegistry.migrate(data)
            assertEquals(true,data[booleanPreferencesKey("deliveryReset")])
        }
    }

    @Test fun peerLimitRejectsAdditionalWorkersWithoutChangingRegistry() {
        val data=legacy();PeerRegistry.migrate(data)
        val peers=(1..PeerRegistry.MAX_PEERS).map {Peer("PC$it","pin$it","pc$it",emptyList())}
        PeerRegistry.write(data,peers);val before=data.toPreferences()
        assertThrows(IllegalArgumentException::class.java) {PeerRegistry.write(data,peers+peers[0].copy(deviceId="extra"))}
        assertEquals(before,data.toPreferences())
    }

}
