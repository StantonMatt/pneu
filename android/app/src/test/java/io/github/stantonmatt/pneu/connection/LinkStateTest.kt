package io.github.stantonmatt.pneu.connection

import org.junit.Assert.*
import org.junit.Test
import kotlinx.coroutines.flow.MutableStateFlow
import io.github.stantonmatt.pneu.features.UiFeedback

class LinkStateTest {
    @Test fun repeatedFeedbackIsPublishedWithoutReplayingItForProgressUpdates() {
        val state=MutableStateFlow(LinkState())
        state.value=state.value.withPairing("This code expired. Scan the new one.")
        val first=state.value
        state.value=state.value.withPairing(first.pairing)
        val second=state.value
        assertEquals(first.pairing,second.pairing)
        assertNotEquals(first.pairingFeedback?.id,second.pairingFeedback?.id)
        assertNotEquals(first,second)
        assertEquals(second.pairingFeedback,second.copy(sent=10,total=100).pairingFeedback)
        assertNotEquals(second.pairingFeedback,LinkState(pairing=second.pairing).pairingFeedback)
        val cleared=second.withPairing("")
        assertEquals("",cleared.pairing)
        assertNull(cleared.pairingFeedback)
        assertNotEquals(UiFeedback("Same error"),UiFeedback("Same error"))
    }
    @Test fun activeMetadataIsIndependentOfQueueProjections() {
        assertNull(LinkState().sendingTitle())
        val active=LinkState(transferMediaId="active",transferKind="file",transferFileName="report.pdf")
        assertEquals("Sending report.pdf",active.sendingTitle())
        assertEquals("Sending file",active.copy(transferFileName="").sendingTitle())
        assertEquals("Sending photo",active.copy(transferKind="photo").sendingTitle())
        assertEquals("Sending screen recording",active.copy(transferKind="recording").sendingTitle())
        assertEquals("Sending clipboard image",active.copy(transferKind="clipboard_image").sendingTitle())
        assertEquals("Sending screenshot",active.copy(transferKind="screenshot").sendingTitle())
        assertNull(active.copy(transferMediaId="").sendingTitle())
    }
}
