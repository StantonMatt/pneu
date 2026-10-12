package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.*
import org.junit.Assert.*
import org.junit.Test
import java.io.*

class EighthReviewTest {
    @Test fun reservationIsHeldThroughWriteAndReleasedOnEveryFailure() {
        var reserved=0L;var used=0L
        val sink=object:OutputStream() {
            override fun write(b:Int) {assertEquals(1L,reserved);used++;throw IOException("Broken disk")}
            override fun write(b:ByteArray,o:Int,n:Int) {assertEquals(n.toLong(),reserved);used+=n}
        }
        val output=ReservingOutputStream(sink,{reserved+=it},{reserved-=it})
        output.write(byteArrayOf(1,2,3));assertEquals(3L,used);assertEquals(0L,reserved)
        try {output.write(1);fail()}catch(_:IOException) {}
        assertEquals(0L,reserved);assertNotNull(output.failure)
        val rejected=ReservingOutputStream(sink,{throw IOException("Quota")},{fail("Unreserved release")})
        try {rejected.write(1);fail()}catch(_:IOException) {}
        assertNotNull(rejected.failure);assertEquals(4L,used)
        val hookFailure=ReservingOutputStream(sink,{reserved+=it},{reserved-=it},{throw IOException("Hook")})
        try {hookFailure.write(1);fail()}catch(_:IOException) {}
        assertEquals(0L,reserved);assertEquals(4L,used)
    }
    @Test fun twoWritersShareReservationsBeforeBytesBecomeVisible() {
        var reserved=0L;var used=0L;var competingRejected=false
        fun reserve(n:Long) {if(used+reserved+n>8)throw IOException("Quota");reserved+=n}
        val sink=object:OutputStream() {override fun write(b:Int) {used++};override fun write(b:ByteArray,o:Int,n:Int) {used+=n}}
        val competitor=ReservingOutputStream(sink,::reserve,{reserved-=it})
        val first=ReservingOutputStream(sink,::reserve,{reserved-=it}) {
            try {competitor.write(ByteArray(5));fail()}catch(_:IOException) {competingRejected=true}
        }
        first.write(ByteArray(4));assertTrue(competingRejected);assertEquals(4L,used);assertEquals(0L,reserved)
    }
    @Test fun admissionUsesEpochAndLiveCatalogInsteadOfRoutineCheckpointChanges() {
        val initial=Prefs(imageVersion="catalog",imageGeneration=10,imageFloor=5,policyEpoch=1)
        val token=AdmissionToken.from(initial,false)
        assertTrue(AdmissionPolicy.accepts(token,initial,"catalog"))
        assertFalse(AdmissionPolicy.accepts(token,initial.copy(paused=true),"catalog"))
        assertFalse(AdmissionPolicy.accepts(token,initial.copy(policyEpoch=2),"catalog"))
        assertFalse(AdmissionPolicy.accepts(token,initial,"rebuilt"))
        assertTrue(AdmissionPolicy.accepts(token,initial.copy(imageGeneration=11),"catalog"))
        assertTrue(AdmissionPolicy.accepts(token,initial.copy(videoGeneration=99),"catalog"))
    }
}
