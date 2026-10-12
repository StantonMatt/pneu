package io.github.stantonmatt.pneu.features

import java.io.OutputStream

/** All managed payload writers reserve bytes before making them visible. */
internal class ReservingOutputStream(private val sink:OutputStream,private val reserve:(Long)->Unit,private val release:(Long)->Unit,private val beforeWrite:(Long)->Unit={}):OutputStream() {
    var failure:Exception?=null
        private set
    private inline fun writeReserved(length:Long,write:()->Unit) {
        try {reserve(length)}catch(e:Exception) {failure=e;throw e}
        try {beforeWrite(length);write()}catch(e:Exception) {failure=e;throw e}finally {release(length)}
    }
    override fun write(value:Int)=writeReserved(1) {sink.write(value)}
    override fun write(buffer:ByteArray,offset:Int,length:Int) {
        if(offset<0 || length<0 || offset>buffer.size-length)throw IndexOutOfBoundsException()
        writeReserved(length.toLong()) {sink.write(buffer,offset,length)}
    }
    override fun flush()=sink.flush()
    override fun close()=sink.close()
}
