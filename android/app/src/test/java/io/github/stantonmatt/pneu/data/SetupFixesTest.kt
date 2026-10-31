package io.github.stantonmatt.pneu.data

import android.app.Application
import androidx.datastore.preferences.core.*
import androidx.room.Room
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.removeSetupMember
import io.github.stantonmatt.pneu.withdrawSetup
import io.github.stantonmatt.pneu.receiveSetupMembers
import io.github.stantonmatt.pneu.connection.PairingRejection
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.test.StandardTestDispatcher
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.File
import java.lang.reflect.Proxy

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class SetupFixesTest {
    private val a="1".repeat(32);private val b="2".repeat(32);private val c="3".repeat(32)
    private val s="0".repeat(32);private val t="f".repeat(32)
    private val self=SetupIdentity(a,"a".repeat(64),"Phone")
    private val peer=Peer("PC","b".repeat(64),b,emptyList(),generation=7,setupDecision=SetupScope.ENABLED)
    private val binding=DeliveryBinding(a,peer.pin,b,7)
    private val own=SetupMember(a,self.pin,"phone","Phone","member","10",a)
    private val remote=SetupMember(b,peer.pin,"computer","PC","member","11",b)
    private val candidate=SetupMember(c,"c".repeat(64),"computer","Laptop","member","12",b)
    private val initial=SetupState(s,1,listOf(own,remote))
    private val now=1800000000100L
    private val context get()=RuntimeEnvironment.getApplication()
    private class Store(val preferences:Preferences,val queue:QueueDao,val reopen:suspend ()->Preferences)
    private suspend fun fixture(state:SetupState=initial,peers:List<Peer> = listOf(peer),body:suspend (Store)->Unit) {
        val file=File(context.cacheDir,"setup-fix-${randomId()}.preferences_pb")
        var owner=SupervisorJob()
        fun open()=Preferences(context,PreferenceDataStoreFactory.create(scope=CoroutineScope(owner+Dispatchers.IO)) {file})
        val prefs=open();val database=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            prefs.edit {p->p[stringPreferencesKey("deviceId")]=a;p[longPreferencesKey("pairingRevision")]=7
                PeerRegistry.write(p,peers);SetupStorage.write(p,state)}
            body(Store(prefs,database.queue()) {owner.cancelAndJoin();owner=SupervisorJob();open()})
        }finally {owner.cancelAndJoin();database.close();file.delete()}
    }
    private suspend fun hello(p:Preferences,remote:SetupHello?):SetupHelloResult {
        val current=p.read()
        return p.setupHello(binding,current.membership.generation,current.membership.hello(current.devices.single {it.deviceId==b}.setupScope),remote,self)
    }
    private suspend fun receive(p:Preferences,q:QueueDao,snapshot:MembersSnapshot)=
        p.receiveMembers(binding,1,s,snapshot.json(),self,null,q,now)
    private suspend fun finish(p:Preferences,q:QueueDao) {
        p.read().resetPeers.forEach {q.clearPeer(it);p.finishPeerReset(it)}
        p.finishMembersIntents(q)
    }
    @Test fun answerPersistsAllowUntilRefreshedHelloAndRejectsChangedSetup()=runBlocking<Unit>(Dispatchers.IO) {
        for(unavailable in listOf(true,false))fixture(peers=listOf(peer.copy(setupDecision=SetupScope.ASK))) {f->
            var p=f.preferences
            hello(p,SetupHello(SetupScope.ENABLED,s))
            p.answerSetupPrompt(p.read().membership.prompts.single(),true)
            assertEquals(SetupScope.ASK,p.read().devices.single().setupScope)
            assertTrue(p.read().membership.prompts.single().allowRequested)
            p=f.reopen()
            assertEquals(SetupScope.ASK,p.read().devices.single().setupScope)
            if(unavailable) {
                assertEquals(SetupHelloResult.DIRECT,hello(p,null))
                assertEquals(SetupScope.ASK,p.read().devices.single().setupScope)
                assertTrue(p.read().membership.prompts.single().allowRequested)
                assertEquals(SetupHelloResult.RECONNECT,hello(p,SetupHello(SetupScope.ENABLED,s)))
                assertEquals(SetupScope.ENABLED,p.read().devices.single().setupScope)
            } else {
                assertEquals(SetupHelloResult.SEPARATE,hello(p,SetupHello(SetupScope.ENABLED,t)))
                assertEquals(SetupScope.DISABLED,p.read().devices.single().setupScope)
                assertTrue(p.read().membership.prompts.isEmpty())
                assertEquals("Direct link kept; separate setups not linked",p.read().membership.linkOutcome(b))
            }
            assertEquals(initial.roster,p.read().membership.roster)
        }
    }
    @Test fun retiredDenyBlocksIntroductionWithoutExportingTombstoneIntoJoinedSetup()=runBlocking<Unit>(Dispatchers.IO) {
        fixture(initial.copy(roster=initial.roster+candidate)) {f->
            val p=f.preferences
            p.edit {SetupTransactions.remove(it,candidate.key,1,now);SetupTransactions.remove(it,remote.key,1,now)}
            assertNull(p.read().membership.setupId);assertTrue(p.read().membership.denies(c,candidate.fingerprint))
            p.edit {data->
                SetupTransactions.paired(data,SetupStorage.read(data),peer,null,true,true,now=now)
                PeerRegistry.write(data,listOf(peer));PeerRegistry.finishReset(data,b)
            }
            assertEquals(SetupHelloResult.RECONNECT,hello(p,SetupHello(SetupScope.ENABLED,t)))
            val generation=p.read().membership.generation
            p.receiveMembers(binding,generation,t,MembersSnapshot("4".repeat(32),t,listOf(own,remote,candidate)).json(),self,null,f.queue,now)
            assertTrue(p.read().membership.denies(c,candidate.fingerprint))
            assertTrue(p.read().devices.none {it.deviceId==c});assertTrue(p.read().membership.roster.none {it.deviceId==c})
            val first=p.prepareMembers(binding,generation,t)
            val exported=SetupWire.decode(first)
            assertTrue(exported.members.none {it.deviceId==c});assertEquals(t,exported.setupId)
            p.acknowledgeMembers(binding,generation,t,first);finish(p,f.queue)
            val retired=p.read().membership.barriers.getValue(candidate.key)
            val currentRemoval=candidate.copy(status="removed",hlc="20")
            p.receiveMembers(binding,generation,t,MembersSnapshot("5".repeat(32),t,listOf(own,remote,currentRemoval)).json(),self,null,f.queue,now)
            assertEquals(retired,p.read().membership.barriers.getValue(candidate.key))
            assertEquals(currentRemoval,p.read().membership.roster.single {it.deviceId==c})
            assertEquals(currentRemoval,SetupWire.decode(p.prepareMembers(binding,generation,t)).members.single {it.deviceId==c})
        }
    }
    @Test fun plainForgetClearsDiscoveryMetadataSoAnotherLinkFoundsAcrossRestart()=runBlocking<Unit>(Dispatchers.IO) {
        for(restart in listOf(false,true))fixture(SetupState(),listOf(peer.copy(setupDecision=SetupScope.ASK),
            peer.copy(deviceId=c,pin=candidate.fingerprint,generation=8,setupDecision=SetupScope.ASK))) {f->
            var p=f.preferences
            p.requestSetupLink(ConfirmedBinding.from(p.read(),b))
            val pending=p.read().membership.pendingLink!!
            p.setupHello(binding,0,SetupHello(SetupScope.ENABLED),SetupHello(SetupScope.ASK,s),self,pending.id)
            assertEquals(1,p.read().membership.pendingLink!!.advertised.size)
            p.edit {data->SetupStorage.write(data,SetupStorage.read(data).copy(
                prompts=listOf(SetupPrompt(b,peer.pin,7,0,peer.name)),separateLinks=setOf(b)))}
            p.forget(peerId=b)
            val forgotten=p.read().membership
            assertTrue(forgotten.prompts.isEmpty());assertTrue(forgotten.separateLinks.isEmpty())
            assertTrue(forgotten.pendingLink!!.advertised.isEmpty())
            assertEquals(listOf(c),forgotten.pendingLink.bindings.map {it.peerId})
            if(restart)p=f.reopen()
            p.requestSetupLink(ConfirmedBinding.from(p.read(),c))
            val next=p.read().membership.pendingLink!!
            p.edit {assertTrue(SetupTransactions.finishLinkDiscovery(it,next.id,self,now,{t}))}
            assertEquals(t,p.read().membership.setupId)
        }
    }
    @Test fun unfinishedMembersReceiptReservesSharedIdAndBoundsRecoveryJournal()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {f->
            val p=f.preferences;val q=f.queue
            val snapshot=MembersSnapshot("4".repeat(32),s,listOf(own,remote,candidate))
            receive(p,q,snapshot)
            assertNotNull("MEMBERS must reserve the shared ID before committing",q.receipt(b,snapshot.transferId))
            assertEquals("pending",q.receipt(b,snapshot.transferId)!!.state)
            val failing=Proxy.newProxyInstance(QueueDao::class.java.classLoader,arrayOf(QueueDao::class.java)) {_,method,args->
                if(method.name=="updateReceipt")throw java.io.IOException("Room completion unavailable")
                method.invoke(q,*(args ?: emptyArray()))
            } as QueueDao
            try {p.finishMembersIntents(failing);fail("Expected Room failure")}
            catch(e:java.lang.reflect.UndeclaredThrowableException) {assertTrue(generateSequence<Throwable>(e) {it.cause}.any {it is java.io.IOException && it.message=="Room completion unavailable"})}
            val settings=p.prepareSettings(binding).deepCopy().apply {addProperty("transfer_id",snapshot.transferId)}
            try {p.receiveSettings(binding,settings,q);fail("MEMBERS owns shared ID")}catch(_:Preferences.SettingsConflict) {}
            repeat(3) {n->
                try {receive(p,q,snapshot.copy(transferId=(n+5).toString().repeat(32)));fail("Recovery must finish first")}
                catch(_:IllegalStateException) {}
            }
            assertEquals(1,p.read().membership.receiptIntents.size);assertEquals(1,q.receiptCount())
            finish(p,q)
            assertEquals("members",q.receipt(b,snapshot.transferId)!!.origin)
            assertEquals("applied",q.receipt(b,snapshot.transferId)!!.state)
            assertTrue(receive(p,q,snapshot).first);assertTrue(p.read().membership.receiptIntents.isEmpty())
        }
    }
    @Test fun membershipReceiptHistoryUsesBoundedHashesAndStillDetectsConflicts()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {f->
            val history=(4..60).map {n->candidate.copy(deviceId=n.toString(16).padStart(32,'0'),
                fingerprint=n.toString(16).padStart(64,'0'),name="x".repeat(128),status="removed",reach=null)}
            val snapshot=MembersSnapshot("4".repeat(32),s,listOf(own,remote)+history)
            repeat(96) {n->receive(f.preferences,f.queue,snapshot.copy(transferId=n.toString(16).padStart(32,'0')));finish(f.preferences,f.queue)}
            val state=f.preferences.read().membership
            assertEquals(96,state.receipts.size)
            val bytes=com.google.gson.Gson().toJson(state.receipts).toByteArray().size
            assertTrue("Receipt bytes: $bytes",bytes<=1024*1024)
            assertFalse(com.google.gson.Gson().toJson(state.receipts).contains("x".repeat(128)))
            val last=snapshot.copy(transferId=95.toString(16).padStart(32,'0'))
            assertTrue(receive(f.preferences,f.queue,last.copy(members=last.members.reversed())).first)
            try {receive(f.preferences,f.queue,last.copy(members=listOf(own,remote.copy(name="Changed"))+history));fail("Expected conflict")}
            catch(_:SetupReceiptConflict) {}
        }
    }
    @Test fun directUnpairConfirmationCannotBecomeGlobalRemoval()=runBlocking<Unit>(Dispatchers.IO) {
        fixture(initial.copy(roster=listOf(own))) {f->
            val p=f.preferences;val confirmed=ConfirmedBinding.from(p.read(),b)
            receive(p,f.queue,MembersSnapshot("4".repeat(32),s,listOf(own,remote)));finish(p,f.queue)
            val graph=AppGraph(context,p,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
            try {
                val before=p.read();var notified=false
                assertFalse(graph.forgetBinding(confirmed=confirmed,beforeRemoval={notified=true}))
                assertFalse(notified);assertEquals(before,p.read())
                assertTrue(graph.removeSetupMember(ConfirmedSetupMember.from(before,remote.key)))
                assertTrue(p.read().membership.denies(b,peer.pin));assertTrue(p.read().devices.isEmpty())
            }finally {graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")}
        }
    }
    @Test fun removalStillSucceedsWhenJournaledCleanupFailsAndRecoveryCanRetry()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {f->
            val graph=AppGraph(context,f.preferences,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
            try {
                val before=f.preferences.read()
                // Fail the first DAO operation in post-commit cleanup; production recovery uses the real DAO.
                val queueField=AppGraph::class.java.getDeclaredField("queue").apply {isAccessible=true}
                queueField.set(graph,Proxy.newProxyInstance(QueueDao::class.java.classLoader,arrayOf(QueueDao::class.java)) {_,_,_->
                    throw java.io.IOException("Injected cleanup failure")
                })
                assertTrue(graph.forgetBinding(confirmed=ConfirmedBinding.from(before,b),setupMember=ConfirmedSetupMember.from(before,remote.key)))
                val after=f.preferences.read()
                assertTrue(after.membership.denies(b,peer.pin));assertTrue(after.devices.isEmpty())
                assertTrue(b in after.resetPeers)
                val recovered=AppGraph(context,f.preferences,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
                try {
                    recovered.completePeerReset(b);recovered.completePeerReset(b)
                    assertFalse(b in f.preferences.read().resetPeers)
                }finally {recovered.scope.cancel();recovered.database.close()}
            }finally {graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")}
        }
    }
    @Test fun attemptRechecksPhoneOnlyCapacityAfterConfirmation()=runBlocking<Unit>(Dispatchers.IO) {
        fixture(SetupState(),emptyList()) {f->
            val p=f.preferences
            val confirmed=ConfirmedBinding.from(p.read())
            p.edit {SetupStorage.write(it,initial.copy(roster=(1..17).map {n->own.copy(deviceId="%032x".format(n),fingerprint="%064x".format(n))}))}
            assertTrue(confirmed.matches(p.read())) // A roster-only addition does not change pairingRevision.
            val graph=AppGraph(context,p,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
            try {
                val uri="pneu://pair?v=1&name=PC&fp=${peer.pin}&addr=127.0.0.1%3A1&token=${"e".repeat(64)}&exp=9999999999999"
                assertTrue(graph.requestPairing(uri,confirmed,true))
                assertFalse(graph.connection.attempt.value!!.setupEnabled)
            }finally {graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")}
        }
    }
    @Test fun tokenPairingCannotBindPinOwnedByRetainedPhoneMember()=runBlocking<Unit>(Dispatchers.IO) {
        val phone=candidate.copy(role="phone")
        fixture(initial.copy(roster=initial.roster+phone)) {f->
            val p=f.preferences;val before=p.read();var installed=false;val d="4".repeat(32)
            val invitation=Invitation(1,"New computer",phone.fingerprint,emptyList(),"e".repeat(64),now)
            try {
                p.paired(invitation,d,setupFence=before.membership,beforeInstall={data,newBinding->
                    SetupTransactions.tokenHello(data,newBinding,SetupHello(SetupScope.ENABLED,s),self,now)
                },onInstalled={installed=true})
                fail("Conflicting retained identity must be rejected")
            }catch(_:IllegalArgumentException) {}
            assertFalse(installed);assertEquals(before,p.read());assertEquals(0,f.queue.receiptCount())
            assertEquals(1,p.read().membership.roster.count {it.active && it.fingerprint==phone.fingerprint})
        }
    }
    @Test fun enabledLegacyLinkStillLosesBindingWhileNegotiatedAndIntroducedLinksKeepTrust()=runBlocking<Unit>(Dispatchers.IO) {
        for(kind in listOf("legacy","negotiated","introduced"))fixture(SetupState(),listOf(peer.copy(
            addedViaId=if(kind=="introduced")c else null))) {f->
            val p=f.preferences
            p.hello(binding,"PC",emptyList(),mesh=true,setup=kind=="negotiated")
            val current=p.read().devices.single()
            assertEquals(kind=="legacy",PairingRejection.unauthorized("unauthorized",true,current.setupScoped))
            val graph=AppGraph(context,p,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
            try {
                assertEquals(kind=="legacy",graph.loseBinding(current,a))
                assertEquals(kind=="legacy",p.read().devices.single().pairingLost)
                assertTrue(p.read().membership.barriers.isEmpty())
            }finally {graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")}
        }
    }
    @Test fun dialReconciliationCancelsStaleAttemptButFreshConfirmationClearsRetainedRemoval()=runBlocking<Unit>(Dispatchers.IO) {
        val keeper=peer.copy(deviceId=c,pin=candidate.fingerprint,generation=8,setupDecision=SetupScope.DISABLED)
        for(fresh in listOf(false,true))for(resolved in listOf(false,true))
            fixture(initial.copy(roster=initial.roster+candidate),listOf(peer,keeper)) {f->
                val p=f.preferences
                val graph=AppGraph(context,p,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
                val uri="pneu://pair?v=1&name=PC&fp=${peer.pin}&addr=192.0.2.1:47821&token=${"e".repeat(64)}&exp=$now"
                val invitation=Invitation.parse(uri)
                suspend fun confirm() {
                    assertTrue(graph.requestPairing(uri,ConfirmedBinding.forInvitation(p.read(),invitation)))
                    if(resolved)assertNotNull(graph.pairingHello(requireNotNull(graph.connection.attempt.value),b))
                }
                try {
                    if(!fresh)confirm()
                    p.edit {SetupTransactions.remove(it,remote.key,1,now)}
                    val barrier=p.read().membership.barriers.getValue(remote.key)
                    if(fresh)confirm()
                    graph.reconcileDelivery()
                    if(fresh) {
                        assertNotNull("Fresh confirmation must survive dial reconciliation",graph.connection.attempt.value)
                        assertTrue(graph.installBinding(invitation,b))
                        assertEquals(barrier.copy(cleared=true),p.read().membership.barriers.getValue(remote.key))
                        assertEquals(peer.pin,p.read().devices.single {it.deviceId==b}.pin)
                    } else {
                        assertNull("Pre-removal attempt must be cancelled",graph.connection.attempt.value)
                        assertFalse(graph.installBinding(invitation,b))
                        assertEquals(barrier,p.read().membership.barriers.getValue(remote.key))
                    }
                }finally {graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")}
            }
    }
    @Test fun withdrawalPreservesEnabledLegacyBindingSettingsAndQueueInBothPaths()=runBlocking<Unit>(Dispatchers.IO) {
        val d="4".repeat(32)
        val negotiated=peer.copy(setupSupported=true)
        val introduced=peer.copy(deviceId=c,pin=candidate.fingerprint,generation=8,
            setupDecision=SetupScope.ASK,addedViaId=b,addedViaName=peer.name)
        val legacy=peer.copy(deviceId=d,pin="d".repeat(64),generation=9)
        for(ownTombstone in listOf(false,true))fixture(initial.copy(roster=initial.roster+candidate),listOf(negotiated,introduced,legacy)) {f->
            val p=f.preferences
            p.hello(DeliveryBinding(a,legacy.pin,d,9),"Legacy PC",emptyList(),mesh=true,setup=false)
            p.setPairSwitch(d,"receiveClipboard",false)
            p.prepareSettings(DeliveryBinding(a,legacy.pin,d,9))
            val retained=p.read().devices.single {it.deviceId==d}
            assertEquals(SetupScope.ENABLED,retained.setupScope);assertFalse(retained.setupScoped)
            val graph=AppGraph(context,p,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
            try {
                for(target in p.read().devices)graph.queue.insert(Outgoing().apply {
                    mediaId="queued-${target.deviceId}";peerId=target.deviceId;bindingGeneration=target.generation
                    transferId=target.deviceId;kind="file";automatic=false
                })
                if(ownTombstone)graph.receiveSetupMembers(binding,1,s,MembersSnapshot("5".repeat(32),s,
                    listOf(own.copy(status="removed",hlc="40"),remote,candidate)).json(),self) {true}
                else graph.withdrawSetup(binding,1) {true}
                val after=p.read()
                assertEquals(listOf(retained),after.devices)
                assertNotNull(graph.queue.delivery("queued-$d",d))
                for(id in listOf(b,c))assertNull(graph.queue.delivery("queued-$id",id))
                assertTrue(after.resetPeers.isEmpty());assertTrue(after.membership.withdrawn)
                assertNull(after.membership.setupId);assertTrue(after.membership.roster.isEmpty())
                assertEquals(if(ownTombstone)setOf(own.key) else emptySet<String>(),after.membership.barriers.keys)
            }finally {graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")}
        }
    }
}
