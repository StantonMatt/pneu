package io.github.stantonmatt.pneu.connection

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.coroutines.selects.onTimeout
import kotlinx.coroutines.selects.select

/** Every lane wait can be interrupted by admission/policy work. Coroutine
 * timeout durations are monotonic; no wall deadline is re-read while waiting. */
internal object LaneWait {
    @OptIn(ExperimentalCoroutinesApi::class)
    suspend fun await(work:ReceiveChannel<Unit>,waitMillis:Long):Boolean {
        if(waitMillis<=0)return false
        return select {
            work.onReceive {true}
            if(waitMillis!=Long.MAX_VALUE)onTimeout(waitMillis) {false}
        }
    }
}
