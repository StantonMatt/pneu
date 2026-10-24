package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import org.junit.Assert.*
import org.junit.Test

class ClipboardHistoryTest {
    @Test fun historySurvivesReloadRefreshesOwnershipAndEvictsHashAndSourceTogether() {
        val data=mutablePreferencesOf()
        ClipboardHistory.remember(data,"X","A");ClipboardHistory.remember(data,"Y","B")
        assertEquals(mapOf("X" to "A","Y" to "B"),ClipboardHistory.read(data.toPreferences()))
        ClipboardHistory.remember(data,"X","C")
        repeat(63) {ClipboardHistory.remember(data,"new-$it","D")}
        val reloaded=data.toPreferences()
        assertEquals(64,ClipboardHistory.read(reloaded).size)
        assertEquals("C",ClipboardHistory.read(reloaded)["X"])
        assertFalse("Y" in ClipboardHistory.read(reloaded))
        ClipboardHistory.remember(data,"last","E")
        assertFalse("X" in ClipboardHistory.read(data))
        assertFalse(data[stringPreferencesKey("recentClipboardHashes")]!!.split('|').contains("X"))
    }
    @Test fun legacyLatestOwnershipIsPreservedWithoutInventingSourcesForOlderHashes() {
        val data=mutablePreferencesOf(stringPreferencesKey("recentClipboardHashes") to "X|Y",
            stringPreferencesKey("lastReceivedHash") to "Y",stringPreferencesKey("lastReceivedSource") to "B")
        assertEquals(mapOf("X" to "","Y" to "B"),ClipboardHistory.read(data))
        ClipboardHistory.remember(data,"Z","C")
        assertEquals(mapOf("X" to "","Y" to "B","Z" to "C"),ClipboardHistory.read(data.toPreferences()))
    }
}
