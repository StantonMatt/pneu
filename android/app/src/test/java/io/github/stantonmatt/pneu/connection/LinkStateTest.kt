package io.github.stantonmatt.pneu.connection

import org.junit.Assert.*
import org.junit.Test

class LinkStateTest {
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
