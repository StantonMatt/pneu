package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.timing.PersistedDeadlines

/** Preserve the daily alert cadence through restarts while bounding future
 * saved times after rollback. Visible UI suppresses alerts without consuming one. */
internal class AlertThrottle(private val wall:()->Long,private val elapsed:()->Long) {
    private val deadlines=PersistedDeadlines<String>(DAY,wall,elapsed)
    fun eligible(key:String,lastShown:Long,visible:Boolean)=
        !visible && (lastShown==0L || deadlines.at(key,PersistedDeadlines.add(lastShown,DAY))<=elapsed())
    fun shown(key:String):Long=wall().also {deadlines.remember(key,PersistedDeadlines.add(it,DAY),DAY)}
    companion object {const val DAY=86_400_000L}
}
