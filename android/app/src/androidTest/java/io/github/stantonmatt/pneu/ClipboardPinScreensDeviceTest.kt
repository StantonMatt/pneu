package io.github.stantonmatt.pneu

import android.content.Intent
import android.graphics.Bitmap
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
import org.junit.runner.RunWith
import java.io.File

/** Real MainActivity scenes; screenshots are taken by the host device session. */
@RunWith(AndroidJUnit4::class)
class ClipboardPinScreensDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val c=InstrumentationRegistry.getInstrumentation().targetContext;private val app=c.graph
    private var activity:ActivityScenario<MainActivity>?=null
    private val marker get()=File(c.filesDir,"pin-capture-ready")
    private val ack get()=File(c.filesDir,"pin-capture-ack")
    @After fun cleanup()=runBlocking<Unit> {activity?.close();marker.delete();ack.delete();app.savedClips.setEnabled(false);DeviceFixture.reset()}
    private fun capture(name:String) {
        compose.waitForIdle();InstrumentationRegistry.getInstrumentation().waitForIdleSync();ack.delete();marker.writeText(name)
        runBlocking {withTimeout(120000) {while(!ack.exists())delay(50)}}
        marker.delete();ack.delete()
    }
    @Test fun scenes()=runBlocking<Unit> {
        val theme=InstrumentationRegistry.getArguments().getString("pinScreenshots")
        Assume.assumeTrue(theme=="light" || theme=="dark")
        DeviceFixture.reset();app.savedClips.setEnabled(false);app.savedClips.setEnabled(true)
        app.preferences.store.edit {p->PeerRegistry.write(p,listOf(Peer("Studio PC","a".repeat(64),"b".repeat(32),listOf(Endpoint("127.0.0.1",1)))))}
        app.preferences.toggle("setup",true);withTimeout(5000) {app.prefs.first {it.setup && it.devices.isNotEmpty()}}
        app.savedClips.recordText("1600 Amphitheatre Parkway, Mountain View, CA 94043")
        app.savedClips.recordText("Hi Ana, Thanks for sending the draft. I've added comments on the proposal.","Studio PC")
        val bitmap=Bitmap.createBitmap(128,96,Bitmap.Config.ARGB_8888).apply {eraseColor(0xff8cbbff.toInt())}
        val bytes=java.io.ByteArrayOutputStream().also {bitmap.compress(Bitmap.CompressFormat.PNG,100,it);bitmap.recycle()}.toByteArray()
        app.savedClips.recordImage("image/png","MacBook Air") {bytes.inputStream()}
        app.savedClips.recordText("https://github.com/StantonMatt/pneu","Studio PC")
        app.savedClips.recordText("Meet at the north entrance at 6")
        app.savedClips.recordText("https://meet.example.com/pneu-weekly","Studio PC")
        app.savedClips.setPinned(app.savedClips.state.value.items.first().id,true)
        app.savedClips.recordText("Calle Los Aromos 1840, Depto 52, Providencia")
        app.savedClips.setPinned(app.savedClips.state.value.items.first {it.text=="Calle Los Aromos 1840, Depto 52, Providencia"}.id,true)
        activity=ActivityScenario.launch(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        compose.waitUntil(10000) {compose.onAllNodesWithContentDescription("Clipboard history").fetchSemanticsNodes().isNotEmpty()}
        compose.onNodeWithContentDescription("Clipboard history").performClick()
        compose.onNodeWithText("Pinned").assertIsDisplayed();capture("pinned-recent-$theme")
        val menuLabel=if(theme=="dark")"Unpin"else "Pin"
        compose.onNodeWithText(if(theme=="dark")"meet.example.com/pneu-weekly"else "Meet at the north entrance at 6").performTouchInput {longClick()}
        compose.onNodeWithText(menuLabel).assertIsDisplayed();capture("long-press-$theme")
        androidx.test.espresso.Espresso.pressBack()
        compose.onNodeWithContentDescription("Search").performClick()
        compose.onNodeWithContentDescription("Search").performTextInput("meet")
        compose.onNodeWithText("Meet at the north entrance at 6").assertIsDisplayed()
        compose.onNodeWithText("meet.example.com/pneu-weekly").assertIsDisplayed()
        compose.onNodeWithText("Pinned").assertIsDisplayed();compose.onNodeWithText("Recent").assertIsDisplayed();capture("searching-$theme")
        compose.onNodeWithContentDescription("Search").performTextReplacement("invoice")
        compose.onNodeWithText("No matches").assertIsDisplayed();capture("no-matches-$theme")
    }
}
