package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import org.junit.Test

/** Actual endpoint transactions, with HELLO delivery controlled independently
 * from local actions and durable commits. No roster gossip is needed to join. */
class SetupMigrationTest {
    private val s="a".repeat(32);private val t="b".repeat(32)
    private val now=1800000000100L
    private class Node(val id:String,val pin:String,val name:String) {
        var p=mutablePreferencesOf(stringPreferencesKey("deviceId") to id)
        val self get()=SetupIdentity(id,pin,name,if(name=="A")"phone" else "computer")
        val state get()=SetupStorage.read(p)
        val peers get()=PeerRegistry.read(p)
        fun binding(other:Node)=peers.single {it.deviceId==other.id}.let {DeliveryBinding(id,it.pin,it.deviceId,it.generation)}
        fun wire(other:Node)=state.hello(peers.single {it.deviceId==other.id}.setupScope)
        fun confirmed(other:Node)=ConfirmedBinding.from(Prefs(deviceId=id,peers=peers),other.id)
    }
    private fun nodes(vararg ids:Int):List<Node> {
        val nodes=ids.mapIndexed {i,id->Node(id.toString(16).padStart(32,'0'),id.toString(16).padStart(64,'0'),('A'+i).toString())}
        for(node in nodes)PeerRegistry.write(node.p,nodes.filter {it!=node}.mapIndexed {i,other->
            Peer(other.name,other.pin,other.id,emptyList(),role=other.self.role,generation=(i+7).toLong(),setupDecision=SetupScope.ASK)})
        return nodes
    }
    private fun hello(to:Node,from:Node,wire:SetupHello=from.wire(to),discoveryId:String?=to.state.pendingLink?.id)=
        SetupTransactions.hello(to.p,to.binding(from),to.state.generation,to.wire(from),wire,to.self,now,
            {error("Migration HELLO must never found")},discoveryId=discoveryId)
    private fun link(node:Node,selected:Node)=SetupTransactions.link(node.p,node.confirmed(selected))
    private fun finish(node:Node,id:String=s)=SetupTransactions.finishLinkDiscovery(node.p,requireNotNull(node.state.pendingLink).id,node.self,now,{id})
    private fun allow(node:Node) {
        val prompt=node.state.prompts.first()
        SetupTransactions.answer(node.p,prompt,true)
        assertEquals(SetupScope.ASK,node.peers.single {it.deviceId==prompt.peerId}.setupScope)
        // Deliver the refreshed compatible HELLO before the enabled reconnect.
        SetupTransactions.hello(node.p,node.peers.single {it.deviceId==prompt.peerId}.let {
            DeliveryBinding(node.id,it.pin,it.deviceId,it.generation)},node.state.generation,
            node.state.hello(SetupScope.ASK),SetupHello(SetupScope.ENABLED,s),node.self,now)
    }
    private fun enable(node:Node,other:Node,scope:SetupScope)=PeerRegistry.write(node.p,node.peers.map {if(it.deviceId==other.id)it.copy(setupDecision=scope) else it})
    private fun assertAssigned(node:Node,id:String=s) {assertEquals(id,node.state.setupId);assertEquals(1L,node.state.generation);assertNull(node.state.pendingLink)}
    private fun sync(left:Node,right:Node) {
        assertEquals(SetupHelloResult.SYNCHRONIZE,hello(left,right));assertEquals(SetupHelloResult.SYNCHRONIZE,hello(right,left))
    }
    @Test fun threeActionMigrationResolvesAllPromptsAndPreservesBindingsWithoutGossip() {
        // A need not have the lowest authenticated ID.
        val (a,b,c,d)=nodes(4,3,2,1)
        for(node in listOf(a,b,c))enable(node,d,SetupScope.DISABLED)
        val originals=listOf(a,b,c).associateWith {it.peers}
        link(a,b)
        assertTrue(a.peers.filter {it.deviceId!=d.id}.all {it.setupScope==SetupScope.ENABLED})
        assertTrue(finish(a));assertAssigned(a)
        assertEquals(listOf(a.id),a.state.roster.map {it.deviceId})
        hello(b,a);hello(c,a);allow(b)
        assertTrue(b.state.prompts.isEmpty());assertEquals(SetupHelloResult.RECONNECT,hello(b,a));assertAssigned(b)
        hello(c,b);assertEquals(2,c.state.prompts.size)
        // Prompt dismissal/restart retains both offers; one Allow resolves both.
        c.p=c.p.toPreferences().toMutablePreferences();allow(c)
        assertTrue(c.state.prompts.isEmpty());hello(c,b);assertAssigned(c)
        for((left,right) in listOf(a to b,a to c,b to c))sync(left,right)
        for(node in listOf(a,b,c)) {
            assertEquals(originals.getValue(node),node.peers.map {peer->peer.copy(setupDecision=originals.getValue(node).single {it.deviceId==peer.deviceId}.setupScope)})
            assertEquals(SetupScope.DISABLED,node.peers.single {it.deviceId==d.id}.setupScope)
            assertTrue(PeerRegistry.resets(node.p).isEmpty())
        }
    }
    @Test fun reorderedMigrationHellosACBBothDeliveryOrdersNeverFoundOnAbsentAbsent() {
        for(bcFirst in listOf(true,false))for(bFirst in listOf(true,false)) {
            val (a,b,c)=nodes(1,3,2);link(a,b);finish(a)
            hello(b,a);hello(c,a);allow(b);allow(c)
            if(bcFirst) {
                assertEquals(SetupHelloResult.WAIT,hello(b,c));assertEquals(SetupHelloResult.WAIT,hello(c,b))
                assertNull(b.state.setupId);assertNull(c.state.setupId)
            }
            val first=if(bFirst)b else c;val second=if(bFirst)c else b
            hello(first,a)
            if(bcFirst)hello(second,first) else hello(second,a)
            assertAssigned(first);assertAssigned(second);sync(b,c)
        }
    }
    @Test fun migrationFounderLossJoinsThroughAnotherEnabledMember() {
        val (a,b,c)=nodes(1,3,2);link(a,b);finish(a)
        hello(b,a);hello(c,a);allow(b);allow(c)
        hello(c,a);assertNull(b.state.setupId)
        // A vanishes before B gets another HELLO. C supplies S without a proposer.
        assertEquals(SetupHelloResult.RECONNECT,hello(b,c));assertAssigned(b);sync(b,c)
    }
    @Test fun concurrentMigrationAllowsSerializeAssignmentsInBothCommitOrders() {
        for(bFirst in listOf(true,false)) {
            val (a,b,c)=nodes(1,3,2);link(a,b);finish(a)
            hello(b,a);hello(c,a);allow(b);allow(c)
            val absentB=b.wire(c);val absentC=c.wire(b)
            hello(b,c,absentC);hello(c,b,absentB)
            val first=if(bFirst)b else c;val second=if(bFirst)c else b
            hello(first,a);hello(second,a)
            hello(b,c,absentC);hello(c,b,absentB)
            assertAssigned(b);assertAssigned(c);assertTrue(b.state.prompts.isEmpty());assertTrue(c.state.prompts.isEmpty());sync(b,c)
        }
    }
    @Test fun linkJoinsBeforeFoundingAndLateDifferentIdStaysDirect() {
        val (a,b,c)=nodes(1,2,3);link(a,b);finish(a)
        enable(b,a,SetupScope.DISABLED);link(b,a)
        assertNotNull(b.state.pendingLink);assertEquals(SetupScope.ENABLED,b.peers.single {it.deviceId==a.id}.setupScope)
        val pending=b.state.pendingLink!!.id
        assertEquals(SetupHelloResult.RECONNECT,hello(b,a));assertAssigned(b)
        assertFalse(SetupTransactions.finishLinkDiscovery(b.p,pending,b.self,now,{error("Must join instead")}))
        link(c,a);finish(c,t)
        assertEquals(SetupHelloResult.SEPARATE,hello(b,c));assertAssigned(b);assertEquals(t,c.state.setupId)
        assertFalse(b.state.roster.any {it.deviceId==c.id})
    }
    @Test fun linkDefersToAskAdvertisementPastWindowThenJoinsOnAllow() {
        val (a,b,c)=nodes(1,2,3);link(a,b);finish(a);enable(a,b,SetupScope.ASK)
        link(b,a)
        assertEquals(SetupHelloResult.WAIT,hello(b,a));assertEquals("A",b.state.pendingLink!!.waitingFor);assertEquals("Waiting for A",b.state.linkStatus)
        assertFalse(finish(b));assertNull(b.state.setupId);assertNotNull(b.state.pendingLink)
        hello(a,b);allow(a)
        assertEquals(SetupScope.ASK,c.peers.single {it.deviceId==a.id}.setupScope)
        hello(b,a);assertAssigned(b);sync(a,b)
    }
    @Test fun repeatedLinkRetainsKnownIdAndFencesThePreviousDiscoveryTimer() {
        val (a,b)=nodes(1,2);link(a,b);finish(a);enable(a,b,SetupScope.ASK)
        link(b,a);hello(b,a)
        val previous=b.state.pendingLink!!.id
        link(b,a)
        assertNotEquals(previous,b.state.pendingLink!!.id)
        assertEquals("Waiting for A",b.state.linkStatus)
        assertFalse(SetupTransactions.finishLinkDiscovery(b.p,previous,b.self,now,{error("Stale action")}))
        assertFalse(finish(b));assertNull(b.state.setupId)
    }
    @Test fun stalePreLinkHellosCannotSupplyDiscoveryEvidenceOrPermitFoundingEarly() {
        val (a,b)=nodes(1,2);link(a,b);finish(a);enable(b,a,SetupScope.ENABLED)
        link(b,a)
        assertEquals(SetupHelloResult.RECONNECT,hello(b,a,discoveryId=null))
        assertTrue(b.state.pendingLink!!.advertised.isEmpty());assertNull(b.state.setupId)
        hello(b,a);assertAssigned(b)
    }
    @Test fun linkWithPendingPromptActsAsAllowAndNeverCreatesFoundingRecord() {
        val (a,b,c)=nodes(1,2,3);link(a,b);finish(a);hello(b,a)
        link(b,c)
        assertTrue(b.state.prompts.single().allowRequested);assertNull(b.state.pendingLink);assertNull(b.state.setupId)
        assertTrue(b.peers.all {it.setupScope==SetupScope.ASK})
        hello(b,a)
        assertTrue(b.state.prompts.isEmpty());assertTrue(b.peers.all {it.setupScope==SetupScope.ENABLED})
        hello(b,c);assertNull(b.state.setupId)
        hello(b,a);assertAssigned(b)
    }
    @Test fun independentLinkTapsFoundSelfOnlySetupsAndNeverMergeInEitherDeliveryOrder() {
        for(aFirst in listOf(true,false)) {
            val (a,b)=nodes(1,2);link(a,b);link(b,a)
            // Hold all HELLOs until both local discovery windows have expired.
            assertTrue(finish(a,s));assertTrue(finish(b,t))
            assertEquals(listOf(a.id),a.state.roster.map {it.deviceId});assertEquals(listOf(b.id),b.state.roster.map {it.deviceId})
            val aWire=a.wire(b);val bWire=b.wire(a)
            if(aFirst) {hello(a,b,bWire);hello(b,a,aWire)} else {hello(b,a,aWire);hello(a,b,bWire)}
            assertAssigned(a,s);assertAssigned(b,t);assertTrue(a.state.separateLinks.contains(b.id));assertTrue(b.state.separateLinks.contains(a.id))
        }
    }
    @Test fun tokenPairingRacingMigrationJoinFirstDurableAssignmentWinsWithEitherProposer() {
        for(bLower in listOf(true,false))for(migrationFirst in listOf(true,false)) {
            val (a,b,d,unrelated)=nodes(1,3,if(bLower)4 else 2,5)
            link(a,b);finish(a);enable(b,a,SetupScope.ENABLED)
            // A token confirmation only enables the token binding, never unrelated ask links.
            enable(b,d,SetupScope.ENABLED);enable(d,b,SetupScope.ENABLED)
            fun token(node:Node,other:Node,wire:SetupHello)=SetupTransactions.tokenHello(node.p,node.binding(other),wire,node.self,now,{t})
            val bAbsent=b.wire(d);val dAbsent=d.wire(b)
            if(!bLower) {token(d,b,bAbsent);assertAssigned(d,t)}
            if(migrationFirst)hello(b,a)
            token(b,d,if(bLower)dAbsent else d.wire(b))
            if(!migrationFirst)hello(b,a)
            assertAssigned(b,if(migrationFirst)s else t)
            assertEquals(SetupScope.ASK,b.peers.single {it.deviceId==unrelated.id}.setupScope)
            if(bLower)token(d,b,b.wire(d))
            if(d.state.setupId!=null && d.state.setupId!=b.state.setupId) {
                hello(b,d);hello(d,b)
            }
            val winner=b.state.setupId!!;val beforeRejected=b.p.toPreferences()
            assertThrows(SetupUnavailable::class.java) {checkSetup(b.p,b.binding(if(migrationFirst)d else a),b.state.generation,if(winner==s)t else s)}
            assertEquals(beforeRejected,b.p.toPreferences());assertTrue(b.state.receipts.isEmpty())
        }
    }
    @Test fun interruptedLinkRestartsDiscoveryAndEitherJoinsOrFoundsWithSameConsent() {
        for(advertised in listOf(true,false)) {
            val (a,b)=nodes(1,2);link(b,a)
            val persisted=b.p.toPreferences();val pending=b.state.pendingLink
            b.p=persisted.toMutablePreferences()
            assertEquals(pending,b.state.pendingLink);assertEquals(SetupScope.ENABLED,b.peers.single().setupScope)
            if(advertised) {link(a,b);finish(a);hello(b,a)} else assertTrue(finish(b))
            assertAssigned(b)
        }
    }
}
