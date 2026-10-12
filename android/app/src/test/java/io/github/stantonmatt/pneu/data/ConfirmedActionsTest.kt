package io.github.stantonmatt.pneu.data

import org.junit.Assert.*
import org.junit.Test

class ConfirmedActionsTest {
    private val original=Prefs(deviceId="phone",peer=Peer("Original","pin","pc",emptyList()),pairingRevision=7)
    @Test fun identityIncludesPhonePcAndSamePcRepairRevision() {
        val selected=ConfirmedBinding.from(original)
        assertTrue(selected.matches(original));assertTrue(selected.matches(original.copy(paused=true)))
        for(changed in listOf(original.copy(deviceId="replacement-phone"),original.copy(peer=null),original.copy(peer=original.peer!!.copy(pin="new-pin")),original.copy(peer=original.peer.copy(deviceId="new-pc")),original.copy(pairingRevision=8),original.copy(loaded=false)))assertFalse(selected.matches(changed))
        assertTrue(runCatching {ConfirmedBinding.from(original.copy(loaded=false))}.isFailure)
        assertTrue(runCatching {ConfirmedBinding.from(original.copy(deviceId=""))}.isFailure)
    }
    @Test fun queueChoiceCopiesMutableIdsAndRevisionsAndRejectsCompletedRows() {
        val row=Outgoing().apply {mediaId="row";transferId="transfer";revision=3}
        val selected=ConfirmedQueue.from(original,listOf(row));assertTrue(selected.items.single().matches(row))
        row.revision++;assertFalse(selected.items.single().matches(row));assertEquals(3L,selected.items.single().revision)
        row.revision=3;row.transferId="replacement";assertFalse(selected.items.single().matches(row))
        row.transferId="transfer";row.state="sent";assertFalse(selected.items.single().matches(row))
    }
}
