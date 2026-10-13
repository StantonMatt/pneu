package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.ui.scannerPlacement
import org.junit.Assert.*
import org.junit.Test

class ScannerPlacementTest {
    @Test fun portraitRetainsTheApprovedFrameAndOffset() {
        val placement=scannerPlacement(412,850,22,1f)
        assertEquals(264,placement.frameSize);assertEquals(200,placement.top);assertEquals(28,placement.gap)
    }
    @Test fun landscapeAndShortWindowsShrinkAndRaiseTheFrame() {
        val placement=scannerPlacement(800,352,22,1f)
        assertTrue(placement.frameSize<264);assertTrue(placement.top<200)
        assertTrue(placement.top+placement.frameSize+placement.gap+22<=352)
    }
    @Test fun measuredWrappingAndInsetsBudgetAlwaysKeepInstructionVisible() {
        for(width in listOf(80,200,320,800))for(height in listOf(80,140,220,400,850))for(text in listOf(22,44,72)) {
            val placement=scannerPlacement(width,height,text,1f)
            assertTrue(placement.frameSize>=0);assertTrue(placement.top>=0);assertTrue(placement.gap>=0)
            assertTrue(placement.frameSize<=width)
            assertTrue("$width x $height, text $text",placement.top+placement.frameSize+placement.gap+text<=height)
        }
        // The same safe-area geometry at a phone's physical-pixel density.
        val scaled=scannerPlacement(960,600,132,3f)
        assertTrue(scaled.top+scaled.frameSize+scaled.gap+132<=600)
    }
}
