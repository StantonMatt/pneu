package io.github.stantonmatt.pneu.features

import java.io.*
import org.junit.Assert.*
import org.junit.Test

class StreamIntegrityTest {
    @Test fun declaredRangesRejectTruncationAndRespectTheirBoundary() {
        assertThrows(EOFException::class.java) {copyExact(ByteArrayInputStream(byteArrayOf(1,2)),ByteArrayOutputStream(),3,2)}
        val input=ByteArrayInputStream(byteArrayOf(1,2,3,4));val out=ByteArrayOutputStream()
        copyExact(input,out,3,2);assertArrayEquals(byteArrayOf(1,2,3),out.toByteArray());assertEquals(4,input.read())
        val zero=ByteArrayOutputStream();copyExact(input,zero,0,2);assertEquals(0,zero.size())
    }
    @Test fun unknownLengthCompletesAtEofAndCopyErrorsPropagate() {
        val out=ByteArrayOutputStream();copyExact(ByteArrayInputStream(byteArrayOf(1,2,3)),out,-1,2)
        assertArrayEquals(byteArrayOf(1,2,3),out.toByteArray())
        assertThrows(IOException::class.java) {copyExact(object:InputStream(){override fun read():Int=throw IOException("Producer failure")},ByteArrayOutputStream(),-1,2)}
    }
}
