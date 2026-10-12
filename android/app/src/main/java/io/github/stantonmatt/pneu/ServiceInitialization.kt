package io.github.stantonmatt.pneu

import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel

/** One startup attempt at a time; service commands wake failed initialization. */
internal class ServiceInitialization(scope:CoroutineScope,attempt:suspend ()->Unit,failure:(Exception)->Unit) {
    private val kicks=Channel<Unit>(Channel.CONFLATED)
    private val job=scope.launch {
        var backoff=1000L
        while(isActive) {
            while(kicks.tryReceive().isSuccess) { /* Coalesce commands for this attempt. */ }
            try {attempt();return@launch}
            catch(e:CancellationException) {throw e}
            catch(e:Exception) {runCatching {failure(e)}}
            withTimeoutOrNull(backoff) {kicks.receive()}
            backoff=(backoff*2).coerceAtMost(30000)
        }
    }
    fun kick() {if(job.isActive)kicks.trySend(Unit)}
}
