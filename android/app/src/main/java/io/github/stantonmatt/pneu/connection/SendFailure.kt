package io.github.stantonmatt.pneu.connection

import kotlinx.coroutines.CancellationException
import java.io.IOException

/** Only wire I/O and a missing peer receipt invalidate the transport. Local work
 * is explicitly bounded to a row, including provider, private file and database failures. */
internal class TransportFailure(cause:Throwable):IOException("Transport failed",cause)
internal class LocalSendFailure(val code:String,cause:Throwable):IOException("Local delivery failed",cause)
internal object SendFailures {
    fun code(error:Exception)=when(error) {
        is LocalSendFailure->error.code
        is io.github.stantonmatt.pneu.features.LocalCapacityException->"local_capacity"
        else->"local_storage"
    }
    fun retryAfter(code:String)=if(code=="invalid_payload")Long.MAX_VALUE else 60000L
    suspend fun <T> local(code:String,body:suspend ()->T):T=try {body()}
        catch(e:CancellationException) {throw e}
        catch(e:Exception) {throw LocalSendFailure(code,e)}
}
