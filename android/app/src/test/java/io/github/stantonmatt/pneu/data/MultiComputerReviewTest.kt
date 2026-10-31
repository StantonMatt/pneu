package io.github.stantonmatt.pneu.data

import android.app.Application
import androidx.datastore.preferences.core.*
import androidx.room.Room
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class MultiComputerReviewTest {
    private val context get()=RuntimeEnvironment.getApplication()
    private val a=Peer("A","a".repeat(64),"a".repeat(32),emptyList(),generation=1)
    private val b=a.copy(name="B",pin="b".repeat(64),deviceId="b".repeat(32),generation=2)
    private val prefs=Prefs(deviceId="f".repeat(32),peer=a,peers=listOf(a,b))
    @Test fun connectedSecondaryComputerWinsOverOfflineOrLostFirstComputer()=runBlocking<Unit>(Dispatchers.IO) {
        for(first in listOf(LinkState("PC unreachable"),LinkState("Pairing lost"),LinkState("Connecting…"))) {
            val result=LinkProjection.aggregate(prefs,mapOf(a.pin to first,b.pin to LinkState("Connected","Tailscale")),null)
            assertEquals("Connected",result.status);assertEquals("Tailscale",result.route)
        }
    }
    @Test fun terminalRetentionKeepsFiftyDistinctArtifactsRegardlessOfRecipientCount()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val queue=room.queue()
            repeat(55) {i->repeat(16) {peer->queue.insert(Outgoing().apply {mediaId="item$i";transferId="transfer$i";peerId="peer$peer";state="sent";orderId=i.toLong()})}}
            assertEquals((0..4).map {"item$it"}.toSet(),queue.evictedTerminal().map {it.mediaId}.toSet())
            assertEquals(50,queue.recent().first().size)
            assertEquals(0,queue.waitingCount().first())
        }finally {room.close()}
    }
    @Test fun addingAndRepairingSecondaryComputerAdvancesScannerInstallationFloor()=runBlocking<Unit>(Dispatchers.IO) {
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val file=File(context.cacheDir,"review-${randomId()}.preferences_pb")
        val store=PreferenceDataStoreFactory.create(scope=owner) {file}
        try {
            store.edit {it[stringPreferencesKey("deviceId")]=prefs.deviceId;it[longPreferencesKey("pairingRevision")]=1;PeerRegistry.write(it,listOf(a))}
            val preferences=Preferences(context,store)
            val before=preferences.read()
            val invite=Invitation(1,"B",b.pin,listOf(Endpoint.parse("127.0.0.1:5338")),"1".repeat(64),9999999999)
            assertTrue(preferences.paired(invite,b.deviceId))
            val added=preferences.read()
            assertTrue("Scanner revision must observe addition",added.pairingRevision>before.pairingRevision)
            assertTrue(preferences.paired(invite,b.deviceId,replacement=ConfirmedBinding.from(added,b.deviceId)))
            assertEquals("Idempotent repair retains generation",added.pairingRevision,preferences.read().pairingRevision)
            preferences.finishPeerReset(b.deviceId)
            val repaired=preferences.read()
            assertTrue(preferences.connected(DeliveryBinding.from(repaired,b.deviceId)!!,Endpoint.parse("127.0.0.1:5338"),200))
            assertTrue(preferences.connected(DeliveryBinding.from(repaired,a.deviceId)!!,Endpoint.parse("127.0.0.1:5338"),100))
            assertEquals(200L,preferences.read().lastConnected)
        }finally {owner.cancel();owner.coroutineContext[Job]?.join();file.delete()}
    }
    @Test fun pairingUsesPairWordingAndNeverPromisesToReplaceAnotherDevice()=runBlocking<Unit>(Dispatchers.IO) {
        assertEquals("Pair",context.getString(R.string.action_pair))
        assertEquals("Pair with B?",context.getString(R.string.pair_confirm_title,"B"))
        assertEquals(a.generation,ConfirmedBinding.from(prefs).revision)
        assertEquals(b.generation,ConfirmedBinding.from(prefs,b.deviceId).revision)
        assertTrue(ConfirmedBinding.from(prefs,b.deviceId).matches(prefs.copy(pairingRevision=99)))
    }
    @Test fun scannerTracksInstalledBindingForFirstAdditionalAndRepairedDevices()=runBlocking<Unit>(Dispatchers.IO) {
        val first=prefs.copy(peer=a,peers=listOf(a),pairingRevision=1)
        assertEquals(a.deviceId,first.installedAfter(0)?.pcId)
        assertNull(first.installedAfter(1))
        assertEquals(b.deviceId,prefs.installedAfter(1)?.pcId)
        assertEquals(b.deviceId,prefs.copy(peers=listOf(a,b.copy(generation=3))).installedAfter(2)?.pcId)
        assertNull(prefs.copy(peers=listOf(a,b.copy(pairingLost=true))).installedAfter(1))
    }
    @Test fun survivingCapturedTargetsDoNotDependOnTheRemovedAnchorOrGainNewRecipients()=runBlocking<Unit>(Dispatchers.IO) {
        val captured=prefs.devices.map {DeliveryBinding.from(prefs,it.deviceId)!!}
        val after=prefs.copy(peer=b,peers=listOf(b,b.copy(name="C",pin="c",deviceId="c")))
        assertEquals(listOf(captured[1]),RecipientSelection.surviving(after,captured))
        assertTrue(RecipientSelection.hasAuthority(after,captured))
        assertFalse(RecipientSelection.hasAuthority(after,emptyList()))
        assertFalse(RecipientSelection.hasAuthority(prefs,emptyList()))
        assertEquals(listOf(captured[1]),RecipientSelection.surviving(prefs.copy(peers=listOf(a.copy(generation=9),b)),captured))
        assertTrue(RecipientSelection.surviving(prefs.copy(deviceId="other-phone"),captured).isEmpty())
        assertTrue(RecipientSelection.surviving(prefs.copy(peers=listOf(a.copy(pairingLost=true),b.copy(generation=3))),captured).isEmpty())
    }
    @Test fun emptyRecipientSetsHaveNoAuthorityDespiteAValidDefaultComputer()=runBlocking<Unit>(Dispatchers.IO) {
        assertTrue(requireNotNull(DeliveryBinding.from(prefs)).matches(prefs))
        for(kind in listOf("text","clipboard_image","file")) {
            val empty=RecipientSelection.freeze(prefs.copy(queue=false),emptySet(),kind)
            assertTrue(empty.isEmpty())
            assertFalse(RecipientSelection.hasAuthority(prefs,empty))
        }
        val disabled=prefs.copy(peers=prefs.devices.map {it.copy(clipboardSend=false)})
        assertFalse(RecipientSelection.hasAuthority(disabled,RecipientSelection.freeze(disabled,setOf(a.deviceId,b.deviceId),"text")))
    }
    @Test fun recentActionsCaptureTheWholeOperationAndRejectEveryStaleDelivery()=runBlocking<Unit>(Dispatchers.IO) {
        fun row(peer:Peer,state:String)=Outgoing().apply {mediaId="artifact";transferId="transfer";peerId=peer.deviceId;bindingGeneration=peer.generation;this.state=state}
        val first=row(a,"failed");val second=row(b,"sending");val other=row(a,"waiting").apply {mediaId="other"}
        val selected=ConfirmedQueue.artifact(prefs,ConfirmedItem.from(first),listOf(first,second,other))
        assertEquals(setOf(a.deviceId,b.deviceId),selected.items.map {it.peerId}.toSet())
        assertEquals(listOf(first,second),selected.resolve(listOf(first,second,other)))
        assertTrue(selected.matches(prefs))
        second.revision++;assertNull(selected.resolve(listOf(first,second)));second.revision--
        second.state="sent";assertNull(selected.resolve(listOf(first,second)));second.state="sending"
        second.transferId="new";assertNull(selected.resolve(listOf(first,second)));second.transferId="transfer"
        assertNull(selected.resolve(listOf(first)))
        assertFalse(selected.matches(prefs.copy(peers=listOf(a,b.copy(generation=3)))))
        assertFalse(selected.matches(prefs.copy(deviceId="other-phone")))
        assertNull(ConfirmedQueue.artifact(prefs,ConfirmedItem.from(first),emptyList()).resolve(listOf(first,second)))
        val onlyB=ConfirmedQueue.artifact(prefs,ConfirmedItem.from(second),listOf(second))
        assertTrue("Unrelated anchor removal must not block B",onlyB.matches(prefs.copy(peer=b,peers=listOf(b))))
    }
    @Test fun recentPagingAndCapacityCountsUseOperationsAndKeepActiveSiblingsVisible()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val queue=room.queue()
            repeat(60) {i->repeat(16) {peer->queue.insert(Outgoing().apply {mediaId="item$i";transferId="transfer$i";peerId="peer$peer";state=if(i<10)"waiting" else "sent";orderId=i.toLong()})}}
            queue.update(queue.delivery("item0","peer0").apply {state="sent"})
            assertEquals(10,queue.pendingCount());assertEquals(10,queue.waitingCount().first())
            val recent=queue.recent().first()
            assertEquals(50,recent.size);assertEquals(50,recent.map {it.mediaId}.toSet().size)
            assertTrue(recent.any {it.mediaId=="item0" && it.state=="waiting"})
            assertTrue(queue.evictedTerminal().none {it.mediaId=="item0"})
        }finally {room.close()}
    }

    @Test fun connectionDetailNamesBothConnectedComputersAndCountsPartialConnectivity()=runBlocking<Unit>(Dispatchers.IO) {
        val connected=LinkState("Connected","Wi-Fi")
        assertEquals("1 of 2 devices",UiPresentation.connectionDetail(context,prefs,mapOf(b.deviceId to connected),connected))
        assertEquals("A and B",UiPresentation.connectionDetail(context,prefs,mapOf(a.deviceId to connected,b.deviceId to connected),connected))
        assertEquals("2 devices",UiPresentation.connectionDetail(context,prefs,emptyMap(),LinkState("Connecting…")))
        assertEquals("A · Wi-Fi",UiPresentation.connectionDetail(context,prefs.copy(peers=listOf(a)),mapOf(a.deviceId to connected),connected))
    }

    @Test fun settingsNamesEachPairsClipboardSwitchAndRetryCaptureCannotAcquireANewBinding()=runBlocking<Unit>(Dispatchers.IO) {
        assertEquals("Receive clipboard from A",UiPresentation.clipboardReceiveLabel(context,prefs,a))
        assertEquals("Receive clipboard from B",UiPresentation.clipboardReceiveLabel(context,prefs,b))
        assertEquals("Receive clipboard from computer",UiPresentation.clipboardReceiveLabel(context,prefs.copy(peers=listOf(a)),a))
        val confirmed=prefs.devices.map {ConfirmedBinding.from(prefs,it.deviceId)}
        assertTrue(confirmed.all {it.matches(prefs)})
        assertFalse(confirmed.all {it.matches(prefs.copy(peers=listOf(a,b.copy(generation=3))))})
        assertEquals(listOf(a.pin,b.pin),confirmed.map {it.pin})
    }

    @Test fun recentOpenKeepsSuccessfulArtifactVisibleWhenAnotherDeliveryWasDiscarded()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val queue=room.queue()
            listOf("a" to "discarded","b" to "sent").forEach {(peer,state)->queue.insert(Outgoing().apply {mediaId="file";transferId="transfer";peerId=peer;this.state=state;uri="content://media/source";automatic=true})}
            val row=queue.recent().first().single()
            assertEquals("sent",row.state);assertEquals("content://media/source",row.uri)
            assertEquals("sent",queue.find("file").state)
            queue.update(queue.delivery("file","a").apply {state="waiting"})
            assertEquals("waiting",queue.recent().first().single().state)
            queue.update(queue.delivery("file","b").apply {state="failed"})
            assertEquals("failed",queue.recent().first().single().state)
        }finally {room.close()}
    }

    @Test fun retryingAnAlreadyRemovedSecondaryBindingRetainsItsIdentityForCleanup()=runBlocking<Unit>(Dispatchers.IO) {
        val after=prefs.copy(peer=a,peers=listOf(a),resetPeers=setOf(b.deviceId),pairingRevision=4)
        val cleanup=ConfirmedBinding.from(after,b.deviceId)
        assertNull(cleanup.pin);assertEquals(b.deviceId,cleanup.pcId);assertTrue(cleanup.matches(after))
        assertFalse(cleanup.matches(after.copy(peers=listOf(a,b))))
        assertFalse(cleanup.matches(after.copy(deviceId="new-phone")))
        assertFalse(cleanup.matches(after.copy(pairingRevision=5)))
        assertNotEquals(ConfirmedBinding.from(after),cleanup)
    }

    @Test fun unpairWarningCountsOnlyTheNamedBindingsDistinctOperations()=runBlocking<Unit>(Dispatchers.IO) {
        fun row(peer:Peer,id:String)=Outgoing().apply {mediaId=id;peerId=peer.deviceId;bindingGeneration=peer.generation}
        val rows=listOf(row(a,"shared"),row(b,"shared"),row(a,"only-a"),row(b,"shared"),row(b,"terminal").apply {state="sent"},row(b.copy(generation=1),"old-b"))
        assertEquals(1,ConfirmedBinding.from(prefs,b.deviceId).pendingCount(rows))
        assertEquals(2,ConfirmedBinding.from(prefs,a.deviceId).pendingCount(rows))
        assertTrue(BindingTransitions.hasWork(prefs.copy(peers=listOf(a))))
        assertFalse(BindingTransitions.hasWork(prefs.copy(peer=null,peers=emptyList())))
    }

    @Test fun quarantinedPairingKeepsRecoveryFeedbackWithoutAdvertisingAConnectedRecipient()=runBlocking<Unit>(Dispatchers.IO) {
        val p=prefs.copy(peer=a,peers=listOf(a),resetPeers=setOf(a.deviceId))
        val failed=LinkState("PC unreachable",pairing="Pairing…")
        assertEquals("Pairing…",LinkProjection.aggregate(p,mapOf(a.pin to failed),null).pairing)
        val stale=LinkState("Connected","Wi-Fi",sent=1,total=2,transferMediaId="old")
        val result=LinkProjection.aggregate(p,mapOf(a.pin to stale),null)
        assertEquals("PC unreachable",result.status);assertEquals("",result.route);assertEquals("",result.transferMediaId)
        val healthy=LinkProjection.aggregate(p.copy(peers=listOf(a,b)),mapOf(a.pin to stale,b.pin to LinkState("Connected","Tailscale")),null)
        assertEquals("Connected",healthy.status);assertEquals("Tailscale",healthy.route)
    }

}
