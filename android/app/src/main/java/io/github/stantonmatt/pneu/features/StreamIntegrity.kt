package io.github.stantonmatt.pneu.features

import java.io.EOFException
import java.io.InputStream
import java.io.OutputStream

/** A declared asset range must be consumed completely; unknown-length streams
 * finish only at EOF. The caller must also check reliable-pipe producer errors. */
internal class StreamRange(length: Long) {
    var remaining = length
        private set
    init { require(length >= -1) }
    fun limit(bufferSize: Int) = if(remaining < 0) bufferSize else minOf(remaining,bufferSize.toLong()).toInt()
    fun consume(bytes: Int): Boolean {
        require(bytes >= 0)
        if(bytes == 0) {
            if(remaining > 0)throw EOFException("Incomplete declared range")
            return false
        }
        if(remaining >= 0) {require(bytes.toLong() <= remaining);remaining -= bytes}
        return true
    }
}

internal fun copyExact(input: InputStream, output: OutputStream, length: Long, bufferSize: Int) {
    val range=StreamRange(length);val buffer=ByteArray(bufferSize)
    while(range.remaining != 0L) {
        val n=input.read(buffer,0,range.limit(buffer.size))
        if(n==0)throw java.io.IOException("Stream made no progress")
        if(!range.consume(if(n<0)0 else n))break
        output.write(buffer,0,n)
    }
}
