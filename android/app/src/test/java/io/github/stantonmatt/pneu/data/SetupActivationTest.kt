package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import org.junit.Test

class SetupActivationTest {
    private val a="1".repeat(32);private val b="2".repeat(32);private val c="3".repeat(32)
    private val s="0".repeat(32);private val t="f".repeat(32)
    private val self=SetupIdentity(a,"a".repeat(64),"Phone")
    private val peer=Peer("PC","b".repeat(64),b,emptyList(),generation=7,setupDecision=SetupScope.ENABLED,setupSupported=true)
    private val binding=DeliveryBinding(a,peer.pin,b,peer.generation)
    private val now=1800000000100L
    private fun data(state:SetupState=SetupState(),peer:Peer=this.peer)=mutablePreferencesOf(stringPreferencesKey("deviceId") to a).also {
        PeerRegistry.write(it,listOf(peer));SetupStorage.write(it,state)
    }
    private fun hello(p:MutablePreferences,remote:SetupHello,generation:Long=SetupStorage.read(p).generation,local:SetupHello=SetupStorage.read(p).hello(PeerRegistry.read(p).single {it.deviceId==b}.setupScope),id:()->String={s},token:Boolean=false)=
        SetupTransactions.hello(p,binding,generation,local,remote,self,now,id,tokenPairing=token)
    @Test fun setupIdCompatibilityIsSymmetricAfterConsentAndNeverMergesAssignedIds() {
        val absent=data()
        assertEquals(SetupHelloResult.RECONNECT,hello(absent,SetupHello(SetupScope.ENABLED),token=true))
        assertEquals(s,SetupStorage.read(absent).setupId);assertEquals(1L,SetupStorage.read(absent).generation)
        assertEquals(setOf(a,b),SetupStorage.read(absent).roster.map {it.deviceId}.toSet())
        val adopt=data();assertEquals(SetupHelloResult.RECONNECT,hello(adopt,SetupHello(SetupScope.ENABLED,s)))
        val assigned=SetupStorage.read(adopt)
        assertEquals(SetupHelloResult.SYNCHRONIZE,hello(adopt,SetupHello(SetupScope.ENABLED,s)))
        assertEquals(assigned,SetupStorage.read(adopt))
        assertEquals(SetupHelloResult.WAIT,hello(adopt,SetupHello(SetupScope.ENABLED)))
        assertEquals(SetupHelloResult.SEPARATE,hello(adopt,SetupHello(SetupScope.ENABLED,t)))
        assertEquals(s,SetupStorage.read(adopt).setupId);assertEquals(SetupScope.DISABLED,PeerRegistry.read(adopt).single().setupScope)
        assertEquals(setOf(b),SetupStorage.read(adopt).separateLinks)
        assertNull(SetupStorage.read(adopt).hello(SetupScope.DISABLED).setupId)
    }
    @Test fun tokenPairingOnlyLowerAuthenticatedIdFoundsAndCrashRecoveryReusesAssignment() {
        val higherSelf=SetupIdentity(c,"c".repeat(64),"Other phone")
        val p=data();assertEquals(SetupHelloResult.WAIT,SetupTransactions.hello(p,binding,0,SetupHello(SetupScope.ENABLED),SetupHello(SetupScope.ENABLED),higherSelf,now,{error("Must not mint")},tokenPairing=true))
        assertNull(SetupStorage.read(p).setupId)
        assertEquals(SetupHelloResult.RECONNECT,hello(p,SetupHello(SetupScope.ENABLED),id={s},token=true))
        val persisted=p.toPreferences();val reopened=persisted.toMutablePreferences()
        assertEquals(SetupHelloResult.WAIT,hello(reopened,SetupHello(SetupScope.ENABLED),id={error("Must not mint twice")}))
        assertEquals(s,SetupStorage.read(reopened).setupId)
        assertEquals(SetupHelloResult.SYNCHRONIZE,hello(reopened,SetupHello(SetupScope.ENABLED,s)))
    }
    @Test fun concurrentSAndTJoinsFirstDurableAssignmentWinsInBothOrders() {
        for((first,second) in listOf(s to t,t to s)) {
            val p=data();val before=p.toPreferences()
            // Failure before commit: discard staged mutable preferences.
            val failed=before.toMutablePreferences();hello(failed,SetupHello(SetupScope.ENABLED,first))
            assertNull(SetupStorage.read(before).setupId)
            hello(p,SetupHello(SetupScope.ENABLED,second));assertEquals(second,SetupStorage.read(p).setupId)
            assertThrows(SetupUnavailable::class.java) {hello(p,SetupHello(SetupScope.ENABLED,first),generation=0,local=SetupHello(SetupScope.ENABLED))}
            assertEquals(second,SetupStorage.read(p).setupId)
        }
    }
    @Test fun legacyAndSwitchOffNeverExportOrGrantConsent() {
        for(scope in listOf(SetupScope.DISABLED,SetupScope.ASK)) {
            val p=data(peer=peer.copy(setupDecision=scope));val before=PeerRegistry.read(p)
            val outcome=hello(p,SetupHello(SetupScope.ENABLED,s))
            assertEquals(if(scope==SetupScope.ASK)SetupHelloResult.WAIT else SetupHelloResult.DIRECT,outcome)
            assertNull(SetupStorage.read(p).setupId);assertTrue(SetupStorage.read(p).roster.isEmpty())
            assertEquals(before,PeerRegistry.read(p))
        }
        val p=data();assertEquals(SetupHelloResult.DIRECT,SetupTransactions.hello(p,binding,0,SetupHello(SetupScope.ENABLED),null,self,now))
        assertTrue(SetupStorage.read(p).roster.isEmpty())
        assertEquals(SetupHelloResult.DIRECT,hello(p,SetupHello(SetupScope.DISABLED)))
    }
    @Test fun migratedAskOffersOneDurablePromptWithoutRosterAndAllowNeedsRefreshedHello() {
        val p=data(peer=peer.copy(setupDecision=null))
        assertEquals(SetupScope.ASK,PeerRegistry.read(p).single().setupScope)
        assertEquals(SetupHelloResult.WAIT,hello(p,SetupHello(SetupScope.ASK)))
        assertTrue(SetupStorage.read(p).prompts.isEmpty())
        repeat(3) {hello(p,SetupHello(SetupScope.ENABLED));assertEquals(1,SetupStorage.read(p).prompts.size)}
        val state=SetupStorage.read(p);assertTrue(state.roster.isEmpty());assertNull(p[stringPreferencesKey("meshSeen")])
        assertTrue(state.receipts.isEmpty());assertEquals(state,SetupStorage.read(p.toPreferences()))
        val prompt=state.prompts.single();SetupTransactions.answer(p,prompt,true)
        assertEquals(SetupHelloResult.RECONNECT,hello(p,SetupHello(SetupScope.ENABLED),local=SetupHello(SetupScope.ASK)))
        assertEquals(SetupScope.ENABLED,PeerRegistry.read(p).single().setupScope);assertNull(SetupStorage.read(p).setupId)
        assertEquals(SetupHelloResult.WAIT,hello(p,SetupHello(SetupScope.ENABLED)))
        assertNull(SetupStorage.read(p).setupId)
        assertEquals(SetupHelloResult.RECONNECT,hello(p,SetupHello(SetupScope.ENABLED,s)))
        assertEquals(s,SetupStorage.read(p).setupId)
    }
    @Test fun staleGenerationBlocksSnapshotsPromptsAndAssignmentEvenSameIdRejoin() {
        val p=data();hello(p,SetupHello(SetupScope.ENABLED,s));val first=SetupStorage.read(p)
        val retired=SetupReconcile.retire(first,false);SetupStorage.write(p,retired)
        hello(p,SetupHello(SetupScope.ENABLED,s))
        assertEquals(s,SetupStorage.read(p).setupId);assertTrue(SetupStorage.read(p).generation>first.generation)
        assertThrows(SetupUnavailable::class.java) {checkSetup(p,binding,first.generation,s)}
        assertThrows(SetupUnavailable::class.java) {SetupTransactions.hello(p,binding,first.generation,first.hello(SetupScope.ENABLED),SetupHello(SetupScope.ENABLED,s),self,now)}
    }
    @Test fun scopedUnpairWithdrawsWithoutReciprocalRemovalsAndDisablesExportUntilFreshPair() {
        val p=data();hello(p,SetupHello(SetupScope.ENABLED,s))
        val plain=peer.copy(deviceId=c,pin="c".repeat(64),setupDecision=SetupScope.DISABLED)
        PeerRegistry.write(p,PeerRegistry.read(p)+plain)
        val forgotten=SetupTransactions.withdraw(p,binding,1)
        assertEquals(listOf(PairSettings.initialize(peer,a)),forgotten);val state=SetupStorage.read(p)
        assertNull(state.setupId);assertTrue(state.withdrawn);assertEquals(1L,state.withdrawalRevision)
        assertTrue(state.barriers.isEmpty());assertTrue(state.roster.isEmpty());assertEquals(listOf(PairSettings.initialize(plain,a)),PeerRegistry.read(p))
        assertTrue(PeerRegistry.resets(p).contains(b));assertTrue(state.sends.isEmpty())
        assertThrows(IllegalStateException::class.java) {SetupTransactions.paired(p,state,peer,null,true,false,now=now)}
        SetupTransactions.paired(p,state,peer,null,true,true,now=now)
        assertFalse(SetupStorage.read(p).withdrawn)
    }
    @Test fun retiredRandomFoundingIdsAreNeverReusedAndCapacityFailureIsAtomicAtCommitBoundary() {
        val p=data(SetupState(retiredIds=setOf(s)))
        var calls=0;hello(p,SetupHello(SetupScope.ENABLED),id={if(calls++==0)s else t},token=true)
        assertEquals(t,SetupStorage.read(p).setupId);assertEquals(2,calls)
        val staged=p.toPreferences().toMutablePreferences()
        SetupTransactions.remove(staged,"$b/${peer.pin}",1,now)
        assertEquals(t,SetupStorage.read(p).setupId);assertNull(SetupStorage.read(staged).setupId)
    }
    @Test fun withdrawalPreservesUnscopedDirectTrafficAndOwnDenyRequiresFreshPair() {
        val own=SetupMember(a,self.pin,"phone","Phone","removed","10",b)
        val p=data(SetupState(withdrawn=true,withdrawalRevision=1,barriers=mapOf(own.key to RemovalBarrier(own))),peer.copy(setupDecision=SetupScope.DISABLED))
        assertEquals(SetupHelloResult.DIRECT,hello(p,SetupHello(SetupScope.ENABLED,s)))
        val state=SetupStorage.read(p)
        SetupTransactions.paired(p,state,peer,null,true,true,self.pin,now)
        assertFalse(SetupStorage.read(p).withdrawn);assertFalse(SetupStorage.read(p).denies(a,self.pin))
        assertEquals(own,SetupStorage.read(p).barriers.getValue(own.key).removed)
    }

}
