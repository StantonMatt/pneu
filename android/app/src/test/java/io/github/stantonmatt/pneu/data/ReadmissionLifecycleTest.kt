package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.Gson
import io.github.stantonmatt.pneu.connection.PairingAttempt
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import org.junit.Test

class ReadmissionLifecycleTest {
    private val a="1".repeat(32);private val b="2".repeat(32);private val c="3".repeat(32)
    private val pin="c".repeat(64)
    private val self=SetupIdentity(b,"b".repeat(64),"Phone")
    private val sender=Peer("Introducer","a".repeat(64),a,emptyList(),setupDecision=SetupScope.ENABLED)
    private val removal=SetupMember(c,pin,"computer","Laptop","removed","10",b)
    private val member=removal.copy(status="member",hlc="20",writer=c,reach=MemberReach(listOf("192.168.1.2:47821"),"21",c),readmits=MemberReadmits(removal.hlc,removal.writer))
    private val base=SetupState("0".repeat(32),7,listOf(SetupMember(b,self.pin,"phone","Phone","member","1",b),SetupMember(a,sender.pin,"computer",sender.name,"member","2",a),removal),
        barriers=mapOf(removal.key to RemovalBarrier(removal)))
    private fun observe(state:SetupState=base,m:SetupMember=member)=SetupReconcile.plan(state,listOf(sender),self,sender,
        MembersSnapshot("f".repeat(32),state.setupId!!,listOf(state.member(a)!!,m)),now=100).state
    private val pending get()=observe()
    private fun prompt(state:SetupState)=SetupReconcile.readmissions(state).single()
    @Test fun oldGsonDataAndIndependentReachMergeRemainCompatible() {
        val old=Gson().fromJson("""{"removed":${Gson().toJson(removal)},"cleared":false}""",RemovalBarrier::class.java)
        assertNull(old.readmission)
        val first=pending
        val newerReach=member.copy(hlc="19",reach=member.reach!!.copy(hlc="22",addresses=listOf("192.168.1.3:47821")))
        val next=observe(first,newerReach)
        assertEquals("20",next.barriers.getValue(removal.key).readmission!!.member.hlc)
        assertEquals("22",next.barriers.getValue(removal.key).readmission!!.member.reach!!.hlc)
        assertEquals(first.roster,next.roster)
    }
    @Test fun allowConflictsAndBothAttemptTargetsLeavePendingAndPhoneSubjectsStayMetadata() {
        val state=pending;val prompt=prompt(state)
        for(peer in listOf(Peer("Conflicting ID","d".repeat(64),c,emptyList()),Peer("Conflicting pin",pin,"4".repeat(32),emptyList()))) {
            val error=assertThrows(ReadmissionBlocked::class.java) {SetupReconcile.answer(state,listOf(sender,peer),b,prompt,true)}
            assertTrue(error.conflict)
        }
        for(target in listOf(SetupAttemptTarget(c,"d".repeat(64)),SetupAttemptTarget(null,pin))) {
            val error=assertThrows(ReadmissionBlocked::class.java) {SetupReconcile.answer(state,listOf(sender),b,prompt,true,target)}
            assertFalse(error.conflict)
        }
        assertTrue(state.denies(c,pin));assertEquals(ReadmissionDecision.PENDING,state.barriers.getValue(removal.key).readmission!!.decision)
        val phone=observe(m=member.copy(role="phone",reach=null))
        val plan=SetupReconcile.answer(phone,listOf(sender),b,prompt(phone),true)!!
        assertTrue(plan.installed.isEmpty());assertNotNull(plan.state.member(c));assertFalse(plan.state.denies(c,pin))
    }
    @Test fun onlyClearedHistoricalBarriersPermitANewPinAndOwnKeysNeverPrompt() {
        val new=member.copy(fingerprint="d".repeat(64),reach=null)
        assertNull(observe(m=new).member(c))
        val cleared=base.copy(barriers=mapOf(removal.key to RemovalBarrier(removal,true)))
        assertEquals(new,observe(cleared,new).member(c))
        val ownRemoval=base.member(b)!!.copy(status="removed",hlc="10")
        val own=base.copy(barriers=mapOf(ownRemoval.key to RemovalBarrier(ownRemoval)))
        val result=observe(own,ownRemoval.copy(status="member",hlc="20"))
        assertNull(result.barriers.getValue(ownRemoval.key).readmission)
    }
    @Test fun sameSnapshotNewPinAndRemovalAreGuardedRegardlessOfEntryOrder() {
        val old=base.copy(barriers=emptyMap(),roster=base.roster.filter {it.active})
        val new=member.copy(fingerprint="d".repeat(64),reach=null)
        for(entries in listOf(listOf(new,removal),listOf(removal,new))) {
            val plan=SetupReconcile.plan(old,listOf(sender),self,sender,
                MembersSnapshot("f".repeat(32),old.setupId!!,listOf(old.member(a)!!)+entries),now=100)
            assertTrue(plan.installed.isEmpty());assertNull(plan.state.member(c))
            assertTrue(plan.state.conflicts.any {it.deviceId==c});assertTrue(plan.state.denies(c,pin))
        }
    }
    @Test fun retirementDropsPendingButRetainsAllowedAndKeptDecisionsAndActionsAreFenced() {
        for(allow in listOf(false,true)) {
            val state=pending;val shown=prompt(state)
            val decided=SetupReconcile.answer(state,listOf(sender),b,shown,allow)!!.state
            val retired=SetupReconcile.retire(decided,true)
            assertEquals(decided.barriers,retired.barriers)
            assertNull(SetupReconcile.answer(retired,listOf(sender),b,shown,true))
            assertNull(SetupReconcile.answer(decided,listOf(sender),b,shown,true))
        }
        val state=pending
        assertNull(SetupReconcile.retire(state,false).barriers.getValue(removal.key).readmission)
        assertNull(SetupReconcile.answer(state.copy(generation=8),listOf(sender),b,prompt(state),true))
        assertNull(SetupReconcile.answer(state.copy(barriers=mapOf(removal.key to RemovalBarrier(removal.copy(hlc="30")))),listOf(sender),b,prompt(state),true))
    }
    @Test fun observationAndDecisionDoNotInvalidateAFreshPairingRemovalFence() {
        val scanned=Prefs(deviceId=b,membership=base)
        val invitation=Invitation(1,"Laptop",pin,emptyList(),"f".repeat(64),100)
        val attempt=PairingAttempt("fresh",invitation,scanned,emptyMap(),deviceId=c)
        val state=pending
        assertTrue(attempt.matches(scanned.copy(membership=state),emptyMap()))
        val kept=SetupReconcile.answer(state,listOf(sender),b,prompt(state),false)!!.state
        assertTrue(attempt.matches(scanned.copy(membership=kept),emptyMap()))
        assertFalse(attempt.matches(scanned.copy(membership=kept.copy(barriers=mapOf(removal.key to RemovalBarrier(removal.copy(hlc="30"))))),emptyMap()))
    }
    @Test fun anUnseenOldJoinedNoticeCannotReappearOnReadmissionAllow() {
        val state=pending.copy(notices=listOf(SetupNotice("7/$c/5",c,"Laptop",a,"Introducer")))
        val p=mutablePreferencesOf(stringPreferencesKey("deviceId") to b,longPreferencesKey("pairingRevision") to 6L)
        PeerRegistry.write(p,listOf(sender));SetupStorage.write(p,state)
        val plan=SetupReconcile.answer(state,listOf(sender),b,prompt(state),true)!!
        SetupTransactions.commit(p,plan)
        assertTrue(SetupStorage.read(p).notices.isEmpty())
        assertEquals(7L,PeerRegistry.read(p).single {it.deviceId==c}.generation)
    }
    @Test fun protectedRefusalIsAuthenticatedAndBindingFencedBeforeAndAfterReady() {
        for(ready in listOf(false,true)) {
            val peer=Peer("Laptop",pin,c,emptyList(),generation=8,addedViaId=a,established=ready)
            val binding=DeliveryBinding(b,pin,c,8)
            assertEquals(peer,refusedPeer(peer,binding,false))
            assertEquals(peer,refusedPeer(peer,binding.copy(generation=9),true))
            assertTrue(refusedPeer(peer,binding,true).setupRefused)
            assertTrue(refusedPeer(peer,binding,true).setupScoped)
            assertEquals(peer.copy(addedViaId=null),refusedPeer(peer.copy(addedViaId=null),binding,true))
        }
    }
    @Test fun inheritedFollowDefersBothAttemptTargetsAndReconcilesOnceWithOriginalIntroducer() {
        val inherited=base.copy(barriers=mapOf(removal.key to RemovalBarrier(removal,authored=false,revoked_binding=true)))
        for(target in listOf(SetupAttemptTarget(c,"d".repeat(64)),SetupAttemptTarget(null,pin))) {
            val blocked=SetupReconcile.plan(inherited,listOf(sender),self,sender,
                MembersSnapshot("f".repeat(32),inherited.setupId!!,listOf(inherited.member(a)!!,member)),target,100)
            assertTrue(blocked.state.denies(c,pin));assertTrue(blocked.installed.isEmpty())
            assertTrue(SetupReconcile.readmissions(blocked.state).isEmpty())
            assertEquals(inherited.roster.toSet(),blocked.state.roster.toSet())
            val prefs=mutablePreferencesOf(stringPreferencesKey("deviceId") to b)
            PeerRegistry.write(prefs,listOf(sender));SetupStorage.write(prefs,blocked.state)
            val released=SetupReconcile.retained(SetupStorage.read(prefs),listOf(sender),b,null)
            assertFalse(released.state.denies(c,pin));assertEquals(member,released.state.member(c))
            assertEquals(a,released.installed.single().addedViaId)
            assertEquals(removal,released.state.barriers.getValue(removal.key).removed)
            assertEquals(false,released.state.barriers.getValue(removal.key).authored)
            assertEquals(false,released.state.barriers.getValue(removal.key).revoked_binding)
            SetupTransactions.commit(prefs,released)
            assertEquals(1,SetupStorage.read(prefs).notices.size)
            assertTrue(SetupReconcile.retained(SetupStorage.read(prefs),PeerRegistry.read(prefs),b,null).installed.isEmpty())
            assertThrows(SetupUnavailable::class.java) {SetupReconcile.retained(SetupReconcile.retire(blocked.state,true),listOf(sender),b,null)}
        }
    }
    @Test fun inheritedFollowKeepsDenyOnConflictsAndCapacityFailureAndHonorsInitialJoinException() {
        val inherited=base.copy(barriers=mapOf(removal.key to RemovalBarrier(removal,authored=false,revoked_binding=false)))
        val snapshot=MembersSnapshot("f".repeat(32),inherited.setupId!!,listOf(inherited.member(a)!!,member))
        for(peer in listOf(Peer("Other ID",pin,"4".repeat(32),emptyList()),Peer("Other pin","d".repeat(64),c,emptyList()))) {
            val blocked=SetupReconcile.plan(inherited,listOf(sender,peer),self,sender,snapshot,now=100)
            assertTrue(blocked.state.denies(c,pin));assertTrue(blocked.installed.isEmpty())
            assertTrue(blocked.state.conflicts.isNotEmpty());assertTrue(SetupReconcile.readmissions(blocked.state).isEmpty())
        }
        val full=inherited.copy(roster=inherited.roster+(4..18).map {n->
            SetupMember(n.toString(16).padStart(32,'0'),n.toString(16).padStart(64,'0'),"phone","Extra","member","1",b)})
        assertThrows(SetupCapacity::class.java) {SetupReconcile.plan(full,listOf(sender),self,sender,snapshot,now=100)}
        assertTrue(full.denies(c,pin))
        val initial=SetupReconcile.plan(inherited.copy(initialSnapshotPending=true),listOf(sender),self,sender,snapshot,now=100)
        assertTrue(initial.alsoLinked);assertEquals(1,initial.installed.size)
    }
    @Test fun retainedOwnClaimProtectsIdentityFromDelayedRemovalWithoutWithdrawal() {
        val own=base.member(b)!!.copy(readmits=MemberReadmits("10",a))
        val state=base.copy(roster=listOf(own,base.member(a)!!),barriers=emptyMap())
        val removed=own.copy(status="removed",hlc="10",writer=a,readmits=null)
        val plan=SetupReconcile.plan(state,listOf(sender),self,sender,
            MembersSnapshot("f".repeat(32),state.setupId!!,listOf(state.member(a)!!,removed)),now=100)
        assertFalse(plan.state.withdrawn);assertEquals(state.setupId,plan.state.setupId)
        assertFalse(plan.state.denies(b,self.pin));assertEquals(own,plan.state.member(b))
        assertTrue(plan.removed.isEmpty());assertTrue(plan.removedKeys.isEmpty())
    }
    @Test fun freshPairClaimSurvivesDelayedRemovalBeforeReciprocalHello() {
        val prefs=mutablePreferencesOf(stringPreferencesKey("deviceId") to b)
        PeerRegistry.write(prefs,listOf(sender));SetupStorage.write(prefs,base)
        val subject=Peer(member.name,pin,c,emptyList(),setupDecision=SetupScope.ENABLED)
        SetupTransactions.paired(prefs,base,subject,null,true,true,now=100)
        val staged=SetupStorage.read(prefs)
        assertFalse(staged.denies(c,pin));assertNull(staged.member(c))
        val delayed=SetupReconcile.plan(staged,listOf(sender,subject),self,sender,
            MembersSnapshot("f".repeat(32),staged.setupId!!,listOf(staged.member(a)!!,removal)),now=100)
        assertFalse(delayed.state.denies(c,pin));assertTrue(delayed.removedKeys.isEmpty())
        SetupTransactions.commit(prefs,delayed,sender)
        SetupTransactions.tokenHello(prefs,DeliveryBinding(b,pin,c,subject.generation),SetupHello(SetupScope.ENABLED,base.setupId),self,100)
        assertEquals(MemberReadmits("10",b),SetupStorage.read(prefs).member(c)!!.readmits)
    }

