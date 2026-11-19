package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import org.junit.Test

class SetupLifecycleTest {
    private val a="1".repeat(32);private val b="2".repeat(32);private val c="3".repeat(32)
    private val pin="b".repeat(64)
    private val invitation=Invitation(1,"PC",pin,emptyList(),"f".repeat(64),100)
    private val prefs=Prefs(deviceId=a)
    private val removal=SetupMember(b,pin,"computer","PC","removed","10",a)
    @Test fun setupUnauthorizedNeverRevokesBeforeOrAfterReadinessWhilePlainDirectStillDoes() {
        for(established in listOf(false,true)) {
            val peer=Peer("PC",pin,b,emptyList(),setupDecision=SetupScope.ENABLED,established=established,setupSupported=true)
            val streak=RejectionStreak()
            repeat(4) {assertFalse(streak.record(b,PairingRejection.unauthorized("unauthorized",true,peer.setupScoped)))}
            assertFalse(PairingRejection.unauthorized("unauthorized",false,true))
        }
        assertTrue(PairingRejection.unauthorized("unauthorized",true,false))
        assertFalse(PairingRejection.unauthorized("connection_replaced",true,false))
    }
    @Test fun preRemovalRecoveryCannotRefreshFrozenFenceAndUnrelatedRemovalDoesNotCancelAttempt() {
        val attempt=PairingAttempt("attempt",invitation,prefs,emptyMap(),setupEnabled=false)
        assertFalse(attempt.setupEnabled)
        val unrelated=removal.copy(deviceId=c,fingerprint="c".repeat(64))
        val changed=prefs.copy(membership=SetupState(barriers=mapOf(unrelated.key to RemovalBarrier(unrelated))))
        assertTrue(attempt.matches(changed,emptyMap()))
        val denied=prefs.copy(membership=SetupState(barriers=mapOf(removal.key to RemovalBarrier(removal))))
        assertFalse(attempt.matches(denied,emptyMap()))
        assertFalse(attempt.copy(deviceId=b).matches(denied,emptyMap()))
        val fresh=PairingAttempt("new-confirmation",invitation,denied,emptyMap(),deviceId=b)
        assertTrue(fresh.matches(denied,emptyMap()));assertTrue(fresh.setupEnabled)
        assertEquals(attempt.scanned,attempt.copy(deviceId=b).scanned)
        assertFalse(attempt.matches(prefs.copy(membership=SetupState(withdrawalRevision=1)),emptyMap()))
    }
    @Test fun oldPinRemovalCannotFenceKnownReplacementAttemptOrChangeItsScopeChoice() {
        val replacement=invitation.copy(fingerprint="d".repeat(64))
        val attempt=PairingAttempt("replacement",replacement,prefs,emptyMap(),deviceId=b,setupEnabled=false)
        val removedOld=prefs.copy(membership=SetupState(barriers=mapOf(removal.key to RemovalBarrier(removal))))
        assertTrue(attempt.matches(removedOld,emptyMap()))
        assertFalse(attempt.copy(deviceId=b).setupEnabled)
    }
    @Test fun assignmentDoesNotRefreshPairingFenceAndScopeGenerationChangeReconnects() {
        val peer=Peer("PC",pin,b,emptyList(),generation=7,setupDecision=SetupScope.ENABLED)
        val original=prefs.copy(peers=listOf(peer),peer=peer)
        val attempt=PairingAttempt("attempt",invitation,original,emptyMap(),deviceId=b)
        val assigned=original.copy(membership=SetupState("0".repeat(32),1))
        assertTrue(attempt.matches(assigned,emptyMap()))
        assertTrue(BindingTransitions.restart(original,assigned,DeliveryBinding.from(original,b)))
        assertFalse(attempt.matches(assigned.copy(peers=listOf(peer.copy(generation=8))),emptyMap()))
    }
    @Test fun tombstoneCancelsOnlyTheSameIncarnationAttemptIncludingBeforeHello() {
        assertTrue(SetupAttemptTarget(null,pin).matchesRemoval(removal.key))
        assertTrue(SetupAttemptTarget(b,pin).matchesRemoval(removal.key))
        assertFalse(SetupAttemptTarget(b,"d".repeat(64)).matchesRemoval(removal.key))
        assertFalse(SetupAttemptTarget(c,pin).matchesRemoval(removal.key))
    }

    @Test fun immediateMembershipRecoveryIsBoundedAndCannotApplyBeforeBindingPersistence() {
        val buffer=PairingControlBuffer(true)
        val members=Frame.Control(Type.MEMBERS,json("transfer_id" to "4".repeat(32),"setup_id" to "0".repeat(32),"members" to emptyList<String>()))
        repeat(4) {buffer.hold(Frame.Control(Type.STATUS,json("paused" to false)))}
        buffer.hold(members)
        assertThrows(IllegalStateException::class.java) {buffer.afterInstall(false)}
        assertThrows(IllegalArgumentException::class.java) {buffer.hold(members)}
        assertThrows(IllegalArgumentException::class.java) {buffer.hold(Frame.Control(Type.STATUS,json("paused" to true)))}
        assertEquals(5,buffer.afterInstall(true).size);assertTrue(buffer.afterInstall(true).isEmpty())
        assertThrows(IllegalArgumentException::class.java) {PairingControlBuffer(false).hold(members)}
        val oversized=members.copy(body=members.body.deepCopy().apply {addProperty("unknown","x".repeat(65536))})
        assertThrows(IllegalArgumentException::class.java) {PairingControlBuffer(true).hold(oversized)}
    }

}
