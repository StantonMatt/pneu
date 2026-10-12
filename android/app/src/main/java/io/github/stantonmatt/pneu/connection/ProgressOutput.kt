package io.github.stantonmatt.pneu.connection

import java.io.OutputStream

/** A frame remains locked, while successful TLS-sized writes refresh its progress deadline. */
class ProgressOutput(private val output:OutputStream,private val progress:()->Unit):OutputStream() {
    override fun write(b:ByteArray,off:Int,len:Int) {
        var n=0
        while(n<len) {val count=minOf(16384,len-n);output.write(b,off+n,count);n+=count;progress()}
    }
    override fun write(value:Int) {output.write(value);progress()}
    override fun flush()=output.flush()
}
