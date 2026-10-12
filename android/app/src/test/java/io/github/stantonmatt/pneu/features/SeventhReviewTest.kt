package io.github.stantonmatt.pneu.features

import org.junit.Assert.*
import org.junit.Test

class SeventhReviewTest {
    @Test fun copyDeadlineIsInactivityNotTotalDurationAndReadsClockBeforeSnapshot() {
        var now=0L;var duringClock:(()->Unit)?=null
        val deadline=CopyProgress(10) {duringClock?.let {duringClock=null;it()};now}
        repeat(100) {now+=9_000_000;assertFalse(deadline.expired());deadline.advance()}
        now+=10_000_000
        duringClock={deadline.advance()}
        assertFalse("Progress during clock read must not expire a stale timestamp",deadline.expired())
        now+=9_999_999;assertFalse(deadline.expired())
        now++;assertTrue(deadline.expired())
    }
}
