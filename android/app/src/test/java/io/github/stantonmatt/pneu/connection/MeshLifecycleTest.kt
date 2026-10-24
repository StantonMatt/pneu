package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.test.*
import org.junit.Assert.*
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class MeshLifecycleTest {
    private val peer=Peer("A","pin","pc",emptyList(),generation=1)
    private val prefs=Prefs(deviceId="phone",peers=listOf(peer),peer=peer)
    @Test fun lostPairCanPassBothServiceAndWorkerGateOnlyWithAnInvitation() {
        val lost=prefs.copy(peers=listOf(peer.copy(pairingLost=true)),peer=peer.copy(pairingLost=true),pairingLost=true)
        assertFalse(BindingTransitions.hasWork(lost));assertFalse(BindingTransitions.mayDial(lost))
        val repair=PairingAttempt("repair",Invitation(1,"A","pin",emptyList(),"token",123),lost,emptyMap())
        assertTrue(BindingTransitions.hasWork(lost,repair));assertTrue(BindingTransitions.mayDial(lost,repair))
    }
    @Test fun ownInstallationDoesNotRestartButExternalRepairRevokesTheOldSession() {
        val old=prefs
        val installed=prefs.copy(peer=peer.copy(generation=2),peers=listOf(peer.copy(generation=2)))
        val binding=DeliveryBinding.from(installed)!!
        assertFalse(BindingTransitions.restart(old,installed,binding))
        val external=installed.copy(peer=peer.copy(generation=3),peers=listOf(peer.copy(generation=3)))
        assertTrue(BindingTransitions.restart(installed,external,binding))
        assertFalse(binding.matches(external))
        assertFalse(BindingTransitions.owns(external,binding))
        assertTrue(BindingTransitions.restart(installed,installed.copy(mobile=false),binding))
    }
    @Test fun settingsTaskAlwaysClosesItsSocketOnFailureOrCancellation()=runTest {
        var closed=0
        try {SessionTasks.settings({closed++}) {throw IllegalStateException("fatal")};fail()}catch(_:IllegalStateException) {}
        assertEquals(1,closed)
        val entered=CompletableDeferred<Unit>()
        val task=launch {SessionTasks.settings({closed++}) {entered.complete(Unit);awaitCancellation()}}
        entered.await();task.cancelAndJoin();assertEquals(2,closed)
    }
    @Test fun settingsBlockedImagesAndAutomaticFilesResumeOnlyAfterTheDirectionChangesAndIsSupported() {
        val initialized=PairSettings.initialize(peer,"phone")
        val row=Outgoing().apply {kind="photo";automatic=true;error="unsupported_kind";rejectedCapabilities="settings:"+PairSettings.signature(initialized)}
        assertFalse(SettingsDeliveryPolicy.releasable(row,initialized,setOf("files")))
        val changed=PairSettings.initialize(initialized.copy(settings=initialized.settings.mapValues {(_,v)->v.copy(hlc="2")}),"phone")
        assertTrue(SettingsDeliveryPolicy.releasable(row,changed,setOf("files")))
        assertFalse(SettingsDeliveryPolicy.releasable(row,changed.copy(automaticMedia=false),setOf("files")))
        assertFalse(SettingsDeliveryPolicy.releasable(row,changed,emptySet()))
        row.automatic=false;row.kind="clipboard_image"
        assertFalse(SettingsDeliveryPolicy.releasable(row,changed,setOf("files")))
        assertTrue(SettingsDeliveryPolicy.releasable(row,changed,setOf("files","clipboard_image")))
        assertFalse(SettingsDeliveryPolicy.releasable(row,changed.copy(clipboardSend=false),setOf("files","clipboard_image")))
    }
    @Test fun rejectedAttemptsRetainEmissionSettingsAcrossInFlightDirectionChanges() {
        val initial=PairSettings.initialize(peer,"phone")
        val changed=PairSettings.initialize(initial.copy(settings=initial.settings.mapValues {(_,field)->field.copy(hlc="2")}),"phone")
        for((kind,automatic) in listOf("photo" to true,"file" to true,"clipboard_image" to false,"text" to false,"file" to false)) {
            val row=Outgoing().apply {this.kind=kind;this.automatic=automatic;error="unsupported_kind";nextAttempt=Long.MAX_VALUE}
            val capabilities=setOf("files","clipboard_image","clipboard_text","mesh")
            val attempt=QueuedEmission(row,DeliveryBinding("phone","pin","pc",1))
            assertTrue(attempt.begin(initial,capabilities))
            assertFalse(attempt.begin(changed,capabilities))
            row.rejectedCapabilities=attempt.rejectedCapabilities
            val settingsGated=automatic || kind=="clipboard_image"
            assertEquals(settingsGated,attempt.rejectedCapabilities.startsWith("settings:"))
            if(settingsGated)assertEquals("settings:"+PairSettings.signature(initial),row.rejectedCapabilities)
            else assertEquals(capabilities.sorted().joinToString(","),row.rejectedCapabilities)
            assertEquals(settingsGated,SettingsDeliveryPolicy.releasable(row,changed,capabilities))
            val retry=QueuedEmission(row,attempt.binding)
            assertTrue(retry.begin(changed,capabilities))
            row.rejectedCapabilities=retry.rejectedCapabilities
            assertFalse(SettingsDeliveryPolicy.releasable(row,changed,capabilities))
        }
    }
    @Test fun legacyRejectionsCaptureCapabilitiesAndCancelledEmissionCannotCaptureSettings() {
        val row=Outgoing().apply {kind="clipboard_image";automatic=false}
        val caps=setOf("clipboard_image","files")
        val emission=QueuedEmission(row,DeliveryBinding("phone","pin","pc",1))
        assertTrue(emission.begin(PairSettings.initialize(peer,"phone"),caps))
        assertEquals("clipboard_image,files",emission.rejectedCapabilities)
        val cancelled=QueuedEmission(row,emission.binding)
        assertFalse(cancelled.cancel());assertFalse(cancelled.begin(peer,caps))
        assertEquals("",cancelled.rejectedCapabilities)
    }
    @Test fun frozenAttemptSelectsOnlyItsTargetAndRetainsRemovalTombstones() {
        val invitation=Invitation(1,"A","new-pin",emptyList(),"token",123)
        val b=peer.copy(deviceId="b",pin="b-pin")
        val scan=prefs.copy(peers=listOf(peer,b))
        val attempt=PairingAttempt("first",invitation,scan,emptyMap()).copy(deviceId="pc")
        assertTrue(attempt.matches(scan,emptyMap()))
        assertTrue(attempt.matches(scan.copy(peers=listOf(peer)),mapOf("id:b" to 1L)))
        assertFalse(attempt.matches(scan.copy(peers=listOf(b)),mapOf("id:pc" to 1L)))
        assertFalse(attempt.matches(scan,mapOf("id:pc" to 2L))) // remove and restore
        assertFalse(attempt.matches(scan.copy(deviceId="other"),emptyMap()))
        assertFalse(attempt.matches(scan.copy(peers=listOf(peer.copy(generation=2),b)),emptyMap()))
        val retry=attempt.copy(id="second")
        assertNotEquals(attempt.id,retry.id)
        assertEquals(attempt.scanned,retry.scanned)
    }
    @Test fun invitationAddApprovalDoesNotSelectAnUnrelatedFirstComputer() {
        val invitation=Invitation(1,"B","new-pin",emptyList(),"token",123)
        assertThrows(IllegalStateException::class.java) {ConfirmedBinding.forInvitation(prefs.copy(loaded=false),invitation)}
        assertThrows(IllegalStateException::class.java) {ConfirmedBinding.forInvitation(prefs.copy(deviceId=""),invitation)}
        val approval=ConfirmedBinding.forInvitation(prefs,invitation)
        assertNull(approval.pin);assertNull(approval.pcId);assertTrue(approval.matches(prefs))
        assertFalse(approval.matches(prefs.copy(pairingRevision=prefs.pairingRevision+1)))
        val repair=ConfirmedBinding.forInvitation(prefs,invitation.copy(fingerprint=peer.pin))
        assertEquals(peer.deviceId,repair.pcId);assertTrue(repair.matches(prefs))
    }
    @Test fun cancellationAndRetryTransitionsAreScopedToTheInvitationsWorker() {
        val b=peer.copy(pin="pin-b",deviceId="pc-b")
        val before=prefs.copy(peers=listOf(peer,b))
        val after=before
        assertFalse(BindingTransitions.restart(BindingTransitions.scoped(before,"pin-b"),BindingTransitions.scoped(after,"pin-b"),DeliveryBinding.from(before,b.deviceId)))
        assertTrue(BindingTransitions.mayDial(BindingTransitions.scoped(after,"pin-b")))
    }
    @Test fun removalClearsOldSuccessSynchronouslyAndFailureFeedbackPreservesTheHealthyPeersRoute() {
        val connected=LinkState("Connected","Wi-Fi",pairing="Paired with A",pairingPin="pin")
        val states=mapOf("pin" to connected)
        val removed=Prefs(deviceId="phone")
        assertEquals("",LinkProjection.aggregate(removed,states,null).pairing)
        assertEquals("",LinkProjection.aggregate(removed,states,null).route)
        val failure=LinkState(pairing="Can't reach your PC")
        val projected=LinkProjection.aggregate(prefs,states,failure)
        assertEquals("Connected",projected.status);assertEquals("Wi-Fi",projected.route)
        assertSame(failure.pairingFeedback,projected.pairingFeedback)
    }

    @Test fun preUpgradeSettingsRejectionsAreRevalidatedOnceAfterSuccessfulSync() {
        val initialized=PairSettings.initialize(peer,"phone")
        val row=Outgoing().apply {kind="photo";automatic=true;error="unsupported_kind";rejectedCapabilities="files,mesh"}
        assertFalse(SettingsDeliveryPolicy.releasable(row,initialized.copy(automaticMedia=false),setOf("files","mesh")))
        assertTrue(SettingsDeliveryPolicy.releasable(row,initialized,setOf("files","mesh")))
        row.automatic=false;row.kind="file"
        assertFalse(SettingsDeliveryPolicy.releasable(row,initialized,setOf("files","mesh")))
    }

    @Test fun secondaryPeerPairingFeedbackUsesItsCapturedGenerationAndCannotSurviveItsRemoval() {
        val b=peer.copy(pin="pin-b",deviceId="pc-b",generation=2)
        val p=prefs.copy(peers=listOf(peer,b))
        val states=mapOf("pin" to LinkState("Connected","Wi-Fi"),"pin-b" to LinkState("Connected","Tailscale",pairing="Paired with B",pairingPin="pin-b",pairingGeneration=2))
        val link=LinkProjection.aggregate(p,states,null,"pin-b")
        assertEquals("Paired with B",link.pairing);assertTrue(link.pairingSuccess(p))
        assertFalse(link.pairingSuccess(p.copy(peers=listOf(peer,b.copy(generation=3)))))
        assertEquals("",LinkProjection.aggregate(p.copy(peers=listOf(peer)),states,null,"pin-b").pairing)
    }

}
