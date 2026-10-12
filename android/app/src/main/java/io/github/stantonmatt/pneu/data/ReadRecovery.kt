package io.github.stantonmatt.pneu.data

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.retryWhen

/** Resubscribe without emitting guessed defaults or losing the last
 * authoritative StateFlow value. Cancellation and fatal errors propagate. */
internal fun <T> Flow<T>.recoverReads():Flow<T> = retryWhen {cause,_->
    if(cause is CancellationException || cause !is Exception)throw cause
    runCatching {android.util.Log.w("PneuReadRecovery",cause.javaClass.simpleName)}
    delay(2000);true
}
