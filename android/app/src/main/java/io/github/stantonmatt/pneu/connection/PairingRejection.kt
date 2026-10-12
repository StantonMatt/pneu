package io.github.stantonmatt.pneu.connection

import java.net.SocketException
import kotlinx.coroutines.CancellationException

/** Only an explicit protocol rejection from the pinned PC signals revocation.
 * Fatal TLS alerts also describe cryptographic faults and never revoke trust. */
internal object PairingRejection {
    fun unauthorized(code:String,pinnedLeafVerified:Boolean)=pinnedLeafVerified && code=="unauthorized"
    /** The PC may close immediately after its post-TLS rejection, racing HELLO.
     * Recover only a complete, validated ERROR unauthorized frame on that socket. */
    fun afterWriteFailure(error:Exception,read:()->Boolean):Boolean {
        if(error !is SocketException)return false
        return try {read()}catch(e:CancellationException) {throw e}catch(_:Exception) {false}
    }
}

/** Count complete dial outcomes, not successful TLS/HELLO phases (a peer may send
 * unauthorized immediately after HELLO). A different binding starts a new streak. */
internal class RejectionStreak {
    private var binding: String? = null
    private var count = 0
    @Synchronized fun record(key: String, rejected: Boolean): Boolean {
        if(binding != key) { binding = key; count = 0 }
        count = if(rejected) count + 1 else 0
        return count >= 3
    }
    @Synchronized fun clear() { binding = null; count = 0 }
}
