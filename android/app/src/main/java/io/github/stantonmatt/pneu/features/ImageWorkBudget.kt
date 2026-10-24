package io.github.stantonmatt.pneu.features

/** Shared by all peers, optimization and previews. The permit belongs to the
 * actual decoder worker, so a timed-out caller cannot release it prematurely. */
internal object ImageWorkBudget {
    private val slots=java.util.concurrent.Semaphore(2,true)
    inline fun <T> run(body:()->T):T {
        acquire()
        try {return body()}finally {release()}
    }
    @PublishedApi internal fun acquire()=slots.acquire()
    @PublishedApi internal fun release()=slots.release()
}
