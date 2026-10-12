package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.timing.PersistedDeadlines

/** Persisted retries survive restart; live retries never follow wall-clock jumps.
 * Long.MAX_VALUE is a permanent/manual-retry marker, never a timed wait. */
internal class RetryDeadlines(private val wall:()->Long,private val elapsed:()->Long) {
    private data class Key(val mediaId:String,val transferId:String,val revision:Long)
    private fun key(row:Outgoing)=Key(row.mediaId,row.transferId,row.revision)
    private val deadlines=PersistedDeadlines<Key>(RetryPolicy.MAX_BACKOFF_MILLIS,wall,elapsed)
    fun deadline(row:Outgoing):Long=when(row.nextAttempt) {
        Long.MAX_VALUE -> Long.MAX_VALUE
        0L -> 0L
        else -> deadlines.at(key(row),row.nextAttempt)
    }
    fun retryAfter(row:Outgoing,wait:Long) {
        require(wait==Long.MAX_VALUE || wait in 0..RetryPolicy.MAX_BACKOFF_MILLIS)
        row.nextAttempt=if(wait==Long.MAX_VALUE)Long.MAX_VALUE else PersistedDeadlines.add(wall(),wait)
        if(wait!=Long.MAX_VALUE)deadlines.remember(key(row),row.nextAttempt,wait)
    }
    fun read(rows:List<Outgoing>) {
        deadlines.retain(rows.map(::key).toSet())
        rows.forEach {deadline(it)} // start every saved retry at first read, even behind ready/held work
    }
}
