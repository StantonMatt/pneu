package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.*
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import org.junit.Test

class SetupReconcileTest {
    private val a="1".repeat(32);private val b="2".repeat(32);private val c="3".repeat(32)
    private val pa="a".repeat(64);private val pb="b".repeat(64);private val pc="c".repeat(64)
    private val s="0".repeat(32);private val t="f".repeat(32)
    private val now=1800000000100L
    private val self=SetupIdentity(b,pb,"Office PC","computer")
    private val sender=Peer("Phone",pa,a,emptyList(),role="phone",generation=5,setupDecision=SetupScope.ENABLED,setupSupported=true)
    private val vectors=JsonParser.parseReader(javaClass.getResourceAsStream("/setup-membership-vectors.json")!!.reader()).asJsonObject["vectors"].asJsonArray
        .associate {it.asJsonObject.let {o->o["name"].asString to SetupWire.decode(JsonParser.parseString(o["payload"].asString).asJsonObject)}}
    private val join get()=vectors.getValue("join")
    private val initial get()=SetupState(s,1,join.members.take(2))
    private fun plan(state:SetupState=initial,peers:List<Peer> = listOf(sender),snapshot:MembersSnapshot=join,attempt:SetupAttemptTarget?=null)=
        SetupReconcile.plan(state,peers,self,sender,snapshot,attempt,now)
    private fun restart(state:SetupState):SetupState {val p=mutablePreferencesOf();SetupStorage.write(p,state);return SetupStorage.read(p)}
    private fun data(state:SetupState=initial,peers:List<Peer> = listOf(sender))=mutablePreferencesOf(stringPreferencesKey("deviceId") to b,longPreferencesKey("pairingRevision") to 5L).also {
        PeerRegistry.write(it,peers);SetupStorage.write(it,state)
    }
    @Test fun fullFlowIntroductionInstallsOnceWithDefaultsReadinessZeroAndProvenance() {
        val first=plan();val p=data();SetupTransactions.commit(p,first,sender)
        val peer=PeerRegistry.read(p).single {it.deviceId==c}
        assertEquals(6L,peer.generation);assertEquals(SetupScope.ENABLED,peer.setupScope);assertFalse(peer.established)
        assertEquals(a,peer.addedViaId);assertEquals("Phone",peer.addedViaName)
        assertTrue(peer.settings.values.all {it.enabled && it.hlc=="0"})
        val next=plan(SetupStorage.read(p),PeerRegistry.read(p))
        assertTrue(next.installed.isEmpty());assertTrue(next.removed.isEmpty())
        assertEquals(peer,next.peers.single {it.deviceId==c})
        assertEquals(1,SetupStorage.read(p).notices.size)
    }
    @Test fun staleReplayAndConcurrentAddRemoveAreTerminalAcrossRestartInBothOrders() {
        val first=plan()
        for(order in listOf(listOf("remove","later-introduction"),listOf("later-introduction","remove"))) {
            var state=first.state;var peers=first.peers
            for(name in order) {val next=plan(restart(state),peers,vectors.getValue(name));state=next.state;peers=next.peers}
            val replay=plan(restart(state),peers,join)
            assertEquals("removed",replay.state.roster.single {it.deviceId==c}.status)
            assertEquals("1800000000010",replay.state.barriers.getValue("$c/$pc").removed.hlc)
            assertTrue(replay.installed.isEmpty());assertTrue(replay.peers.none {it.deviceId==c})
        }
    }
    @Test fun freshLocalPairClearsOnlyItsDenyAndKeepsRemovalHighWater() {
        val removed=plan(plan().state,plan().peers,vectors.getValue("remove"));val p=data(removed.state,removed.peers)
        MeshClock.observe(p,"1800000000020".toBigInteger(),now)
        val peer=Peer("Laptop",pc,c,emptyList(),setupDecision=SetupScope.ENABLED)
        assertThrows(IllegalStateException::class.java) {SetupTransactions.paired(p,removed.state,peer,null,true,false,now=now)}
        SetupTransactions.paired(p,removed.state,peer,null,true,true,now=now)
        val revived=SetupStorage.read(p).copy(roster=removed.state.roster.filterNot {it.deviceId==c}+
            join.members[2].copy(hlc="1800000000021",readmits=MemberReadmits("1800000000010",b)))
        val old=plan(revived,listOf(sender,peer),vectors.getValue("remove"))
        assertFalse(old.state.denies(c,pc));assertTrue(old.peers.any {it.deviceId==c})
        val newer=vectors.getValue("remove").copy(members=vectors.getValue("remove").members.map {if(it.deviceId==c)it.copy(hlc="1800000000040") else it})
        assertTrue(plan(old.state,old.peers,newer).state.denies(c,pc))
    }
    @Test fun reusedIdCannotReplaceBindingAndHistoricalRemovalDoesNotDeleteReplacement() {
        val first=plan();val existing=first.peers.single {it.deviceId==c}.copy(generation=9)
        val changed=join.copy(members=join.members.map {if(it.deviceId==c)it.copy(fingerprint="d".repeat(64)) else it})
        val blocked=plan(first.state,listOf(sender,existing),changed)
        assertEquals(pc,blocked.peers.single {it.deviceId==c}.pin);assertTrue(blocked.installed.isEmpty())
        assertTrue(blocked.state.conflicts.isNotEmpty())
        val replacement=existing.copy(pin="d".repeat(64))
        val state=first.state.copy(roster=first.state.roster.map {if(it.deviceId==c)it.copy(fingerprint=replacement.pin) else it})
        val removal=plan(state,listOf(sender,replacement),vectors.getValue("remove"))
        assertEquals(replacement,removal.peers.single {it.deviceId==c});assertTrue(removal.removed.isEmpty())
    }
    @Test fun historicalSelfAndSenderPinsAreAcceptedWithoutChangingCurrentIdentity() {
        val snap=vectors.getValue("receiver-certificate-replacement")
        val replacedSelf=self.copy(pin="d".repeat(64))
        val result=SetupReconcile.plan(initial.copy(roster=snap.members.take(2)),listOf(sender),replacedSelf,sender,snap,now=now)
        assertFalse(result.state.withdrawn);assertTrue(result.state.denies(b,pb));assertEquals(s,result.state.setupId)
        val historical=sender.let {SetupMember(a,"e".repeat(64),"computer","Old phone","removed","1800000000000",b)}
        val accepted=SetupReconcile.plan(result.state,result.peers,replacedSelf,sender,snap.copy(members=snap.members+historical),now=now)
        assertTrue(accepted.state.denies(a,historical.fingerprint));assertFalse(accepted.state.denies(a,pa))
        assertThrows(MeshViolation::class.java) {plan(snapshot=join.copy(members=join.members.map {if(it.deviceId==b)it.copy(fingerprint="d".repeat(64)) else it}))}
        assertThrows(MeshViolation::class.java) {plan(snapshot=join.copy(members=join.members.map {if(it.deviceId==a)it.copy(fingerprint="d".repeat(64)) else it}))}
    }
    @Test fun uniquePinAndSelfIntroductionNeverInstallOrOverwriteIdentity() {
        val first=plan()
        val existing=first.peers.single {it.deviceId==c}
        val conflict=join.copy(members=join.members.take(2)+join.members[2].copy(deviceId="4".repeat(32),reach=null))
        val blocked=plan(first.state,first.peers,conflict)
        assertTrue(blocked.installed.isEmpty());assertEquals(existing,blocked.peers.single {it.deviceId==c})
        for(pin in listOf(pb,pa))assertThrows(IllegalArgumentException::class.java) {
            plan(snapshot=join.copy(members=join.members.map {if(it.deviceId==c)it.copy(fingerprint=pin) else it}))
        }
        val own=initial.roster.single {it.deviceId==b}
        val observed=plan(snapshot=join.copy(members=join.members.map {if(it.deviceId==b)it.copy(name="Spoof rename",hlc="1800000000050") else it}))
        assertEquals("Spoof rename",observed.state.roster.single {it.deviceId==b}.name)
        assertEquals("Office PC",self.name);assertEquals(own.fingerprint,self.pin)
        assertTrue(observed.peers.none {it.deviceId==b})
    }
    @Test fun unchangedRosterAndReachRefreshPreserveGenerationSettingsQueuesReceiptsAndProvenance() {
        val first=plan();val installed=first.peers.single {it.deviceId==c}.copy(generation=17,lastLan="192.168.1.99:47821",
            pendingSettings="pending",confirmedSettings="confirmed",established=true,clipboardSend=false,addedViaName="Original")
        val peers=listOf(sender,installed)
        val reorder=join.copy(transferId="9".repeat(32),members=join.members.reversed())
        val replay=plan(first.state,peers,reorder)
        assertEquals(installed,replay.peers.single {it.deviceId==c})
        val refresh=plan(first.state,peers,vectors.getValue("reach-refresh"))
        val updated=refresh.peers.single {it.deviceId==c}
        assertEquals(installed,updated.copy(addresses=installed.addresses))
        assertEquals("192.168.1.13:47821",updated.addresses.first().toString());assertEquals(installed.lastLan,updated.lastLan)
        assertTrue(refresh.installed.isEmpty())
        val removed=plan(first.state,peers,vectors.getValue("remove"))
        assertTrue(plan(removed.state,removed.peers,vectors.getValue("reach-refresh")).installed.isEmpty())
    }
    @Test fun equalMembershipOrReachStampConflictRejectsWholeSnapshot() {
        val first=plan()
        for(m in listOf(join.members[2].copy(name="Conflict"),join.members[2].copy(status="removed",reach=null),
            join.members[2].copy(reach=join.members[2].reach!!.copy(addresses=listOf("192.168.1.88:47821"))))) {
            assertThrows(MeshViolation::class.java) {plan(first.state,first.peers,join.copy(members=join.members.take(2)+m))}
        }
        assertEquals(first.state,restart(first.state))
    }
    @Test fun liveAttemptBlocksByIdOrUnresolvedPinThenReconcilesWithoutNewReceipt() {
        for(target in listOf(SetupAttemptTarget(c,pc),SetupAttemptTarget(null,pc),SetupAttemptTarget(c,"d".repeat(64)))) {
            val blocked=plan(attempt=target);assertTrue(blocked.installed.isEmpty())
            assertTrue(blocked.state.roster.any {it.deviceId==c && it.active})
            val released=plan(restart(blocked.state),blocked.peers)
            assertEquals(1,released.installed.size);assertEquals(a,released.installed.single().addedViaId)
        }
        assertEquals(1,plan(attempt=SetupAttemptTarget("4".repeat(32),"d".repeat(64))).installed.size)
    }
    @Test fun offlineCatchupRetainsMissingEntriesAndOriginalWriters() {
        val first=plan();val omitted=join.copy(members=join.members.take(2))
        val catchup=plan(restart(first.state),first.peers,omitted)
        assertEquals(first.state.roster,catchup.state.roster)
        assertEquals(join.members[2].writer,catchup.state.roster.single {it.deviceId==c}.writer)
        assertEquals(join.members[2].reach,catchup.state.roster.single {it.deviceId==c}.reach)
    }
    @Test fun phonePhoneMembershipIsMetadataOnlyNeverDirectRecipientsOrSettings() {
        val snapshot=join.copy(members=join.members.map {if(it.deviceId==c)it.copy(role="phone",reach=null) else it})
        val result=plan(snapshot=snapshot)
        assertTrue(result.state.roster.any {it.deviceId==c});assertTrue(result.peers.none {it.deviceId==c})
        assertTrue(result.installed.isEmpty());assertTrue(result.state.sends.isEmpty())
        val phone=Peer("Other phone",pc,c,emptyList(),role="phone")
        assertTrue(RecipientSelection.freeze(Prefs(deviceId=b,peers=listOf(phone)),setOf(c),"file").isEmpty())
    }
    @Test fun ownWithdrawalRetainsHistoryUnscopedLinksAndNoReciprocalTombstones() {
        val first=plan();val unscoped=Peer("Unscoped","e".repeat(64),"5".repeat(32),emptyList(),setupDecision=SetupScope.DISABLED)
        val result=plan(first.state,first.peers+unscoped,vectors.getValue("targeted-withdrawal"))
        assertNull(result.state.setupId);assertTrue(result.state.withdrawn);assertEquals(2L,result.state.generation)
        assertEquals(listOf(unscoped),result.peers)
        assertEquals(setOf("$b/$pb"),result.state.barriers.keys);assertTrue(result.state.roster.isEmpty())
        assertTrue(result.state.sends.isEmpty());assertTrue(result.state.receipts.isEmpty())
        assertFalse(result.state.denies(a,pa));assertFalse(result.state.denies(c,pc))
    }
    @Test fun removedSenderIsIgnoredBeforeMergeClockOrReceipt() {
        val removal=join.members.first().copy(status="removed",hlc="1800000000010")
        val state=initial.copy(barriers=mapOf(removal.key to RemovalBarrier(removal)))
        assertThrows(RemovedSetupSender::class.java) {plan(state,snapshot=vectors.getValue("remove"))}
        assertEquals(state,restart(state))
    }
    @Test fun lastMemberDissolutionAndS5RetainedMemberBridgeDoNotMergeSeparateSetups() {
        val first=plan();val p=data(first.state,first.peers)
        // B learns C through A, while its existing direct C scope stays ask.
        PeerRegistry.write(p,PeerRegistry.read(p).map {if(it.deviceId==c)it.copy(setupDecision=SetupScope.ASK) else it})
        SetupTransactions.remove(p,"$a/$pa",1,now)
        var state=SetupStorage.read(p);assertEquals(s,state.setupId);assertTrue(state.member(c)!=null)
        val d=Peer("D","d".repeat(64),"4".repeat(32),emptyList(),setupDecision=SetupScope.ENABLED)
        PeerRegistry.write(p,PeerRegistry.read(p)+d)
        val binding=DeliveryBinding(b,d.pin,d.deviceId,d.generation)
        assertEquals(SetupHelloResult.SEPARATE,SetupTransactions.hello(p,binding,1,state.hello(SetupScope.ENABLED),SetupHello(SetupScope.ENABLED,t),self,now))
        assertEquals(s,SetupStorage.read(p).setupId);assertTrue(SetupStorage.read(p).member(c)!=null)
        SetupTransactions.remove(p,"$c/$pc",1,now)
        state=restart(SetupStorage.read(p));assertNull(state.setupId);assertEquals(2L,state.generation)
        assertTrue(state.roster.isEmpty());assertTrue(state.denies(c,pc));assertTrue(state.denies(a,pa))
        // Retired state is local history; subsequent assignment exports only new positives.
        PeerRegistry.write(p,PeerRegistry.read(p).map {it.copy(setupDecision=SetupScope.ENABLED)})
        assertEquals(SetupHelloResult.RECONNECT,SetupTransactions.hello(p,binding,2,state.hello(SetupScope.ENABLED),SetupHello(SetupScope.ENABLED,t),self,now))
        state=SetupStorage.read(p);assertEquals(t,state.setupId);assertEquals(setOf(b,d.deviceId),state.roster.map {it.deviceId}.toSet())
        assertTrue(state.denies(c,pc));assertTrue(state.roster.none {it.deviceId==c || it.deviceId==a})
    }
    @Test fun sizeCapsIncludeRetiredBarriersAndNoEvictionIsPermitted() {
        val removed=(1..64).map {n->SetupMember(n.toString(16).padStart(32,'0'),n.toString(16).padStart(64,'0'),"computer","Old","removed","1",b)}
        val history=SetupState(barriers=removed.associate {it.key to RemovalBarrier(it)})
        SetupReconcile.capacity(history,emptyList())
        val extra=removed[0].copy(deviceId="f".repeat(32),fingerprint="f".repeat(64))
        assertThrows(SetupCapacity::class.java) {SetupReconcile.capacity(history.copy(barriers=history.barriers+(extra.key to RemovalBarrier(extra))),emptyList())}
        val perId=(1..4).map {removed[0].copy(fingerprint=it.toString(16).padStart(64,'0'))}
        val four=SetupState(barriers=perId.associate {it.key to RemovalBarrier(it)})
        SetupReconcile.capacity(four,emptyList())
        assertThrows(SetupCapacity::class.java) {SetupReconcile.capacity(four.copy(roster=listOf(perId[0].copy(fingerprint="f".repeat(64),status="member"))),emptyList())}
        val peers=(1..16).map {n->Peer("PC$n",n.toString(16).padStart(64,'0'),n.toString(16).padStart(32,'0'),emptyList())}
        SetupReconcile.capacity(SetupState(),peers)
        assertThrows(SetupCapacity::class.java) {SetupReconcile.capacity(SetupState(),peers+peers[0].copy(deviceId="f".repeat(32),pin="f".repeat(64)))}
    }
    @Test fun futureSkewAndUnsignedTupleOrderRejectBeforeStaging() {
        val boundary=join.copy(members=join.members.map {it.copy(hlc=(now+60000).toString(),reach=null)})
        plan(snapshot=boundary)
        assertThrows(MeshViolation::class.java) {plan(snapshot=boundary.copy(members=boundary.members.map {it.copy(hlc=(now+60001).toString())}))}
        assertThrows(MeshViolation::class.java) {plan(snapshot=join.copy(members=join.members.map {it.copy(hlc="18446744073709551615")}))}
        assertTrue(SetupReconcile.stamp(join.members[2].copy(hlc="9223372036854775808"))>SetupReconcile.stamp(join.members[2].copy(hlc=Long.MAX_VALUE.toString())))
    }
    @Test fun attemptBlockedCandidateReconcilesAfterIntroducerRemovalWithoutNewReceiptOrEnabledLink() {
        val blocked=plan(attempt=SetupAttemptTarget(null,pc))
        val p=data(blocked.state,blocked.peers)
        SetupTransactions.remove(p,"$a/$pa",1,now)
        val state=SetupStorage.read(p)
        assertEquals(s,state.setupId);assertTrue(PeerRegistry.read(p).isEmpty())
        val released=SetupReconcile.retained(state,emptyList(),b,null)
        assertEquals(1,released.installed.size);assertEquals(a,released.installed.single().addedViaId)
        SetupTransactions.commit(p,released)
        assertEquals(6L,PeerRegistry.read(p).single().generation);assertTrue(c in PeerRegistry.resets(p))
    }
    @Test fun confirmedRosterRemovalCannotFollowAReplacementBindingOrSetupGeneration() {
        val first=plan();val peer=first.peers.single {it.deviceId==c}.copy(generation=8)
        val p=Prefs(deviceId=b,peers=listOf(sender,peer),membership=first.state)
        val confirmed=ConfirmedSetupMember.from(p,"$c/$pc")
        assertTrue(confirmed.matches(p))
        assertFalse(confirmed.matches(p.copy(peers=listOf(sender,peer.copy(generation=9)))))
        assertFalse(confirmed.matches(p.copy(membership=p.membership.copy(generation=2))))
        val phoneRow=join.members[2].copy(role="phone",reach=null)
        val phonePrefs=p.copy(peers=listOf(sender),membership=p.membership.copy(roster=join.members.take(2)+phoneRow))
        assertNull(ConfirmedSetupMember.from(phonePrefs,phoneRow.key).binding)
        assertTrue(ConfirmedSetupMember.from(phonePrefs,phoneRow.key).matches(phonePrefs))
    }

    @Test fun legitimateReverseRemovalRevokesOnlyTheChosenMemberAndOfflineCatchupRemainsNecessary() {
        val first=plan();val p=data(first.state,first.peers)
        SetupTransactions.remove(p,"$a/$pa",1,now)
        val removal=SetupStorage.read(p)
        // A different endpoint, C, still has A until it receives B's full state.
        val cIdentity=SetupIdentity(c,pc,"Laptop","computer")
        val bPeer=Peer("Office PC",pb,b,emptyList(),role="computer",setupDecision=SetupScope.ENABLED)
        val offline=initial.copy(roster=join.members)
        assertTrue(offline.member(a)!=null);assertFalse(offline.denies(a,pa))
        val caughtUp=SetupReconcile.plan(offline,listOf(sender,bPeer),cIdentity,bPeer,
            MembersSnapshot("a".repeat(32),s,removal.roster),now=now)
        assertTrue(caughtUp.state.denies(a,pa));assertFalse(caughtUp.state.denies(b,pb))
        assertTrue(caughtUp.peers.any {it.deviceId==b});assertTrue(caughtUp.peers.none {it.deviceId==a})
        assertTrue(offline.member(a)!=null) // no instantaneous/global revocation
    }

}
