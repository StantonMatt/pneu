package io.github.stantonmatt.pneu.connection

import java.util.concurrent.atomic.AtomicReference

/** Each progress/completion publishes a new identity, even within the same clock tick.
 * Expiry must claim the exact snapshot: a completed or progressing write wins over
 * a monitor that computed a deadline from stale state. Clock is monotonic. */
internal class ProgressDeadline(private val timeout: Long, private val clock: () -> Long) {
    class Snapshot(val at: Long)
    private val current = AtomicReference<Snapshot?>(null)
    fun progress() { current.set(Snapshot(clock())) }
    fun complete() { current.set(null) }
    fun snapshot(): Snapshot? = current.get()
    fun remaining(snapshot: Snapshot): Long = timeout - (clock() - snapshot.at)
    fun expire(snapshot: Snapshot): Boolean = remaining(snapshot) <= 0 && current.compareAndSet(snapshot, null)
}

internal class PingDeadline(private val timeout: Long, private val clock: () -> Long) {
    class Pending(val nonce: String, val at: Long)
    private val current = AtomicReference<Pending?>(null)
    fun start(nonce: String): Pending {
        val pending=Pending(nonce,clock())
        check(current.compareAndSet(null,pending));return pending
    }
    // Register the nonce before sending so an immediate PONG can clear it, but
    // budget the response timeout from the completed write, not mutex acquisition.
    @Synchronized fun sent(pending: Pending) { current.compareAndSet(pending,Pending(pending.nonce,clock())) }
    fun snapshot(): Pending? = current.get()
    fun remaining(pending: Pending): Long = timeout - (clock() - pending.at)
    // sent() must not replace the matching snapshot midway through PONG completion.
    @Synchronized fun pong(nonce: String): Boolean {
        val pending = current.get() ?: return false
        return pending.nonce == nonce && current.compareAndSet(pending, null)
    }
    fun expire(pending: Pending): Boolean = remaining(pending) <= 0 && current.compareAndSet(pending, null)
}
