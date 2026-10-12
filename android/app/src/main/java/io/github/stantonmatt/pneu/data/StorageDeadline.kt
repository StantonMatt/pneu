package io.github.stantonmatt.pneu.data

import java.io.IOException
import kotlinx.coroutines.withTimeoutOrNull

internal class StorageTimeoutException : IOException("Storage deadline expired")

/** A local deadline is a retryable I/O failure, not cancellation of the caller.
 * Boxing also lets a successful nullable result remain distinct from timeout. */
internal suspend fun <T> storageDeadline(timeoutMillis:Long=5000,body:suspend ()->T):T {
    class Result(val value:T)
    val result=withTimeoutOrNull(timeoutMillis) {Result(body())} ?: throw StorageTimeoutException()
    return result.value
}
