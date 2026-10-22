package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*
import org.junit.Assert.*
import org.junit.Test

class EmissionPolicyTest {
    private val prefs=Prefs(deviceId="phone",peer=Peer("PC","pin","pc",emptyList()),originals=true)
    private val binding=DeliveryBinding.from(prefs)!!
    private fun row(kind:String="file",automatic:Boolean=false)=Outgoing().apply {
        mediaId="media";transferId="transfer";revision=3;this.kind=kind;this.automatic=automatic
        mime=if(kind=="text")"text/plain" else "image/png"
    }
    private fun eligible(row:Outgoing,current:Outgoing?=row,p:Prefs=prefs,route:Boolean=true,mobile:Boolean=false,
                         caps:Set<String> = setOf("files","clipboard_text","clipboard_image"),newer:Boolean=false)=
        EmissionPolicy.eligible(row,current,p,binding,route,mobile,caps,newer)
    @Test fun bothLanesRequireTheSelectedRevisionAndLiveQueueRow() {
        for(kind in listOf("text","file")) {
            val selected=row(kind)
            assertTrue(eligible(selected))
            assertFalse(eligible(selected,null))
            for(state in listOf("discarded","superseded","skipped","sent","sending")) {
                assertFalse(eligible(selected,row(kind).apply {this.state=state}))
            }
            assertFalse(eligible(selected,row(kind).apply {revision++}))
            assertFalse(eligible(selected,row(kind).apply {transferId="reencoded"}))
            assertTrue(eligible(selected,row(kind).apply {state="failed"}))
        }
        assertFalse(eligible(row("text"),newer=true))
    }
    @Test fun firstFrameUsesCurrentBindingTransportCapabilitiesAndPausePolicy() {
        for(kind in listOf("file","text")) {
            val selected=row(kind)
            for(p in listOf(prefs.copy(deviceId="rotated"),prefs.copy(peer=null),prefs.copy(pairingLost=true),
                prefs.copy(deliveryReset=true),prefs.copy(peer=prefs.peer!!.copy(deviceId="replacement"))))assertFalse(eligible(selected,p=p))
            assertFalse(eligible(selected,route=false))
            assertFalse(eligible(selected,caps=emptySet()))
            assertTrue(eligible(selected,p=prefs.copy(paused=true))) // manual sends
        }
        assertFalse(eligible(row(automatic=true),p=prefs.copy(paused=true)))
        assertFalse(eligible(row("clipboard_image"),caps=setOf("files")))
    }
    @Test fun currentCostPolicyCanHoldOrRequirePreparationButNeverReencodeSnapshots() {
        val p=prefs.copy(originals=false)
        assertFalse(eligible(row("recording",true),p=p,mobile=true))
        assertFalse(eligible(row("photo",true),p=p,mobile=true)) // encoding now required
        assertFalse(eligible(row("photo",true).apply {attempted=true},p=p,mobile=true))
        assertTrue(eligible(row("photo",true).apply {optimized=true},p=p,mobile=true))
        assertTrue(eligible(row(),p=p,mobile=true)) // unchanged shared file
    }
    @Test fun meshFirstFrameWaitsForTheExactCurrentSettingsToSynchronizeEvenIfReadyFlagLags() {
        val initial=PairSettings.initialize(prefs.peer!!,"phone")
        val confirmed=initial.copy(confirmedSettings=PairSettings.signature(initial))
        val changed=PairSettings.initialize(confirmed.copy(settings=confirmed.settings.mapValues {(_,field)->field.copy(hlc="2")}),"phone")
        val caps=setOf("mesh","files","clipboard_text","clipboard_image")
        for((kind,automatic) in listOf("text" to false,"file" to false,"photo" to true,"clipboard_image" to false)) {
            val selected=row(kind,automatic)
            assertTrue(eligible(selected,p=prefs.copy(peer=confirmed),caps=caps))
            assertFalse("Unsynced $kind cannot emit",eligible(selected,p=prefs.copy(peer=changed),caps=caps))
            val synced=changed.copy(confirmedSettings=PairSettings.signature(changed))
            assertTrue(eligible(selected,p=prefs.copy(peer=synced),caps=caps))
            assertTrue("Legacy has no settings handshake",eligible(selected,p=prefs.copy(peer=changed)))
        }
    }

    @Test fun cancellationBeforeEmissionPreventsBothLanesAndAfterEmissionKeepsTheReceipt() {
        for(kind in listOf("file","text")) {
            val queued=QueuedEmission(row(kind),binding)
            assertFalse(queued.cancel());assertFalse(queued.begin());assertFalse(queued.cancel())
            val emitted=QueuedEmission(row(kind),binding)
            assertTrue(emitted.begin());assertTrue(emitted.cancel());assertFalse(emitted.begin())
        }
    }
}
