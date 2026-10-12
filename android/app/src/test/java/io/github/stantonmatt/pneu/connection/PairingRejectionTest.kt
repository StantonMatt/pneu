package io.github.stantonmatt.pneu.connection

import javax.net.ssl.*
import java.io.*
import org.junit.Assert.*
import org.junit.Test

class PairingRejectionTest {
    @Test fun failedWriteRecoversOnlyExplicitUnauthorizedAndPreservesCancellation() {
        val broken=java.net.SocketException("Broken pipe")
        assertTrue(PairingRejection.afterWriteFailure(broken) {true})
        assertFalse(PairingRejection.afterWriteFailure(broken) {false})
        for(error in listOf(EOFException(),java.net.SocketTimeoutException(),SSLException("Received fatal alert: handshake_failure")))
            assertFalse(PairingRejection.afterWriteFailure(broken) {throw error})
        assertFalse(PairingRejection.afterWriteFailure(IOException()) {fail("No read for local errors");true})
        val cancelled=kotlinx.coroutines.CancellationException()
        try {PairingRejection.afterWriteFailure(broken) {throw cancelled};fail()}
        catch(e:kotlinx.coroutines.CancellationException) {assertSame(cancelled,e)}
    }
    @Test fun unauthorizedRequiresAnAuthenticatedPinnedPeer() {
        assertTrue(PairingRejection.unauthorized("unauthorized",true))
        assertFalse(PairingRejection.unauthorized("unauthorized",false))
        for(code in listOf("closing","timeout","version_mismatch","unexpected_message","identity_mismatch"))
            assertFalse(PairingRejection.unauthorized(code,true))
    }
    @Test fun threeConsecutiveRejectionsRequiredAndOtherOutcomesOrBindingResetThem() {
        val streak=RejectionStreak()
        assertFalse(streak.record("pc-a",true));assertFalse(streak.record("pc-a",true))
        assertFalse(streak.record("pc-a",false));assertFalse(streak.record("pc-a",true))
        assertFalse(streak.record("pc-a",true));assertTrue(streak.record("pc-a",true))
        assertFalse(streak.record("pc-b",true));assertFalse(streak.record("pc-b",true))
        streak.clear();assertFalse(streak.record("pc-b",true))
    }
}