    @Test fun allowedRecordCannotRollBackLaterMemberOrClaimRegisters() {
        val asked=pending
        val allowed=SetupReconcile.answer(asked,listOf(sender),b,prompt(asked),true)!!
        val renamed=member.copy(hlc="25",name="Renamed",readmits=MemberReadmits("12",a))
        val latest=SetupReconcile.plan(allowed.state,allowed.peers,self,sender,
            MembersSnapshot("e".repeat(32),base.setupId!!,listOf(base.member(a)!!,renamed)),now=100)
        val stale=member.copy(hlc="21",readmits=null)
        val replay=SetupReconcile.plan(latest.state,latest.peers,self,sender,
            MembersSnapshot("d".repeat(32),base.setupId!!,listOf(base.member(a)!!,stale)),now=100)
        assertEquals(renamed,replay.state.member(c))
        assertEquals(latest.peers,replay.peers)
    }

    @Test fun deferredInitialInheritedFollowRemembersTheJoiningDeviceNoticeException() {
        val inherited=base.copy(initialSnapshotPending=true,barriers=mapOf(removal.key to RemovalBarrier(removal,authored=false,revoked_binding=false)))
        val blocked=SetupReconcile.plan(inherited,listOf(sender),self,sender,
            MembersSnapshot("f".repeat(32),inherited.setupId!!,listOf(inherited.member(a)!!,member)),SetupAttemptTarget(null,pin),100)
        val prefs=mutablePreferencesOf(stringPreferencesKey("deviceId") to b)
        PeerRegistry.write(prefs,listOf(sender));SetupStorage.write(prefs,blocked.state)
        val released=SetupReconcile.retained(SetupStorage.read(prefs),listOf(sender),b,null)
        SetupTransactions.commit(prefs,released)
        assertTrue(SetupStorage.read(prefs).notices.single().alsoLinked)
    }

