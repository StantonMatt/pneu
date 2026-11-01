package io.github.stantonmatt.pneu.data

import android.app.Application
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class HistorySearchTest {
    private fun text(value:String)=SavedClip().apply {text=value}
    @Test fun accentsCaseUnicodeCaseFoldingAndWhitespaceAreNormalized() {
        assertTrue(historyMatches(text("  Café\n CON  azúcar  ")," CAFE con "))
        assertTrue(historyMatches(text("Straße"),"STRASSE"))
        assertTrue(historyMatches(text("CAFÉ"),"cafe\u0301"))
        assertTrue(historyMatches(text("a\u00a0b\t c"),"a b c"))
        assertFalse(historyMatches(text("mañana"),"yesterday"))
    }
    @Test fun urlsUseFullTextAndImagesOnlyMatchAnEmptyQuery() {
        assertTrue(historyMatches(text("https://EXAMPLE.com/mañana"),"HTTPS://example.com/manana"))
        val image=SavedClip().apply {image="photo.png";text="match"}
        assertFalse(historyMatches(image,"match"));assertTrue(historyMatches(image," \n\t "))
        assertTrue(historyMatches(text("anything"),""))
    }
}
