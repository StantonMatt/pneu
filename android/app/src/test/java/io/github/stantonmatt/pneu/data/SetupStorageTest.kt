package io.github.stantonmatt.pneu.data

import android.app.Application
import androidx.datastore.preferences.core.*
import androidx.room.Room
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class SetupStorageTest {
    private val a="1".repeat(32);private val b="2".repeat(32);private val c="3".repeat(32)
    private val pa="a".repeat(64);private val pb="b".repeat(64);private val pc="c".repeat(64)
    private val s="0".repeat(32);private val t="f".repeat(32)
    private val now=1800000000100L
    private val self=SetupIdentity(a,pa,"Phone")
    private val sender=Peer("PC",pb,b,listOf(Endpoint.parse("192.168.1.2:47821")),generation=7,setupDecision=SetupScope.ENABLED,setupSupported=true)
    private val binding=DeliveryBinding(a,pb,b,7)
    private val selfRow=SetupMember(a,pa,"phone","Phone","member","10",a)
    private val senderRow=SetupMember(b,pb,"computer","PC","member","11",b)
    private val newRow=SetupMember(c,pc,"computer","Laptop","member","12",b,MemberReach(listOf("192.168.1.3:47821"),"13",c))
    private val initial=SetupState(s,1,listOf(selfRow,senderRow))
    private val join=MembersSnapshot("4".repeat(32),s,listOf(selfRow,senderRow,newRow))
    private val context get()=RuntimeEnvironment.getApplication()
    private suspend fun fixture(state:SetupState=initial,peer:Peer=sender,body:suspend (Preferences,QueueDao)->Unit) {
        val file=File(context.cacheDir,"setup-${randomId()}.preferences_pb")
        val job=SupervisorJob();val owner=CoroutineScope(job+Dispatchers.IO)
        val store=PreferenceDataStoreFactory.create(scope=owner) {file}
        val prefs=Preferences(context,store)
        val db=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            store.edit {p->p[stringPreferencesKey("deviceId")]=a;p[longPreferencesKey("pairingRevision")]=7
                PeerRegistry.write(p,listOf(peer));SetupStorage.write(p,state)}
            body(prefs,db.queue())
        }finally {job.cancelAndJoin();db.close();file.delete()}
    }
    private suspend fun receive(p:Preferences,q:QueueDao,snapshot:MembersSnapshot=join,generation:Long=1)=
        p.receiveMembers(binding,generation,snapshot.setupId,snapshot.json(),self,null,q,now)
    private suspend fun finish(p:Preferences,q:QueueDao) {
        for(id in p.read().resetPeers) {q.clearPeer(id);p.finishPeerReset(id)}
        p.finishMembersIntents(q)
    }
    @Test fun crashBeforeAuthoritativeCommitKeepsStateAndOnlyReservesSharedReceipt()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            val before=p.read()
            for(step in listOf("membersClockStaged","membersPeersStaged","membersResetsStaged","membersReceiptStaged","membersCommittedInEdit")) {
                p.beforeMutation={if(it==step)throw java.io.IOException("Injected pre-commit failure")}
                try {receive(p,q);fail("Expected failure at $step")}catch(_:java.io.IOException) {}
                p.beforeMutation={}
                assertEquals(before,p.read());assertEquals("pending",q.receipt(b,join.transferId)!!.state)
            }
            assertTrue(p.read().membership.notices.isEmpty());assertTrue(p.read().resetPeers.isEmpty())
            assertFalse(receive(p,q).first);finish(p,q)
            assertEquals(8L,p.read().devices.single {it.deviceId==c}.generation)
        }
    }
    @Test fun crashAfterCommitResumesResetAndReceiptIntentsBeforeReplayWithoutReinstallOrNotice()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            p.beforeMutation={if(it=="membersCommitted")throw java.io.IOException("Injected post-commit crash")}
            try {receive(p,q);fail("Expected failure")}catch(_:java.io.IOException) {}
            p.beforeMutation={}
            val committed=p.read();assertEquals(1,committed.membership.receiptIntents.size)
            assertTrue(c in committed.resetPeers);assertEquals(1,committed.membership.notices.size)
            assertEquals("pending",q.receipt(b,join.transferId)!!.state)
            finish(p,q)
            assertTrue(receive(p,q).first)
            val recovered=p.read();assertEquals(committed.devices,recovered.devices)
            assertEquals(committed.membership.notices,recovered.membership.notices)
            assertTrue(recovered.resetPeers.isEmpty());assertTrue(recovered.membership.receiptIntents.isEmpty())
            assertEquals("members",q.receipt(b,join.transferId)!!.origin)
            assertEquals(8L,recovered.devices.single {it.deviceId==c}.generation)
        }
    }
    @Test fun sharedReceiptConflictAndSemanticReplayHaveNoLifecycleEffects()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            receive(p,q);finish(p,q);val installed=p.read()
            val reordered=join.copy(members=join.members.reversed())
            assertTrue(receive(p,q,reordered).first);assertEquals(installed,p.read())
            for(changed in listOf(newRow.copy(name="Changed"),newRow.copy(hlc="30"),newRow.copy(reach=newRow.reach!!.copy(addresses=listOf("192.168.1.4:47821"))))) {
                try {receive(p,q,join.copy(members=listOf(selfRow,senderRow,changed)));fail("Expected ID conflict")}catch(_:SetupReceiptConflict) {}
                assertEquals(installed,p.read())
            }
            val other="9".repeat(32)
            q.insertReceipt(Receipt().apply {peerId=b;transferId=other;origin="file";hash="unrelated";state="applied"})
            try {receive(p,q,join.copy(transferId=other));fail("Expected cross-kind ID conflict")}catch(_:SetupReceiptConflict) {}
            assertEquals(installed,p.read())
        }
    }
    @Test fun retainedAndEvictedReplayCannotResurrectRemovalOrResetUnrelatedDeliveries()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            receive(p,q);finish(p,q)
            q.insert(Outgoing().apply {mediaId="file-for-sender";peerId=b;bindingGeneration=7;transferId="8".repeat(32);kind="file";automatic=false})
            val removal=join.copy(transferId="5".repeat(32),members=listOf(selfRow,senderRow,newRow.copy(status="removed",hlc="30",reach=null)))
            assertFalse(receive(p,q,removal).first);finish(p,q)
            assertTrue(receive(p,q).first)
            val state=p.read().membership
            p.edit {data->SetupStorage.write(data,state.copy(receipts=state.receipts.filterNot {it.transferId==join.transferId}))}
            q.deleteReceipt(b,join.transferId)
            assertFalse(receive(p,q).first);finish(p,q)
            assertTrue(receive(p,q).first)
            assertTrue(p.read().devices.none {it.deviceId==c});assertTrue(p.read().membership.denies(c,pc))
            assertNotNull(q.find("file-for-sender"));assertEquals(7L,p.read().devices.single().generation)
            assertEquals("30",p.read().membership.barriers.getValue(newRow.key).removed.hlc)
        }
    }
    @Test fun pendingSnapshotsReplayExactlyBeforeCoalescedChangesAndPersistAcknowledgedIdentity()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            val first=p.prepareMembers(binding,1,s)
            val settings=p.prepareSettings(binding)
            assertNotEquals(first["transfer_id"].asString,settings["transfer_id"].asString)
            val oldSettings=p.read().devices.single().pendingSettings
            receive(p,q);finish(p,q)
            assertEquals(first,p.prepareMembers(binding,1,s))
            p.acknowledgeMembers(binding,1,s,first)
            assertEquals(oldSettings,p.read().devices.single {it.deviceId==b}.pendingSettings)
            val next=p.prepareMembers(binding,1,s);assertNotEquals(first["transfer_id"],next["transfer_id"])
            assertEquals(3,SetupWire.decode(next).members.size)
            p.acknowledgeMembers(binding,1,s,next)
            assertEquals(next,p.prepareMembers(binding,1,s))
            p.acknowledgeSettings(binding,settings)
            assertEquals(next.toString(),p.read().membership.sends.getValue(b).pending)
        }
    }
    @Test fun idMismatchAndStrayUnassignedSnapshotsNeverObserveClockOrCreateReceiptEvenOnReplay()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            receive(p,q);finish(p,q);val before=p.read()
            try {receive(p,q,join.copy(setupId=t));fail("Expected mismatch")}catch(_:SetupUnavailable) {}
            assertEquals(before,p.read())
            p.edit {data->SetupStorage.write(data,SetupReconcile.retire(SetupStorage.read(data),false))}
            val retired=p.read()
            try {receive(p,q);fail("Expected unassigned rejection")}catch(_:SetupUnavailable) {}
            assertEquals(retired,p.read())
            try {p.prepareMembers(binding,1,s);fail("Expected stale send rejection")}catch(_:SetupUnavailable) {}
        }
    }
    @Test fun migrationAllowDenyAndExplicitLinkPersistWithoutResettingPairState()=runBlocking<Unit>(Dispatchers.IO) {
        fixture(state=SetupState(),peer=sender.copy(setupDecision=null)) {p,_->
            val before=p.read().devices.single();assertEquals(SetupScope.ASK,before.setupScope)
            assertEquals(SetupHelloResult.WAIT,p.setupHello(binding,0,SetupHello(SetupScope.ASK),SetupHello(SetupScope.ENABLED),self))
            val prompt=p.read().membership.prompts.single()
            p.answerSetupPrompt(prompt,false)
            assertEquals(SetupScope.DISABLED,p.read().devices.single().setupScope)
            assertTrue(p.read().membership.prompts.isEmpty())
            p.setupHello(binding,0,SetupHello(SetupScope.DISABLED),SetupHello(SetupScope.ENABLED),self)
            assertTrue(p.read().membership.prompts.isEmpty())
            val confirmed=ConfirmedBinding.from(p.read(),b)
            p.requestSetupLink(confirmed)
            assertEquals(SetupScope.ENABLED,p.read().devices.single().setupScope)
            assertEquals(before,p.read().devices.single().copy(setupDecision=null))
            assertEquals(7L,p.read().devices.single().generation);assertTrue(p.read().resetPeers.isEmpty())
        }
        fixture(state=SetupState(),peer=sender.copy(setupDecision=SetupScope.ASK)) {p,_->
            p.setupHello(binding,0,SetupHello(SetupScope.ASK),SetupHello(SetupScope.ENABLED,s),self)
            val prompt=p.read().membership.prompts.single();p.answerSetupPrompt(prompt,true)
            assertEquals(SetupScope.ASK,p.read().devices.single().setupScope)
            assertEquals(SetupHelloResult.RECONNECT,p.setupHello(binding,0,SetupHello(SetupScope.ASK),SetupHello(SetupScope.ENABLED,s),self))
            assertEquals(SetupScope.ENABLED,p.read().devices.single().setupScope)
            assertEquals(SetupHelloResult.RECONNECT,p.setupHello(binding,0,SetupHello(SetupScope.ENABLED),SetupHello(SetupScope.ENABLED,s),self))
            try {p.answerSetupPrompt(prompt,true);fail("Expected stale prompt rejection")}catch(_:IllegalStateException) {}
        }
    }
    @Test fun unionCapacityFailureRollsBackEveryAuthoritativeWrite()=runBlocking<Unit>(Dispatchers.IO) {
        val many=(4..18).map {n->newRow.copy(deviceId=n.toString(16).padStart(32,'0'),fingerprint=n.toString(16).padStart(64,'0'),role="phone",reach=null)}
        fixture(state=initial.copy(roster=initial.roster+many)) {p,q->
            val before=p.read()
            try {receive(p,q);fail("Expected capacity failure")}catch(_:SetupCapacity) {}
            assertEquals(before,p.read());assertEquals(0,q.receiptCount())
        }
    }
    @Test fun withdrawalJournalsAllScopedResetsAndNeverAuthorsReciprocalTombstones()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            receive(p,q);finish(p,q)
            val snapshot=join.copy(transferId="8".repeat(32),members=listOf(selfRow.copy(status="removed",hlc="40"),senderRow,newRow))
            receive(p,q,snapshot)
            val state=p.read();assertNull(state.membership.setupId);assertTrue(state.membership.withdrawn)
            assertEquals(setOf(b,c),state.resetPeers);assertEquals(setOf(selfRow.key),state.membership.barriers.keys)
            assertTrue(state.membership.roster.isEmpty());assertTrue(state.membership.sends.isEmpty());assertTrue(state.devices.isEmpty())
            finish(p,q);assertTrue(p.read().resetPeers.isEmpty());assertTrue(p.read().membership.receiptIntents.isEmpty())
            try {receive(p,q,snapshot);fail("Expected retired generation rejection")}catch(_:SetupUnavailable) {}
        }
    }
    @Test fun processRestartFinishesCommittedCrossStoreIntentsAndRetainsExactPendingSnapshot()=runBlocking<Unit>(Dispatchers.IO) {
        val file=File(context.cacheDir,"restart-${randomId()}.preferences_pb")
        val databaseName="setup-restart-${randomId()}.db"
        var ownerJob=SupervisorJob()
        fun open()=Preferences(context,PreferenceDataStoreFactory.create(scope=CoroutineScope(ownerJob+Dispatchers.IO)) {file})
        var p=open();var database=Room.databaseBuilder(context,QueueDatabase::class.java,databaseName).build()
        try {
            p.store.edit {data->data[stringPreferencesKey("deviceId")]=a;data[longPreferencesKey("pairingRevision")]=7
                PeerRegistry.write(data,listOf(sender));SetupStorage.write(data,initial)}
            val pending=p.prepareMembers(binding,1,s)
            p.beforeMutation={if(it=="membersCommitted")throw java.io.IOException("Process death")}
            try {receive(p,database.queue());fail()}catch(_:java.io.IOException) {}
            val committed=p.read()
            database.close();ownerJob.cancelAndJoin()
            ownerJob=SupervisorJob();p=open();database=Room.databaseBuilder(context,QueueDatabase::class.java,databaseName).build()
            assertEquals(committed,p.read());finish(p,database.queue())
            assertTrue(receive(p,database.queue()).first)
            assertEquals(pending,p.prepareMembers(binding,1,s))
            assertEquals(1,p.read().membership.notices.size);assertEquals(8L,p.read().devices.single {it.deviceId==c}.generation)
        }finally {ownerJob.cancelAndJoin();database.close();file.delete();context.deleteDatabase(databaseName)}
    }
    @Test fun linkConsentAndPendingRecordCommitAtomicallyAndSurviveRealStoreRestart()=runBlocking<Unit>(Dispatchers.IO) {
        for(joinExisting in listOf(true,false)) {
            val file=File(context.cacheDir,"link-restart-${randomId()}.preferences_pb")
            var owner=SupervisorJob()
            fun open()=Preferences(context,PreferenceDataStoreFactory.create(scope=CoroutineScope(owner+Dispatchers.IO)) {file})
            var p=open()
            try {
                p.store.edit {data->data[stringPreferencesKey("deviceId")]=a
                    PeerRegistry.write(data,listOf(sender.copy(setupDecision=SetupScope.ASK)));SetupStorage.write(data,SetupState())}
                val before=p.read();val confirmed=ConfirmedBinding.from(before,b)
                // DataStore rolls back consent and the record together.
                try {p.edit {data->SetupTransactions.link(data,confirmed);throw java.io.IOException("Before commit")};fail()}
                catch(_:java.io.IOException) {}
                assertEquals(before,p.read())
                p.requestSetupLink(confirmed)
                val committed=p.read();val pending=requireNotNull(committed.membership.pendingLink)
                assertEquals(SetupScope.ENABLED,committed.devices.single().setupScope);assertNull(committed.membership.setupId)
                owner.cancelAndJoin();owner=SupervisorJob();p=open()
                assertEquals(committed,p.read())
                if(joinExisting) {
                    assertEquals(SetupHelloResult.RECONNECT,p.setupHello(binding,0,SetupHello(SetupScope.ENABLED),
                        SetupHello(SetupScope.ENABLED,s),self,pending.id))
                } else p.edit {data->assertTrue(SetupTransactions.finishLinkDiscovery(data,pending.id,self,now,{s}))}
                val assigned=p.read();assertEquals(s,assigned.membership.setupId);assertEquals(1L,assigned.membership.generation)
                assertNull(assigned.membership.pendingLink);assertEquals(committed.devices,assigned.devices)
                assertEquals(if(joinExisting)setOf(a,b) else setOf(a),assigned.membership.roster.map {it.deviceId}.toSet())
                owner.cancelAndJoin();owner=SupervisorJob();p=open();assertEquals(assigned,p.read())
                p.edit {data->assertFalse(SetupTransactions.finishLinkDiscovery(data,pending.id,self,now,{error("Must not found twice")}))}
            }finally {owner.cancelAndJoin();file.delete()}
        }
    }
    @Test fun tokenPairingAndMigrationJoinRaceRejectsDelayedForeignMembersWithoutEffects()=runBlocking<Unit>(Dispatchers.IO) {
        for(lowerProposer in listOf(true,false))for(migrationFirst in listOf(true,false))fixture(state=SetupState()) {p,q->
            val tokenId=if(lowerProposer)c else "0".repeat(32)
            val invitation=Invitation(1,"Token PC",pc,emptyList(),"f".repeat(64),100)
            val unrelated=sender.copy(deviceId="4".repeat(32),pin="d".repeat(64),setupDecision=SetupScope.ASK)
            p.edit {PeerRegistry.write(it,PeerRegistry.read(it)+unrelated)}
            val frozen=p.read().membership
            suspend fun migration() {
                val state=p.read().membership
                p.setupHello(binding,state.generation,state.hello(SetupScope.ENABLED),SetupHello(SetupScope.ENABLED,s),self)
            }
            if(migrationFirst)migration()
            var token:DeliveryBinding?=null
            assertTrue(p.paired(invitation,tokenId,setupChoice=true,setupFence=frozen,beforeInstall={data,installed->
                SetupTransactions.tokenHello(data,installed,SetupHello(SetupScope.ENABLED,if(lowerProposer)null else t),self,now,{t})
            },onInstalled={token=it}))
            if(!migrationFirst)migration()
            val winner=if(migrationFirst)s else t
            assertEquals(winner,p.read().membership.setupId);assertEquals(1L,p.read().membership.generation)
            assertEquals(SetupScope.ASK,p.read().devices.single {it.deviceId==unrelated.deviceId}.setupScope)
            val loser=if(migrationFirst)t else s
            val foreignBinding=if(migrationFirst)requireNotNull(token) else binding
            // A delayed authenticated HELLO and MEMBERS from the losing setup
            // cannot merge or observe clocks, reset workers, or reserve receipts.
            val current=p.read().membership
            val local=p.read().devices.single {it.deviceId==foreignBinding.pcId}.setupScope
            p.setupHello(foreignBinding,current.generation,current.hello(local),SetupHello(SetupScope.ENABLED,loser),self)
            val before=p.read();val body=join.copy(setupId=loser).json()
            try {p.receiveMembers(foreignBinding,1,loser,body,self,null,q,now);fail("Foreign setup must be rejected")}
            catch(_:SetupUnavailable) {}
            assertEquals(before,p.read());assertEquals(0,q.receiptCount())
        }
    }
    @Test fun commitRechecksConnectionAuthorityAndStaleWorkCannotApplyOrAcknowledge()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            val before=p.read()
            try {p.receiveMembers(binding,1,s,join.json(),self,null,q,now,current={false});fail()}catch(_:IllegalStateException) {}
            assertEquals(before,p.read());assertEquals(0,q.receiptCount())
            val body=p.prepareMembers(binding,1,s)
            try {p.acknowledgeMembers(binding,1,s,body,current={false});fail()}catch(_:IllegalStateException) {}
            assertEquals(body.toString(),p.read().membership.sends.getValue(b).pending)
        }
    }

    @Test fun localCertificateReplacementAtFingerprintCapRollsBackOldBindingAndAllTombstones()=runBlocking<Unit>(Dispatchers.IO) {
        val historic=(1..3).map {n->senderRow.copy(fingerprint=n.toString().repeat(64),status="removed",hlc=n.toString())}
        val state=initial.copy(roster=initial.roster+historic,barriers=historic.associate {it.key to RemovalBarrier(it)})
        fixture(state,sender.copy(pairingLost=true)) {p,_->
            val before=p.read();val invitation=Invitation(1,"Replacement","d".repeat(64),sender.addresses,"e".repeat(64),now)
            try {
                p.paired(invitation,b,replacement=ConfirmedBinding.from(before,b),setupFence=before.membership,beforeInstall={data,installed->
                    SetupTransactions.hello(data,installed,1,SetupHello(SetupScope.ENABLED,s),SetupHello(SetupScope.ENABLED,s),self,now,permitReset=true)
                })
                fail("Expected visible capacity failure")
            }catch(_:SetupCapacity) {}
            assertEquals(before,p.read());assertEquals(3,p.read().membership.barriers.size)
        }
    }

    @Test fun aDirectMembershipAckDoesNotAcknowledgeAnotherLinksPendingSnapshot()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {p,q->
            receive(p,q);finish(p,q)
            val cBinding=DeliveryBinding.from(p.read(),c)!!
            val bSnapshot=p.prepareMembers(binding,1,s);val cSnapshot=p.prepareMembers(cBinding,1,s)
            p.acknowledgeMembers(binding,1,s,bSnapshot)
            assertEquals("",p.read().membership.sends.getValue(b).pending)
            assertEquals(cSnapshot.toString(),p.read().membership.sends.getValue(c).pending)
            assertEquals("",p.read().membership.sends.getValue(c).acknowledged)
        }
    }

}
