package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.connection.ProgressOutput
import io.github.stantonmatt.pneu.connection.Connectivity.*
import io.github.stantonmatt.pneu.connection.QueueSchedule
import io.github.stantonmatt.pneu.connection.RetryPolicy
import io.github.stantonmatt.pneu.data.Outgoing
import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayOutputStream

class AdvisoryPolicyTest {
    @Test fun unicodeNamesAndJpegFilenamesFitWithoutSplittingCodePoints() {
        val text="界".repeat(64)+"😀"
        assertEquals(126,utf8Prefix(text,128).toByteArray().size)
        assertEquals("a".repeat(199)+"😀.png",safeFilename("a".repeat(199)+"😀.png"))
        val jpeg=safeFilename("界".repeat(84),true)
        assertTrue(jpeg.toByteArray().size<=255)
        Protocol.validate(Type.FILE_BEGIN,json("transfer_id" to "a".repeat(32),"file_name" to jpeg,"size" to 0,"mime" to "image/jpeg","sha256" to sha256(byteArrayOf()),"created_at" to 0,"kind" to "photo"))
        assertEquals("ab",utf8Prefix("a\uD800b",128))
    }
    @Test fun mappedIpv6IsALiteralWithoutDns() {
        assertEquals("::ffff:192.168.1.10",Endpoint.parse("[::ffff:192.168.1.10]:47821").host)
        assertTrue(runCatching {Endpoint.parse("[::ffff:999.1.1.1]:1")}.isFailure)
        assertTrue(runCatching {Endpoint.parse("[hostname]:1")}.isFailure)
    }
    @Test fun everyTransportSwitchIsCheckedAgainstCurrentPolicy() {
        assertFalse(DeliveryPolicy.transport(false,false,true,UNMETERED,true))
        assertFalse(DeliveryPolicy.transport(true,true,false,UNMETERED,true))
        assertFalse(DeliveryPolicy.transport(true,true,true,METERED,false))
        assertTrue(DeliveryPolicy.transport(false,true,false,UNMETERED,false))
        assertFalse(DeliveryPolicy.transport(false,true,true,NONE,true))
    }
    @Test fun exifTransposeAndTransverseHaveDistinctCornerMappings() {
        fun map(o:Int,x:Float,y:Float):Pair<Float,Float> {val m=ImagePolicy.matrix(o);return (m[0]*x+m[1]*y) to (m[3]*x+m[4]*y)}
        assertEquals(2f to 1f,map(5,1f,2f))
        assertEquals(-2f to -1f,map(7,1f,2f))
        assertEquals(-2f to 1f,map(6,1f,2f))
        for(o in 1..8) {val m=ImagePolicy.matrix(o);assertEquals(1f,kotlin.math.abs(m[0]*m[4]-m[1]*m[3]),0f)}
    }
    @Test fun progressRefreshesForEverySuccessfulSegment() {
        val bytes=ByteArray(256*1024) {it.toByte()};val output=ByteArrayOutputStream();var calls=0
        ProgressOutput(output) {calls++}.write(bytes)
        assertArrayEquals(bytes,output.toByteArray());assertEquals(16,calls)
    }
    @Test fun permissionCopyMatchesRepairAndLimitedGrantsRemainRequestable() {
        assertTrue(PermissionPolicy.batterySettings(true));assertFalse(PermissionPolicy.batterySettings(false))
        assertFalse(PermissionPolicy.sleeping(true,true,true,false));assertTrue(PermissionPolicy.sleeping(true,true,false,false))
        assertEquals("Allow all",PermissionPolicy.photoAction(true,true,false))
        assertEquals("Allow",PermissionPolicy.photoAction(false,true,true))
        assertEquals("Settings",PermissionPolicy.photoAction(false,true,false))
        assertEquals("Allow camera",PermissionPolicy.cameraAction(true,true))
        assertEquals("Open settings",PermissionPolicy.cameraAction(true,false))
    }
    @Test fun systemExitEvidenceExcludesDeliberateStops() {
        assertTrue(PermissionPolicy.systemExit(2));assertTrue(PermissionPolicy.systemExit(12))
        assertTrue(PermissionPolicy.systemExit(9));assertFalse(PermissionPolicy.systemExit(10))
    }
    @Test fun receiptTimeoutNeverClaimsNonDelivery() {
        for(state in listOf("waiting","sending","failed"))assertEquals("Still sending clipboard",ClipboardFeedback.result(Outgoing().apply {this.state=state}))
        assertEquals("Clipboard sent",ClipboardFeedback.result(Outgoing().apply {state="sent";nextAttempt=Long.MAX_VALUE}))
        assertEquals("Replaced by newer clipboard",ClipboardFeedback.result(Outgoing().apply {state="superseded";nextAttempt=Long.MAX_VALUE}))
        for(state in listOf("discarded","skipped"))assertEquals("Clipboard send cancelled",ClipboardFeedback.result(Outgoing().apply {this.state=state}))
        assertEquals("Clipboard send cancelled",ClipboardFeedback.result(null))
    }
    @Test fun blockedClipboardFeedbackMatchesTheSendersRetryMarker() {
        for(code in listOf("id_conflict","unsupported_kind","hash_mismatch","size_mismatch")) {
            val row=Outgoing().apply {state="failed";error=code;nextAttempt=RetryPolicy.nack(code,3,1.0).waitMillis}
            assertEquals(Long.MAX_VALUE,row.nextAttempt)
            assertNull(QueueSchedule.choose(listOf(row),0,{it.nextAttempt}).row)
            assertEquals("Couldn't send clipboard",ClipboardFeedback.result(row))
            row.nextAttempt=0 // Explicit Retry or renewed capability eligibility.
            assertSame(row,QueueSchedule.choose(listOf(row),0,{it.nextAttempt}).row)
            assertEquals("Still sending clipboard",ClipboardFeedback.result(row))
        }
        // The deadline, not an error-code allowlist, is authoritative.
        assertEquals("Couldn't send clipboard",ClipboardFeedback.result(Outgoing().apply {state="waiting";nextAttempt=Long.MAX_VALUE}))
        assertEquals("Couldn't send clipboard",ClipboardFeedback.result(Outgoing().apply {state="failed";error="invalid_payload";nextAttempt=Long.MAX_VALUE}))
    }
    @Test fun retryableClipboardFailuresKeepPendingFeedbackDuringCooldown() {
        for(code in listOf("storage_failed","disk_full","not_writable","clipboard_unavailable","hash_mismatch")) {
            val row=Outgoing().apply {state="failed";error=code;nextAttempt=RetryPolicy.nack(code,0,1.0).waitMillis}
            assertTrue(row.nextAttempt in 1 until Long.MAX_VALUE)
            assertEquals("Still sending clipboard",ClipboardFeedback.result(row))
        }
    }
}