    @Test fun freshPairWithClearedHighWaterPreservesAHigherIndependentAcceptedClaim() {
        val accepted=member.copy(readmits=MemberReadmits("30",a))
        val state=base.copy(roster=base.roster.filterNot {it.key==member.key}+accepted,
            barriers=mapOf(removal.key to RemovalBarrier(removal,true,authored=false,revoked_binding=false)))
        val prefs=mutablePreferencesOf(stringPreferencesKey("deviceId") to b)
        PeerRegistry.write(prefs,listOf(sender));SetupStorage.write(prefs,state)
        SetupTransactions.paired(prefs,state,Peer(member.name,pin,c,emptyList()),null,true,true,now=100)
        assertEquals(accepted.readmits,SetupStorage.read(prefs).member(c)!!.readmits)
    }

    @Test fun freshPairOrdersAfterRetainedHistoryEvenIfTheWallClockMovedBackwards() {
        val high=removal.copy(hlc="1000000")
        val state=base.copy(roster=base.roster.filterNot {it.key==high.key}+high,barriers=mapOf(high.key to RemovalBarrier(high)))
        val prefs=mutablePreferencesOf(stringPreferencesKey("deviceId") to b)
        PeerRegistry.write(prefs,listOf(sender));SetupStorage.write(prefs,state)
        val subject=Peer(member.name,pin,c,emptyList(),setupDecision=SetupScope.ENABLED)
        SetupTransactions.paired(prefs,state,subject,null,true,true,now=0)
        PeerRegistry.write(prefs,listOf(sender,subject))
        SetupTransactions.tokenHello(prefs,DeliveryBinding(b,pin,c,subject.generation),SetupHello(SetupScope.ENABLED,state.setupId),self,0)
        val admitted=SetupStorage.read(prefs).member(c)!!
        assertEquals(MemberReadmits(high.hlc,high.writer),admitted.readmits)
        assertTrue(SetupReconcile.stamp(admitted)>SetupReconcile.stamp(high))
    }

}
