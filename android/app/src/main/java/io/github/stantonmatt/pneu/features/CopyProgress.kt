package io.github.stantonmatt.pneu.features

import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.TimeUnit

/** One snapshot per inactivity decision; total copy time is not a deadline. */
internal class CopyProgress(timeoutMillis:Long,private val clock:()->Long=System::nanoTime) {
    private val timeout=TimeUnit.MILLISECONDS.toNanos(timeoutMillis)
    private val last=AtomicLong(clock())
    init {require(timeoutMillis>0)}
    fun advance() {last.set(clock())}
    fun expired():Boolean {val now=clock();val snapshot=last.get();return now-snapshot>=timeout}
}
