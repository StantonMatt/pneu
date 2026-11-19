package io.github.stantonmatt.pneu

import android.content.ClipboardManager
import android.content.Intent
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.datastore.preferences.core.edit
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.Endpoint
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

/** Settings mutations run in the real Activity, outside a test-owned composition. */
@RunWith(AndroidJUnit4::class)
class ClipboardHistoryDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private var activity:ActivityScenario<MainActivity>?=null
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset();app.savedClips.setEnabled(false)
        app.preferences.store.edit {p->PeerRegistry.write(p,listOf(Peer("Studio PC","a".repeat(64),"b".repeat(32),listOf(Endpoint("127.0.0.1",1)))))}
        app.preferences.toggle("setup",true)
        withTimeout(5000) {app.prefs.first {it.setup && it.devices.isNotEmpty()}}
    }
    @After fun cleanup()=runBlocking<Unit> {activity?.close();app.savedClips.setEnabled(false);DeviceFixture.reset()}
    private fun launch() {activity=ActivityScenario.launch(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK));waitText("Pneu")}
    private fun waitText(text:String) {compose.waitUntil(10000) {compose.onAllNodesWithText(text).fetchSemanticsNodes().isNotEmpty()}}
    private fun settings() {compose.onNodeWithContentDescription("Settings").performClick();waitText("Settings");compose.onNodeWithText("Keep history").performScrollTo()}
    private fun history() {compose.onNodeWithContentDescription("Clipboard history").performClick();waitText("Clipboard history")}
    private fun keepHistory()=compose.onNodeWithText("Keep history")
    @Test fun longPressPinsSearchFiltersBothGroupsAndLeavingClearsSearch()=runBlocking<Unit> {
        app.savedClips.setEnabled(true);app.savedClips.recordText("Café meeting");app.savedClips.recordText("Other note")
        launch();history()
        compose.onNodeWithText("Café meeting").performTouchInput {longClick()}
        compose.onNodeWithText("Pin").assertIsDisplayed().performClick();waitText("Pinned")
        assertTrue(app.savedClips.state.value.items.first {it.text=="Café meeting"}.pinned)
        compose.onNodeWithContentDescription("Search").performTextInput("CAFE")
        compose.onNodeWithText("Café meeting").assertIsDisplayed();compose.onNodeWithText("Other note").assertDoesNotExist()
        compose.onNodeWithText("Clear all").assertDoesNotExist()
        app.savedClips.recordText("cafe arrival");waitText("cafe arrival")
        compose.onNodeWithContentDescription("Search").performTextReplacement("invoice")
        compose.onNodeWithText("No matches").assertIsDisplayed()
        compose.onNodeWithContentDescription("Clear search").performClick()
        compose.onNodeWithText("Other note").assertIsDisplayed()
        compose.onNodeWithText("Café meeting").performTouchInput {longClick()}
        compose.onNodeWithText("Unpin").assertIsDisplayed().performClick()
        compose.waitUntil(5000) {app.savedClips.state.value.items.none {it.pinned}}
        compose.onNodeWithText("Pinned").assertDoesNotExist()
        compose.onNodeWithContentDescription("Search").performTextInput("invoice")
        compose.onNodeWithContentDescription("Back").performClick();history()
        compose.onNodeWithText("Other note").assertIsDisplayed()
    }
    @Test fun lastSearchResultScrollsAboveVisibleKeyboard()=runBlocking<Unit> {
        app.savedClips.setEnabled(true)
        app.savedClips.recordText("Keyboard result 01 - final row")
        for(n in 2..20)app.savedClips.recordText("Keyboard result ${n.toString().padStart(2,'0')}")
        launch();history()
        compose.onNodeWithContentDescription("Search").performClick()
        compose.onNodeWithContentDescription("Search").performTextInput("Keyboard result")
        activity!!.onActivity {it.window.insetsController!!.show(android.view.WindowInsets.Type.ime())}
        var keyboardTop=0
        compose.waitUntil(10000) {
            var visible=false
            activity!!.onActivity {a->
                val insets=a.window.decorView.rootWindowInsets
                visible=insets?.isVisible(android.view.WindowInsets.Type.ime())==true
                if(visible)keyboardTop=a.window.decorView.height-insets!!.getInsets(android.view.WindowInsets.Type.ime()).bottom
            }
            visible
        }
        val last=compose.onNodeWithText("Keyboard result 01 - final row")
        compose.onNode(hasScrollToIndexAction()).performScrollToNode(hasText("Keyboard result 01 - final row"))
        last.assertIsDisplayed()
        assertTrue("Final result must be completely above the keyboard",last.fetchSemanticsNode().boundsInWindow.bottom<=keyboardTop)
        // Optional host screenshot barrier, after the real IME and final row assertions.
        if(InstrumentationRegistry.getArguments().getString("imeScreenshot")=="true") {
            val marker=java.io.File(c.filesDir,"ime-capture-ready");val ack=java.io.File(c.filesDir,"ime-capture-ack")
            try {
                ack.delete();marker.writeText("final-row-above-ime")
                withTimeout(120000) {while(!ack.exists())delay(50)}
            } finally {marker.delete();ack.delete()}
        }
    }
    @Test fun clearKeepsPinsAndTurningOffCountsAndDeletesThem()=runBlocking<Unit> {
        app.savedClips.setEnabled(true);app.savedClips.recordText("pinned note")
        app.savedClips.setPinned(app.savedClips.state.value.items.single().id,true)
        app.savedClips.recordText("recent note")
        launch();history();compose.onNodeWithText("Clear all").performClick()
        compose.onNodeWithText("This deletes 1 saved item. Pinned items stay.").assertIsDisplayed()
        compose.onNodeWithText("Clear").performClick();compose.onNodeWithText("pinned note").assertIsDisplayed()
        compose.onNodeWithText("Clear all").assertDoesNotExist()
        compose.onNodeWithContentDescription("Back").performClick();settings();keepHistory().performClick()
        compose.onNodeWithText("This deletes 1 pinned item.").assertIsDisplayed()
        compose.onNodeWithText("Cancel").performClick();keepHistory().assertIsOn()
        app.savedClips.recordText("another recent")
        keepHistory().performClick();compose.onNodeWithText("This deletes 2 saved items, including 1 pinned.").assertIsDisplayed()
        compose.onNodeWithText("Turn off").performClick()
        compose.waitUntil(5000) {!app.savedClips.state.value.enabled};assertTrue(app.savedClips.state.value.items.isEmpty())
    }
    @Test fun settingStartsOffHomeIconIsConditionalAndEmptyTurnOffHasNoDialog()=runBlocking<Unit> {
        launch();compose.onNodeWithContentDescription("Clipboard history").assertDoesNotExist()
        settings();keepHistory().assertIsOff().performClick()
        compose.waitUntil(5000) {app.savedClips.state.value.enabled};keepHistory().assertIsOn()
        compose.onNodeWithContentDescription("Back").performClick();history()
        compose.onNodeWithText("Nothing sent or received yet").assertIsDisplayed();compose.onNodeWithText("Clear all").assertDoesNotExist()
        compose.onNodeWithContentDescription("Back").performClick();settings();keepHistory().performClick()
        compose.waitUntil(5000) {!app.savedClips.state.value.enabled};keepHistory().assertIsOff()
        compose.onNodeWithText("Turn off clipboard history?").assertDoesNotExist()
        compose.onNodeWithContentDescription("Back").performClick();compose.onNodeWithContentDescription("Clipboard history").assertDoesNotExist()
    }
    @Test fun copyingStaysInPlaceArrivalsPrependAndDeleteDoesNotChangeClipboard()=runBlocking<Unit> {
        app.savedClips.setEnabled(true);app.savedClips.recordText("older note","Studio PC");app.savedClips.recordText("newer note")
        launch();history()
        val oldTop=compose.onNodeWithText("older note").fetchSemanticsNode().boundsInRoot.top
        compose.onNodeWithText("older note").performClick();waitText("Copied")
        assertEquals(oldTop,compose.onNodeWithText("older note").fetchSemanticsNode().boundsInRoot.top)
        assertEquals("older note",withContext(Dispatchers.Main) {c.getSystemService(ClipboardManager::class.java).primaryClip!!.getItemAt(0).text.toString()})
        assertEquals("Studio PC",app.savedClips.state.value.items.first().sender)
        compose.waitUntil(5000) {compose.onAllNodesWithText("Copied").fetchSemanticsNodes().isEmpty()}
        app.savedClips.recordText("new arrival","MacBook Air");waitText("new arrival")
        assertTrue(compose.onNodeWithText("new arrival").fetchSemanticsNode().boundsInRoot.top<compose.onNodeWithText("newer note").fetchSemanticsNode().boundsInRoot.top)
        app.savedClips.recordText("newer note","Studio PC")
        compose.waitUntil(5000) {compose.onNodeWithText("newer note").fetchSemanticsNode().boundsInRoot.top<compose.onNodeWithText("new arrival").fetchSemanticsNode().boundsInRoot.top}
        compose.onAllNodesWithContentDescription("Delete")[2].performClick()
        compose.waitUntil(5000) {app.savedClips.state.value.items.none {it.text=="older note"}}
        assertEquals("older note",withContext(Dispatchers.Main) {c.getSystemService(ClipboardManager::class.java).primaryClip!!.getItemAt(0).text.toString()})
    }
    @Test fun clearAllSupportsCancelAndConfirmationAndLastDeletionShowsEmpty()=runBlocking<Unit> {
        app.savedClips.setEnabled(true);app.savedClips.recordText("one");app.savedClips.recordText("two")
        launch();history();compose.onNodeWithText("Clear all").performClick();waitText("Clear clipboard history?")
        compose.onNodeWithText("This deletes 2 saved items.").assertIsDisplayed();compose.onNodeWithText("Cancel").performClick()
        assertEquals(2,app.savedClips.state.value.items.size)
        compose.onNodeWithText("Clear all").performClick();compose.onNodeWithText("Clear").performClick();waitText("Nothing sent or received yet")
        assertTrue(app.savedClips.state.value.enabled)
        app.savedClips.recordText("last");waitText("last");compose.onNodeWithContentDescription("Delete").performClick();waitText("Nothing sent or received yet")
    }
    @Test fun turningOffKeepsSwitchOnUntilConfirmedAndWipesImageFiles()=runBlocking<Unit> {
        app.savedClips.setEnabled(true);app.savedClips.recordText("saved")
        val bitmap=android.graphics.Bitmap.createBitmap(12,12,android.graphics.Bitmap.Config.ARGB_8888)
        val bytes=java.io.ByteArrayOutputStream().also {bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG,100,it);bitmap.recycle()}.toByteArray()
        app.savedClips.recordImage("image/png") {bytes.inputStream()}
        launch();settings();keepHistory().performClick();waitText("Turn off clipboard history?")
        assertTrue(app.savedClips.state.value.enabled);assertEquals(2,app.savedClips.state.value.items.size)
        compose.onNodeWithText("Cancel").performClick();keepHistory().assertIsOn()
        keepHistory().performClick();compose.onNodeWithText("Turn off").performClick()
        compose.waitUntil(5000) {!app.savedClips.state.value.enabled};keepHistory().assertIsOff()
        assertTrue(app.savedClips.state.value.items.isEmpty());assertTrue(java.io.File(c.filesDir,"clipboard-history/images").listFiles()!!.isEmpty())
        compose.onNodeWithContentDescription("Back").performClick();compose.onNodeWithContentDescription("Clipboard history").assertDoesNotExist()
    }
}
