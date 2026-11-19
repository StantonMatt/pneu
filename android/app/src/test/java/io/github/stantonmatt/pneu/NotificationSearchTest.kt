package io.github.stantonmatt.pneu

import android.app.Application
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Surface
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import io.github.stantonmatt.pneu.data.Prefs
import io.github.stantonmatt.pneu.features.NotificationConsent
import io.github.stantonmatt.pneu.ui.*
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,qualifiers="w411dp-h923dp")
class NotificationSearchTest {
    @get:Rule val compose=createComposeRule()
    private val whatsapp=NotificationPickerApp("0/com.whatsapp","WhatsApp")
    private val cafe=NotificationPickerApp("0/org.example.cafe","Café Straße")
    private val calendar=NotificationPickerApp("0/com.android.calendar","Calendar")
    private val apps=listOf(calendar,cafe,whatsapp)

    private fun screen(blocked:Boolean=false) {
        compose.setContent {
            var prefs by remember {mutableStateOf(Prefs(notificationConsent=NotificationConsent(enabled=true,apps=setOf(calendar.key))))}
            PneuTheme {Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {
                NotificationsContent(prefs,!blocked,blocked,apps,{},{},{},{_,key,enabled->
                    val consent=prefs.notificationConsent
                    prefs=prefs.copy(notificationConsent=consent.copy(apps=if(enabled)consent.apps+key else consent.apps-key))
                },{})
            }}
        }
    }
    private fun query(value:String) {
        compose.onNodeWithContentDescription("Search").performScrollTo().performTextReplacement(value)
    }
    @Test fun labelAndPackageMatchIgnoringCaseAccentsAndUnicodeCaseFolding() {
        assertTrue(notificationAppMatches(cafe," CAFE STRASSE "))
        assertTrue(notificationAppMatches(cafe,"cafe\u0301"))
        assertTrue(notificationAppMatches(whatsapp,"COM.WHATSAPP"))
        assertFalse(notificationAppMatches(calendar,"wha"))
        screen();query("CAFE STRASSE")
        compose.onNodeWithText(cafe.label).assertIsDisplayed()
        compose.onNodeWithText(calendar.label).assertDoesNotExist()
        compose.onNodeWithText(whatsapp.label).assertDoesNotExist()
        query("COM.WHATSAPP")
        compose.onNodeWithText(whatsapp.label).assertIsDisplayed()
        compose.onNodeWithText(cafe.label).assertDoesNotExist()
    }
    @Test fun noMatchesAndClearingRestoreAllAppsAndSelection() {
        screen();query("missing")
        compose.onNodeWithText("No matches").assertIsDisplayed()
        apps.forEach {compose.onNodeWithText(it.label).assertDoesNotExist()}
        compose.onNodeWithContentDescription("Clear search").performClick()
        compose.onNodeWithText("No matches").assertDoesNotExist()
        apps.forEach {compose.onNodeWithText(it.label).assertIsDisplayed()}
        compose.onNodeWithText(calendar.label).assertIsOn()
        compose.onNodeWithText(whatsapp.label).assertIsOff()
    }
    @Test fun togglingFilteredAppRetainsQueryAndHiddenSelections() {
        screen();query("wha")
        compose.onNodeWithText(whatsapp.label).assertIsOff().performClick().assertIsOn()
        compose.onNodeWithContentDescription("Search").assertTextEquals("wha")
        compose.onNodeWithText(calendar.label).assertDoesNotExist()
        compose.onNodeWithContentDescription("Clear search").performClick()
        compose.onNodeWithText(calendar.label).assertIsOn()
        compose.onNodeWithText(whatsapp.label).assertIsOn()
        query("WHA")
        compose.onNodeWithText(whatsapp.label).performClick().assertIsOff()
        compose.onNodeWithContentDescription("Search").assertTextEquals("WHA")
        compose.onNodeWithContentDescription("Clear search").performClick()
        compose.onNodeWithText(calendar.label).assertIsOn()
        compose.onNodeWithText(whatsapp.label).assertIsOff()
    }
    @Test fun largeAppListOnlyComposesVisibleRows() {
        val manyApps=(0 until 300).map {NotificationPickerApp("0/org.example.app$it","Fixture app %03d".format(it))}
        compose.setContent {
            PneuTheme {Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {
                NotificationsContent(Prefs(notificationConsent=NotificationConsent(enabled=true)),true,false,manyApps,{},{},{},{_,_,_->},{})
            }}
        }
        val appRows=hasText("Fixture app",substring=true)
        val initiallyComposed=compose.onAllNodes(appRows).fetchSemanticsNodes().size
        assertTrue("Expected only viewport rows, composed $initiallyComposed of 300",initiallyComposed in 1..25)
        compose.onNodeWithText(manyApps.last().label).assertDoesNotExist()
        compose.onNode(hasScrollToIndexAction()).performScrollToIndex(155)
        compose.onNodeWithText(manyApps.first().label).assertDoesNotExist()
        val afterScroll=compose.onAllNodes(appRows).fetchSemanticsNodes().size
        assertTrue("Expected a bounded viewport after scrolling, composed $afterScroll",afterScroll in 1..25)
    }
    @Test fun accessOffCardUsesConditionalRestrictedSettingsAdvice() {
        screen(blocked=true)
        compose.onNodeWithText("Notification access is off").assertIsDisplayed()
        compose.onNodeWithText("Turn on Pneu in notification access. If Android says it's restricted, open App info, tap ⋮ › Allow restricted settings, then try again.").assertIsDisplayed()
        compose.onNodeWithText("Open app info").assertIsDisplayed()
        compose.onNodeWithText("Android blocked notification access").assertDoesNotExist()
    }
}
