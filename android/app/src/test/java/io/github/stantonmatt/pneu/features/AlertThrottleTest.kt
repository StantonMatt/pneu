package io.github.stantonmatt.pneu.features

import org.junit.Assert.*
import org.junit.Test

class AlertThrottleTest {
    @Test fun dailyThrottleIgnoresWallChangesAndForegroundDoesNotConsumeTheAlert() {
        var wall=2*AlertThrottle.DAY;var elapsed=1L;val alerts=AlertThrottle({wall},{elapsed})
        assertFalse(alerts.eligible("photos",0,true));assertTrue(alerts.eligible("photos",0,false))
        val saved=alerts.shown("photos")
        wall-=10*AlertThrottle.DAY;elapsed+=AlertThrottle.DAY-1
        assertFalse(alerts.eligible("photos",saved,false))
        elapsed++;assertTrue(alerts.eligible("photos",saved,false));assertFalse(alerts.eligible("photos",saved,true))
        // Another cause has its own cadence.
        assertTrue(alerts.eligible("battery",0,false))
    }
    @Test fun savedFutureAlertsWaitAtMostOneDayAndRepeatedChecksCannotExtendIt() {
        var wall=1000L;var elapsed=1L;val alerts=AlertThrottle({wall},{elapsed});val future=100*AlertThrottle.DAY
        assertFalse(alerts.eligible("pairing",future,false))
        repeat(23) {wall-=AlertThrottle.DAY;elapsed+=3_600_000;assertFalse(alerts.eligible("pairing",future,false))}
        elapsed+=3_600_000;assertTrue(alerts.eligible("pairing",future,false))
        val freshProcess=AlertThrottle({wall},{1L})
        assertFalse(freshProcess.eligible("pairing",future,false))
        assertTrue(AlertThrottle({2*AlertThrottle.DAY},{1L}).eligible("old",1,false))
    }
}
