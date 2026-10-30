package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.pairingScanBlocked
import org.junit.Assert.*
import org.junit.Test

class ScannerStateTest {
    @Test fun expiryDialogBlocksScanningUntilAcknowledged() {
        // Expiry can be reported before a pairing attempt is created.
        assertFalse(pairingScanBlocked(attemptPending=false,installed=false,expired=false))
        assertTrue(pairingScanBlocked(attemptPending=false,installed=false,expired=true))
        // OK clears the dialog state, allowing the next QR callback again.
        assertFalse(pairingScanBlocked(attemptPending=false,installed=false,expired=false))
    }
}
