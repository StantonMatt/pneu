package io.github.stantonmatt.pneu.protocol
import io.github.stantonmatt.pneu.connection.RetryPolicy
import org.junit.Assert.*
import org.junit.Test
class RetryPolicyTest {
    @Test fun mismatchesStopAfterThreeRetriesAndDoNotResetOnStorageFailure() {
        var count=0
        repeat(4) { index ->
            val result=RetryPolicy.nack("hash_mismatch",count,1.0)
            count=result.mismatches
            assertEquals(index+1,count)
            assertEquals(if(index==3)Long.MAX_VALUE else 2000L,result.waitMillis)
        }
        assertEquals(4,RetryPolicy.nack("disk_full",count,1.0).mismatches)
    }
    @Test fun storageErrorsWaitAndPermanentErrorsRemainBlocked() {
        for(code in listOf("storage_failed","disk_full","not_writable","clipboard_unavailable")) {
            assertEquals(48000,RetryPolicy.nack(code,0,.8).waitMillis)
            assertEquals(72000,RetryPolicy.nack(code,0,1.2).waitMillis)
        }
        for(code in listOf("unsupported_kind","id_conflict")) assertEquals(Long.MAX_VALUE,RetryPolicy.nack(code,0,1.0).waitMillis)
    }
}
