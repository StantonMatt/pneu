package io.github.stantonmatt.pneu.data

import android.app.Application
import androidx.datastore.preferences.core.*
import androidx.room.Room
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.features.QueueAdmission
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.test.StandardTestDispatcher
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
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ReinstallReplacementTest {
    private val phone="1".repeat(32);private val a="2".repeat(32);private val b="3".repeat(32)
    private val old=Peer("  Straße  ","a".repeat(64),a,emptyList(),generation=1,lastConnected=100,clipboardSend=false,clipboardReceive=true,automaticMedia=false)
    private val fresh=Peer("STRASSE","b".repeat(64),b,emptyList(),generation=2)
    private val binding=DeliveryBinding(phone,fresh.pin,b,2)
    private fun data(peers:List<Peer> = listOf(old,fresh))=mutablePreferencesOf(stringPreferencesKey("deviceId") to phone,longPreferencesKey("pairingRevision") to 2).also {PeerRegistry.write(it,peers)}
    private fun p(data:androidx.datastore.preferences.core.Preferences)=Prefs(deviceId=phone,peers=PeerRegistry.read(data),replacements=ReinstallReplacement.read(data))
    private fun offer(data:MutablePreferences):ReplacementChoice {
        ReinstallReplacement.paired(data,binding,null) {false}
        return ReinstallReplacement.read(data).offers.single()
    }
    @Test fun matchingUsesUnicodeCaseFoldingAndTrimmedWhitespace() {
        assertEquals(ReinstallReplacement.name("Straße"),ReinstallReplacement.name("\u00a0STRASSE\u00a0"))
        assertEquals(ReinstallReplacement.name("Σ"),ReinstallReplacement.name("ς"))
        assertNotEquals(ReinstallReplacement.name("I"),ReinstallReplacement.name("ı"))
    }
    @Test fun replaceCopiesOnlyOrdinaryValuesWithFreshLocalStampsAndPreservesNewIdentity() {
        val data=data();val choice=offer(data)
        ReinstallReplacement.replace(data,choice) {false}
        val after=p(data);val peer=after.devices.single()
        assertEquals(b,peer.deviceId);assertEquals(fresh.pin,peer.pin);assertEquals(2L,peer.generation)
        assertFalse(peer.clipboardSend);assertTrue(peer.clipboardReceive);assertFalse(peer.automaticMedia)
        assertEquals(PairSettings.keys(phone,b).toSet(),peer.localSettingsEdits)
        assertEquals(3,peer.settings.size)
        assertTrue(peer.settings.values.all {it.writer==phone && it.hlc.toBigInteger().signum()>0})
        assertEquals(3,peer.settings.values.map {it.hlc}.distinct().size)
        assertTrue(a in PeerRegistry.resets(data));assertEquals(choice,after.replacements.notificationNotice)
        assertTrue(after.replacements.offers.isEmpty())
        assertEquals(choice,after.replacements.replacement(a,1))
    }
    @Test fun declinePersistsExactPairSuppressionAndKeepsBothPairingsAndSwitches() {
        val data=data();val choice=offer(data);val before=PeerRegistry.read(data)
        ReinstallReplacement.decline(data,choice)
        assertEquals(before,PeerRegistry.read(data));assertTrue(PeerRegistry.resets(data).isEmpty())
        assertTrue(ReinstallReplacement.read(data).replaced.isEmpty())
        // Re-read serialized state, then retry the same local suggestion after restart.
        ReinstallReplacement.paired(data,binding,null) {false}
        assertTrue(ReinstallReplacement.read(data).offers.isEmpty())
        assertTrue(choice.key in ReinstallReplacement.read(data).declined)
        assertThrows(IllegalStateException::class.java) {ReinstallReplacement.replace(data,choice) {false}}
        assertEquals(before,PeerRegistry.read(data))
    }
    @Test fun sameRoleOfflineAndExactIdentitiesAreRequiredAtCommit() {
        for(changed in listOf(old.copy(role="phone"),old.copy(name="Another"))) {
            val data=data(listOf(changed,fresh));ReinstallReplacement.paired(data,binding,null) {false}
            assertTrue(ReinstallReplacement.read(data).offers.isEmpty())
        }
        val online=data();ReinstallReplacement.paired(online,binding,null) {it==a}
        assertTrue(ReinstallReplacement.read(online).offers.isEmpty())
        for(change in listOf("online","pin","generation","name","role","newPin","newGeneration")) {
            val data=data();val choice=offer(data)
            PeerRegistry.write(data,PeerRegistry.read(data).map {peer->when {
                peer.deviceId==a -> when(change) {"pin"->peer.copy(pin="c".repeat(64));"generation"->peer.copy(generation=5);"name"->peer.copy(name="Changed");"role"->peer.copy(role="phone");else->peer}
                change=="newPin" -> peer.copy(pin="d".repeat(64))
                change=="newGeneration" -> peer.copy(generation=9)
                else->peer
            }})
            val before=data.toPreferences()
            assertThrows(IllegalStateException::class.java) {ReinstallReplacement.replace(data,choice) {change=="online"}}
            assertEquals(before,data.toPreferences())
        }
    }
    @Test fun mostRecentlySeenWinsWithDeterministicIdTieBreakAndPairAgainRequiresExactChoice() {
        val other=old.copy(deviceId="4".repeat(32),pin="c".repeat(64),lastConnected=200)
        val data=data(listOf(other,old,fresh));assertEquals(other.deviceId,offer(data).old.pcId)
        val tie=data(listOf(other.copy(lastConnected=100),old,fresh));assertEquals(a,offer(tie).old.pcId)
        val implied=data(listOf(other,old,fresh))
        ReinstallReplacement.paired(implied,binding,ConfirmedBinding.from(p(implied),a)) {false}
        val intent=ReinstallReplacement.read(implied).offers.single()
        assertTrue(intent.implied);assertEquals(3,PeerRegistry.read(implied).size)
        ReinstallReplacement.replace(implied,intent) {false}
        assertTrue(p(implied).devices.none {it.deviceId==a});assertTrue(p(implied).devices.any {it.deviceId==other.deviceId})
        assertTrue(ReinstallReplacement.read(implied).offers.isEmpty())
        val stale=data();ReinstallReplacement.paired(stale,binding,ConfirmedBinding.from(p(stale),a).copy(revision=99)) {false}
        assertEquals(2,PeerRegistry.read(stale).size);assertTrue(ReinstallReplacement.read(stale).replaced.isEmpty())
    }
    @Test fun memberRemovalUsesExistingTombstoneAndDoesNotMergeSetups() {
        val data=data();val self=SetupMember(phone,"f".repeat(64),"phone","Phone","member","1",phone)
        val member=SetupMember(a,old.pin,"computer",old.name,"member","2",phone)
        SetupStorage.write(data,SetupState("e".repeat(32),1,listOf(self,member,SetupMember(b,fresh.pin,"computer",fresh.name,"member","3",phone))))
        ReinstallReplacement.replace(data,offer(data)) {false}
        val state=SetupStorage.read(data)
        assertEquals("e".repeat(32),state.setupId);assertEquals(1L,state.generation)
        assertFalse(state.member(a)?.active==true);assertTrue(state.barriers.getValue(member.key).authored==true)
        assertTrue(state.denies(a,old.pin));assertTrue(state.member(b)!!.active)
    }
    @Test fun interruptedCleanupCancelsOnlyOldRecipientAndRetainsResendablePayload()=runBlocking<Unit>(Dispatchers.IO) {
        val context=RuntimeEnvironment.getApplication();val file=File(context.cacheDir,"replace-${randomId()}.preferences_pb")
        val job=SupervisorJob();val store=PreferenceDataStoreFactory.create(scope=CoroutineScope(job+Dispatchers.IO)) {file}
        val prefs=Preferences(context,store);val db=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build();val queue=db.queue()
        try {
            store.edit {it[stringPreferencesKey("deviceId")]=phone;it[longPreferencesKey("pairingRevision")]=2;PeerRegistry.write(it,listOf(old,fresh))}
            val row=Outgoing().apply {mediaId="payload";transferId="transfer";peerId=a;bindingGeneration=1;fileName="report.pdf";path="/retained/report.pdf";automatic=false;kind="file"}
            queue.insert(row);queue.insertDelivery(Delivery.from(row).apply {peerId="other";bindingGeneration=7})
            var choice:ReplacementChoice?=null
            prefs.edit {choice=offer(it)}
            prefs.beforeMutation={if(it=="replacementStaged")error("Interrupted transaction")}
            assertThrows(IllegalStateException::class.java) {runBlocking {prefs.replaceReinstalled(choice!!) {false}}}
            assertEquals(2,prefs.read().devices.size);assertEquals("waiting",queue.delivery(row.mediaId,a).state)
            prefs.beforeMutation={};prefs.replaceReinstalled(choice!!) {false}
            // Simulate death after DataStore commit and before Room cleanup.
            val restarted=Preferences(context,store).read()
            assertTrue(a in restarted.resetPeers);assertNull(DeliveryBinding.from(restarted,a))
            repeat(2) {queue.cancelReplacedPeer(a,1);queue.clearPeer(a)}
            val cancelled=queue.delivery(row.mediaId,a)
            assertEquals("failed",cancelled.state);assertEquals(ReinstallReplacement.CANCELLED,cancelled.error)
            assertEquals(Long.MAX_VALUE,cancelled.nextAttempt);assertEquals(1L,cancelled.revision)
            assertEquals(a,cancelled.peerId);assertNull(queue.delivery(row.mediaId,b))
            assertEquals("waiting",queue.delivery(row.mediaId,"other").state)
            assertEquals(row.path,cancelled.path)
            val text=Outgoing().apply {mediaId="cancelled-text";transferId="text-transfer";peerId=a;bindingGeneration=1;kind="text";state="failed";error=ReinstallReplacement.CANCELLED;nextAttempt=Long.MAX_VALUE;text="Retained for explicit resend"}
            queue.insert(text)
            queue.supersedePeerText(a);queue.dropPeerClipboard(a,1);queue.dropPeerText(a);queue.recover()
            assertEquals("failed",queue.delivery(text.mediaId,a).state)
            assertEquals(ReinstallReplacement.CANCELLED,queue.delivery(text.mediaId,a).error)
            assertTrue(queue.pending().none {it.error==ReinstallReplacement.CANCELLED})
            assertTrue(queue.pendingPeer(a).isEmpty())
            assertEquals(1,queue.waitingCount().first())
            val confirmed=ConfirmedQueue.from(restarted,listOf(cancelled))
            assertTrue(confirmed.matches(restarted));assertEquals(binding,confirmed.replacementTargets[a])
            assertEquals(choice,ReinstallReplacement.read(store.data.first()).notificationNotice)
        }finally {job.cancelAndJoin();db.close();file.delete()}
    }
    @Test fun replacementRetryOriginatesTextAfterNewerQueuedTextAndEmitsIt()=runBlocking<Unit>(Dispatchers.IO) {
        retryFixture("text") {graph,cancelled,newer,other->
            assertTrue(graph.retry(cancelled.mediaId,ConfirmedQueue.from(graph.preferences.read(),graph.queue.deliveries(cancelled.mediaId))))
            val resend=graph.queue.pendingPeer(b).single()
            assertEquals("Retained for explicit resend",resend.text)
            assertNotEquals(cancelled.mediaId,resend.mediaId)
            assertNotEquals(cancelled.transferId,resend.transferId)
            assertTrue(resend.orderId>newer.orderId)
            assertTrue(resend.createdAt>newer.createdAt)
            assertTrue(resend.hlc.toBigInteger()>newer.hlc.toBigInteger())
            assertEquals(phone,resend.sourceDevice)
            assertEquals("superseded",graph.queue.delivery(newer.mediaId,b).state)
            assertNull(graph.queue.delivery(cancelled.mediaId,a))
            assertOriginalRecipient(graph,cancelled,other)
            val emission=QueuedEmission(resend,binding)
            val capabilities=setOf("clipboard_text")
            assertTrue(graph.beginDelivery(resend,binding,
                eligible={p,current,newerText->EmissionPolicy.eligible(resend,current,p,binding,true,false,capabilities,newerText)},
                begin={emission.begin(it.devices.single {peer->peer.deviceId==b},capabilities)}))
            val bytes=java.io.ByteArrayOutputStream()
            Protocol.write(bytes,Frame.Control(Type.CLIPBOARD,io.github.stantonmatt.pneu.features.Staging.clipboardMetadata(resend)))
            val frame=Protocol.read(java.io.ByteArrayInputStream(bytes.toByteArray())) as Frame.Control
            assertEquals(resend.transferId,frame.body["transfer_id"].asString)
            assertEquals(cancelled.text,frame.body["text"].asString)
            assertEquals("sending",graph.queue.delivery(resend.mediaId,b).state)
        }
    }
    @Test @Config(shadows=[ReplacementDirectorySync::class])
    fun replacementRetryCopiesFilesAndRefreshesClipboardImageStamp()=runBlocking<Unit>(Dispatchers.IO) {
        for(kind in listOf("file","clipboard_image"))retryFixture(kind) {graph,cancelled,newer,other->
            val source=File(cancelled.path)
            val bytes=source.readBytes()
            assertTrue(graph.retry(cancelled.mediaId,ConfirmedQueue.from(graph.preferences.read(),graph.queue.deliveries(cancelled.mediaId))))
            val resend=graph.queue.pendingPeer(b).single {it.kind==kind}
            assertNotEquals(cancelled.mediaId,resend.mediaId);assertNotEquals(cancelled.transferId,resend.transferId)
            assertTrue(resend.orderId>newer.orderId);assertTrue(resend.createdAt>newer.createdAt)
            assertEquals(binding.generation,resend.bindingGeneration)
            assertEquals("waiting",resend.state);assertFalse(resend.automatic);assertFalse(resend.attempted)
            assertNotEquals(cancelled.path,resend.path)
            assertArrayEquals(bytes,File(resend.path).readBytes());assertArrayEquals(bytes,source.readBytes())
            if(kind=="clipboard_image") {
                assertTrue(resend.hlc.toBigInteger()>newer.hlc.toBigInteger());assertEquals(phone,resend.sourceDevice)
            }
            assertOriginalRecipient(graph,cancelled,other)
            val retained=graph.queue.delivery(cancelled.mediaId,other.deviceId).apply {state="sent"}
            assertTrue(graph.staging.completeFile(retained))
            assertFalse(source.exists());assertArrayEquals(bytes,File(resend.path).readBytes())
        }
    }
    @Test @Config(shadows=[ReplacementDirectorySync::class])
    fun replacementRetryOfLastDeliveryReclaimsOldStagingBytes()=runBlocking<Unit>(Dispatchers.IO) {
        assertRetryReleasesPayload(keepTerminalRecipient=false)
    }
    @Test @Config(shadows=[ReplacementDirectorySync::class])
    fun replacementRetryWithOnlyTerminalRecipientsReclaimsOldStagingBytes()=runBlocking<Unit>(Dispatchers.IO) {
        assertRetryReleasesPayload(keepTerminalRecipient=true)
    }
    private suspend fun assertRetryReleasesPayload(keepTerminalRecipient:Boolean) {
        for(kind in listOf("file","clipboard_image"))retryFixture(kind) {graph,cancelled,_,other->
            val dir=File(graph.context.filesDir,"outgoing").apply {mkdirs()}
            val source=File(dir,"retained-${randomId()}.bin")
            check(File(cancelled.path).renameTo(source))
            cancelled.path=source.absolutePath;graph.queue.replacePayload(cancelled)
            if(keepTerminalRecipient)graph.queue.update(graph.queue.delivery(cancelled.mediaId,other.deviceId).apply {state="sent"})
            else graph.queue.deleteDelivery(cancelled.mediaId,other.deviceId)
            val bytes=source.readBytes()
            val usageBefore=dir.listFiles().orEmpty().sumOf {it.length()}
            try {
                assertTrue(graph.retry(cancelled.mediaId,ConfirmedQueue.from(graph.preferences.read(),graph.queue.deliveries(cancelled.mediaId))))
                val resend=graph.queue.pendingPeer(b).single {it.kind==kind}
                assertEquals(usageBefore,dir.listFiles().orEmpty().sumOf {it.length()})
                assertFalse(source.exists());assertArrayEquals(bytes,File(resend.path).readBytes())
                assertTrue(graph.queue.orphanArtifacts().isEmpty())
                if(keepTerminalRecipient)assertEquals("sent",graph.queue.delivery(cancelled.mediaId,other.deviceId).state)
                else assertTrue(graph.queue.deliveries(cancelled.mediaId).isEmpty())
            }finally {source.delete()}
        }
    }
    @Test @Config(shadows=[ReplacementDirectorySync::class])
    fun replacementRetryAtCapacityRetainsCancellationAndCleansUnpublishedCopy()=runBlocking<Unit>(Dispatchers.IO) {
        retryFixture("file") {graph,cancelled,newer,other->
            graph.database.runInTransaction {
                repeat(2046) {index->graph.queue.insert(Outgoing().apply {
                    mediaId="full:$index";transferId=randomId();peerId=b;bindingGeneration=2;kind="file";automatic=false
                })}
            }
            assertEquals(2048,graph.queue.pendingCount())
            val dir=File(graph.context.filesDir,"outgoing")
            val before=dir.listFiles().orEmpty().map {it.name}.toSet()
            try {
                graph.retry(cancelled.mediaId,ConfirmedQueue.from(graph.preferences.read(),graph.queue.deliveries(cancelled.mediaId)))
                fail("Expected queue capacity failure")
            }catch(_:io.github.stantonmatt.pneu.features.LocalCapacityException) {}
            assertEquals(before,dir.listFiles().orEmpty().map {it.name}.toSet())
            assertEquals(ReinstallReplacement.CANCELLED,graph.queue.delivery(cancelled.mediaId,a).error)
            assertEquals("waiting",graph.queue.delivery(newer.mediaId,b).state)
            assertOriginalRecipient(graph,cancelled,other)
            assertEquals(2048,graph.queue.pendingCount())
        }
    }
    private fun assertOriginalRecipient(graph:AppGraph,cancelled:Outgoing,other:Peer) {
        val retained=graph.queue.delivery(cancelled.mediaId,other.deviceId)
        assertEquals("failed",retained.state);assertEquals("offline",retained.error)
        assertEquals(9L,retained.revision);assertEquals(123L,retained.nextAttempt)
        assertEquals(cancelled.transferId,retained.transferId);assertEquals(cancelled.orderId,retained.orderId)
        assertEquals(cancelled.hlc,retained.hlc);assertEquals(cancelled.path,retained.path)
    }
    private suspend fun retryFixture(kind:String,check:suspend (AppGraph,Outgoing,Outgoing,Peer)->Unit) {
        val context=RuntimeEnvironment.getApplication();val file=File(context.cacheDir,"retry-${randomId()}.preferences_pb")
        val job=SupervisorJob();val store=PreferenceDataStoreFactory.create(scope=CoroutineScope(job+Dispatchers.IO)) {file}
        val prefs=Preferences(context,store)
        val graph=AppGraph(context,prefs,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
        val other=Peer("Other","c".repeat(64),"4".repeat(32),emptyList(),generation=3)
        try {
            store.edit {
                it[stringPreferencesKey("deviceId")]=phone;it[longPreferencesKey("pairingRevision")]=3
                PeerRegistry.write(it,listOf(old.copy(clipboardSend=true),fresh,other))
                ReinstallReplacement.replace(it,offer(it)) {false}
            }
            graph.initialized.complete(Unit)
            val cancelled=Outgoing().apply {
                mediaId="cancelled";transferId=randomId();peerId=a;bindingGeneration=1;this.kind=kind
                text="Retained for explicit resend";hlc="1";sourceDevice=phone;createdAt=1;orderId=1
                automatic=false;state="failed";error=ReinstallReplacement.CANCELLED;nextAttempt=Long.MAX_VALUE
            }
            if(kind!="text") {
                cancelled.path=File(context.cacheDir,"retained-${randomId()}.bin").apply {writeText("Retained payload")}.absolutePath
                cancelled.size=File(cancelled.path).length();cancelled.hash=sha256(File(cancelled.path).readBytes())
                cancelled.mime=if(kind=="clipboard_image")"image/png" else "application/pdf"
            }
            graph.queue.insert(cancelled)
            graph.queue.insertDelivery(Delivery.from(cancelled).apply {peerId=other.deviceId;bindingGeneration=3;error="offline";revision=9;nextAttempt=123})
            graph.queue.rememberOutgoing(1,1)
            val (stamp,time)=prefs.originateText(1)
            val newer=Outgoing().apply {
                mediaId="newer";transferId=randomId();this.kind="text";text="Newer queued text";createdAt=time
                hlc=stamp.hlc.toString();sourceDevice=stamp.source;automatic=false;recipients=listOf(binding)
            }
            assertTrue(QueueAdmission.admit(graph.database,newer))
            check(graph,cancelled,newer,other)
        }finally {
            graph.queue.all().map {it.path}.filter {it.isNotEmpty()}.distinct().forEach {File(it).delete()}
            graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")
            job.cancelAndJoin();file.delete()
        }
    }

}

/** Robolectric's ShadowLinux opens paths as regular files, so it cannot open
 * the staging directory. Payload writes still use real FileDescriptor.sync(). */
@org.robolectric.annotation.Implements(android.system.Os::class)
class ReplacementDirectorySync {
    companion object {
        @JvmStatic @org.robolectric.annotation.Implementation
        fun open(path:String,flags:Int,mode:Int):java.io.FileDescriptor {
            check(File(path).isDirectory())
            return java.io.FileDescriptor()
        }
        @JvmStatic @org.robolectric.annotation.Implementation
        fun fsync(fd:java.io.FileDescriptor) {}
        @JvmStatic @org.robolectric.annotation.Implementation
        fun close(fd:java.io.FileDescriptor) {}
    }
}
