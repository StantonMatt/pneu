package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.Outgoing

/** All deadlines supplied here are monotonic. Preserve ready-entry order while
 * delayed rows yield to ready siblings. */
object QueueSchedule {
    data class Choice(val row:Outgoing?,val waitMillis:Long)
    fun choose(rows:List<Outgoing>,now:Long,retryDeadline:(Outgoing)->Long,deferred:Map<String,Long> = emptyMap()):Choice {
        val eligible=rows.filter {it.state in setOf("waiting","failed") && it.nextAttempt!=Long.MAX_VALUE}
        fun deadline(row:Outgoing)=maxOf(retryDeadline(row),deferred[row.mediaId] ?: 0)
        eligible.firstOrNull {deadline(it)<=now}?.let {return Choice(it,0)}
        return Choice(null,eligible.minOfOrNull {deadline(it)-now} ?: Long.MAX_VALUE)
    }
}
