package io.github.stantonmatt.pneu.features

import kotlinx.coroutines.*
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import java.io.IOException
import java.security.GeneralSecurityException

class FifthReviewTest {
    @Test fun callerDeadlineIsNotReportedAsAnActionFailure()=runTest {
        var failures=0
        val actions=UiActions {_,_->failures++}
        try {withTimeout(10) {actions.run("fixture") {awaitCancellation()}};fail()}
        catch(_:TimeoutCancellationException) {}
        assertEquals(0,failures)
    }
    @Test fun maintenanceCannotUndoCommittedAdmissionOrReceipt()=runBlocking<Unit> {
        var committed=false
        PostCommit.maintain {committed=true;throw IOException("Synthetic prune failure")}
        assertTrue(committed)
        PostCommit.maintainSuspending {throw IOException("Synthetic banner failure")}
        try {PostCommit.maintainSuspending {throw CancellationException()};fail()}
        catch(_:CancellationException) {}
        assertTrue(committed)
    }
    @Test fun uiFailuresReportActionAndPreserveStateInsteadOfNavigating()=runBlocking<Unit> {
        val failures=mutableListOf<String>();var retries=0;var retry:(()->Unit)?=null
        val actions=UiActions {message,again->failures+=message;retry=again}
        var navigated=false
        for(error in listOf(IOException(),GeneralSecurityException(),IllegalStateException())) {
            if(actions.run("Couldn't unpair. Try again.",{retries++}) {throw error})navigated=true
        }
        assertFalse(navigated);assertEquals(3,failures.size);retry!!.invoke();assertEquals(1,retries)
        assertTrue(actions.run("fixture") {});assertEquals(3,failures.size)
        try {actions.run("fixture") {throw CancellationException()};fail()}catch(_:CancellationException) {}
        assertEquals(3,failures.size)
    }
}
