package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import org.junit.Test
import java.math.BigInteger

class MeshStateTest {
    private val phone="a".repeat(32);private val pc="b".repeat(32)
    private val peer=PairSettings.initialize(Peer("PC","c".repeat(64),pc,emptyList()),phone)
    private fun incoming(hlc:Long,writer:String=pc,enabled:Boolean=false)=json("transfer_id" to "d".repeat(32),"settings" to PairSettings.snapshot(peer).apply {
        first {it.asJsonObject["key"].asString=="$phone/$pc/clipboard"}.asJsonObject.apply {addProperty("hlc",hlc);addProperty("writer_device",writer);addProperty("enabled",enabled)}
    })
    @Test fun globalRegisterOrdersDifferentSourcesAndLocalCopiesAcrossClockRollback() {
        val data=mutablePreferencesOf()
        val first=MeshClock.next(data,phone,true,100)
        assertEquals(BigInteger.valueOf(100),first.hlc)
        assertFalse(MeshClock.wins(data,ClipboardStamp(BigInteger.valueOf(99),pc)))
        val later=ClipboardStamp(first.hlc,pc);assertTrue(MeshClock.wins(data,later))
        MeshClock.observe(data,later.hlc,100);MeshClock.applied(data,later)
        assertFalse(MeshClock.wins(data,first))
        assertEquals(BigInteger.valueOf(101),MeshClock.next(data,phone,true,1).hlc)
        val restored=data.toPreferences().toMutablePreferences()
        assertEquals(BigInteger.valueOf(102),MeshClock.next(restored,phone,false,1).hlc)
        assertTrue(MeshClock.wins(restored,ClipboardStamp(BigInteger.valueOf(102),pc)))
    }
    @Test fun futureU64AndExhaustionNeverMutateOrWrapTheClock() {
        val data=mutablePreferencesOf();val before=data.toPreferences()
        assertThrows(MeshViolation::class.java) {MeshClock.observe(data,BigInteger("18446744073709551615"),100)}
        assertEquals(before,data.toPreferences())
        data[stringPreferencesKey("meshSeen")]="18446744073709551615"
        assertThrows(IllegalStateException::class.java) {MeshClock.next(data,phone,true,100)}
    }
    @Test fun settingsMergeUsesEachFieldsClockThenWriterAndPreservesPendingState() {
        assertFalse(PairSettings.pending(peer));assertTrue(PairSettings.syncing(peer))
        val confirmed=peer.copy(confirmedSettings=PairSettings.signature(peer));assertFalse(PairSettings.pending(confirmed))
        val first=PairSettings.merge(confirmed,phone,incoming(10,phone),100)
        assertFalse(first.clipboardSend);assertTrue(first.clipboardReceive);assertTrue(first.automaticMedia)
        assertFalse(PairSettings.pending(first));assertTrue(PairSettings.syncing(first))
        val tie=PairSettings.merge(first,phone,incoming(10,pc,true),100)
        assertTrue(tie.clipboardSend)
        assertEquals(tie,PairSettings.merge(tie,phone,incoming(9,phone),100))
        assertThrows(MeshViolation::class.java) {PairSettings.merge(tie,phone,incoming(10,pc,false),100)}
    }
    @Test fun invalidOrIncompleteSettingsSnapshotCannotPartiallyMerge() {
        val invalid=incoming(100);invalid["settings"].asJsonArray.get(2).asJsonObject.addProperty("hlc",60101)
        assertThrows(MeshViolation::class.java) {PairSettings.merge(peer,phone,invalid,100)}
        val incomplete=incoming(1);incomplete["settings"].asJsonArray.remove(2)
        assertThrows(MeshViolation::class.java) {PairSettings.merge(peer,phone,incomplete,100)}
        assertTrue(peer.clipboardSend)
    }
    @Test fun fanOutFreezesRecipientsAndAutomaticMediaNeverAddsOfflinePeers() {
        val second=peer.copy(deviceId="e".repeat(32),pin="f".repeat(64),settings=emptyMap())
        val prefs=Prefs(deviceId=phone,peers=listOf(peer,second))
        val frozen=RecipientSelection.freeze(prefs,setOf(pc),"file")
        assertEquals(listOf(pc),frozen.map {it.pcId})
        assertEquals(listOf(pc,second.deviceId),RecipientSelection.freeze(prefs,setOf(pc,second.deviceId),"file").map {it.pcId})
        assertEquals(listOf(pc),frozen.map {it.pcId})
        assertTrue(RecipientSelection.freeze(prefs,emptySet(),"photo",true).isEmpty())
        assertTrue(RecipientSelection.freeze(prefs,emptySet(),"text").isEmpty())
        assertEquals(listOf(second.deviceId),RecipientSelection.freeze(prefs,setOf(pc),"file",selected=setOf(second.deviceId)).map {it.pcId})
        assertTrue(RecipientSelection.freeze(prefs.copy(peers=listOf(peer.copy(automaticMedia=false))),setOf(pc),"photo",true).isEmpty())
    }
    @Test fun explicitOfflineQueueAddsOfflineAutomaticRecipientsEvenWithOnlinePeers() {
        val second=peer.copy(deviceId="e".repeat(32),pin="f".repeat(64))
        val prefs=Prefs(deviceId=phone,peers=listOf(peer,second),queue=true)
        assertEquals(2,RecipientSelection.freeze(prefs,setOf(pc),"photo",true).size)
        assertEquals(2,RecipientSelection.freeze(prefs,emptySet(),"recording",true).size)
        assertEquals(listOf(pc),RecipientSelection.freeze(prefs.copy(queue=false),setOf(pc),"photo",true).map {it.pcId})
    }

    @Test fun frozenRecipientsCannotAcquireAReplacementOrLateOnlinePeer() {
        val other=peer.copy(deviceId="e".repeat(32),pin="f".repeat(64))
        val p=Prefs(deviceId=phone,peers=listOf(peer,other),queue=false)
        val frozen=RecipientSelection.freeze(p,setOf(pc),"text")
        val repaired=p.copy(peers=listOf(peer.copy(generation=1),other))
        assertTrue(frozen.filter {it.matches(repaired)}.isEmpty())
        assertEquals(listOf(pc),frozen.map {it.pcId})
        assertEquals(2,RecipientSelection.freeze(repaired,setOf(pc,other.deviceId),"text").size)
    }

}
