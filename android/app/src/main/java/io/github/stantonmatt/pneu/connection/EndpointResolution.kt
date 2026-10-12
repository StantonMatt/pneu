package io.github.stantonmatt.pneu.connection

import java.io.InterruptedIOException
import java.net.InetAddress
import java.net.SocketException
import java.net.SocketTimeoutException
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/** One monotonic budget, shared by DNS and TCP instead of resetting after resolution. */
internal class DialBudget(milliseconds:Long,private val clock:()->Long=System::nanoTime) {
    private val deadline=clock()+TimeUnit.MILLISECONDS.toNanos(milliseconds)
    fun remainingMillis():Int {
        val remaining=deadline-clock()
        if(remaining<=0)throw SocketTimeoutException("Endpoint deadline")
        return TimeUnit.NANOSECONDS.toMillis(remaining).coerceAtLeast(1).coerceAtMost(Int.MAX_VALUE.toLong()).toInt()
    }
}

/** Callback DNS avoids an uninterruptible getByName worker for hostnames. Cancellation
 * also follows the registered socket, so policy changes interrupt resolution immediately. */
internal object EndpointResolution {
    private class Answer(val value:Result<List<InetAddress>>)
    fun await(budget:DialBudget,cancelled:()->Boolean,start:((Result<List<InetAddress>>)->Unit)->()->Unit):List<InetAddress> {
        val answer=AtomicReference<Answer?>()
        val done=CountDownLatch(1)
        val cancel=start {result->answer.compareAndSet(null,Answer(result));done.countDown()}
        try {
            while(true) {
                if(cancelled())throw SocketException("Dial cancelled")
                val remaining=budget.remainingMillis()
                if(done.await(minOf(remaining,50).toLong(),TimeUnit.MILLISECONDS)) {
                    if(cancelled())throw SocketException("Dial cancelled")
                    budget.remainingMillis()
                    return requireNotNull(answer.get()).value.getOrThrow().also {if(it.isEmpty())throw java.net.UnknownHostException()}
                }
            }
        } catch(e:InterruptedException) {
            Thread.currentThread().interrupt();throw InterruptedIOException("DNS interrupted")
        } finally {cancel()}
    }
}

/** TCP failures consume one shared DNS/connect budget. Never reuse a failed socket. */
internal object EndpointDialer {
    fun connect(addresses:List<InetAddress>,port:Int,budget:DialBudget,register:(java.net.Socket)->Unit,
                create:()->java.net.Socket={java.net.Socket()}):java.net.Socket =
        authenticate(addresses,port,budget,register,create=create) {it}
    /** DNS addresses are alternatives through authentication, not just TCP.
     * All stages consume the endpoint budget; each fresh TCP dial is capped. */
    fun <T> authenticate(addresses:List<InetAddress>,port:Int,budget:DialBudget,register:(java.net.Socket)->Unit,
                         maxConnect:Int=Int.MAX_VALUE,create:()->java.net.Socket={java.net.Socket()},
                         authenticate:(java.net.Socket)->T):T {
        var last:Exception?=null
        for(address in addresses) {
            val remaining=budget.remainingMillis()
            val socket=create();register(socket)
            if(socket.isClosed)throw SocketException("Dial cancelled")
            try {
                socket.tcpNoDelay=true
                socket.connect(java.net.InetSocketAddress(address,port),minOf(remaining,maxConnect))
            } catch(e:java.io.IOException) {
                val cancelled=socket.isClosed;socket.close()
                if(cancelled)throw e
                last=e;continue
            }
            try {return authenticate(socket)}
            catch(e:kotlinx.coroutines.CancellationException) {socket.close();throw e}
            catch(e:Exception) {socket.close();if(Thread.currentThread().isInterrupted)throw e;last=e}
        }
        throw last ?: java.net.UnknownHostException()
    }
}
