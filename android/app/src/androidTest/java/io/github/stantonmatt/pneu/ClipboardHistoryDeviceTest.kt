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
