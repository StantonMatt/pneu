package io.github.stantonmatt.pneu

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream
import java.io.File

/** Presentation fixtures. A host captures through the device session, then
 * acknowledges each scene; this class never takes device screenshots itself. */
@RunWith(AndroidJUnit4::class)
class ClipboardHistoryScreensDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val marker get()=File(c.filesDir,"history-capture-ready")
    private val ack get()=File(c.filesDir,"history-capture-ack")
    @Before fun setup()=runBlocking<Unit> {DeviceFixture.reset();app.savedClips.setEnabled(false)}
    @After fun cleanup()=runBlocking<Unit> {marker.delete();ack.delete();app.savedClips.setEnabled(false);DeviceFixture.reset()}
    private fun capture(name:String) {
        compose.waitForIdle()
        // Android's clipboard overlay uses a real six-second timeout, separate
        // from Compose's clock. Keep it in Copied; let it leave other scenes.
        if(!name.startsWith("20-"))Thread.sleep(6500)
        ack.delete();marker.writeText(name)
        runBlocking {withTimeout(15000) {while(!ack.exists())delay(25)}}
        marker.delete();ack.delete()
    }
    private fun image():ByteArray {
        val bitmap=Bitmap.createBitmap(160,120,Bitmap.Config.ARGB_8888)
        val canvas=Canvas(bitmap);val paint=Paint().apply {color=0xff8cbbff.toInt()};canvas.drawRect(0f,0f,160f,120f,paint)
        paint.color=0xffffedb0.toInt();canvas.drawCircle(125f,25f,13f,paint)
        paint.color=0xff397d5e.toInt();canvas.drawCircle(40f,150f,100f,paint);paint.color=0xff24553f.toInt();canvas.drawCircle(125f,160f,105f,paint)
        return ByteArrayOutputStream().also {bitmap.compress(Bitmap.CompressFormat.PNG,100,it);bitmap.recycle()}.toByteArray()
    }
    @OptIn(ExperimentalMaterial3Api::class)
    private fun scenes(dark:Boolean)=runBlocking<Unit> {
        Assume.assumeTrue(InstrumentationRegistry.getArguments().getString("historyScreenshots")=="true")
        app.savedClips.setEnabled(true)
        app.savedClips.recordText("¡Feliz cumpleaños, abuela! 🎉","Studio PC")
        app.savedClips.recordText("Tracking number 1Z 999 AA1 01 2345 6784","Studio PC")
        app.savedClips.recordText("1600 Amphitheatre Parkway, Mountain View, CA 94043")
        val bytes=image();app.savedClips.recordImage("image/png") {bytes.inputStream()}
        app.savedClips.recordText("Hi Ana, Thanks for sending the draft. I've added comments on the proposal.","Studio PC")
        app.savedClips.recordText("https://github.com/StantonMatt/pneu","Studio PC")
        app.savedClips.recordText("Meet at the north entrance at 6")
        val p=app.preferences.read().copy(setup=true,peer=Peer("Studio PC","a".repeat(64),"b".repeat(32),emptyList()))
        val scene=mutableStateOf("home")
        compose.activityRule.scenario.onActivity {val transparent=android.graphics.Color.TRANSPARENT;val bars=if(dark)SystemBarStyle.dark(transparent)else SystemBarStyle.light(transparent,transparent);it.enableEdgeToEdge(bars,bars)}
        compose.setContent {val history by app.savedClips.state.collectAsState();CompositionLocalProvider(LocalUiActions provides UiActions {message,_->fail("Unexpected fixture action: $message")}) {PneuTheme(dark=dark) {Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {
            when(scene.value) {
                "home"->Scaffold(containerColor=LocalPneuColors.current.bg,topBar={HomeTopBar(false,history={}) {}}) {padding->HomeScreen(p,LinkState("Connected","Wi-Fi"),emptyList(),Health(true,true,true,true),Modifier.padding(padding),{},{},{})}
                "settings","off"->{SettingsScreen(p,Health(true,true,true,true),0,{},{})
                    if(scene.value=="off")HistoryDeleteDialog(history.items.size,true,{},{})}
                else->ClipboardHistoryScreen {}
            }
        }}}}
        val theme=if(dark)"dark"else "light"
        capture("17-home-$theme")
        compose.runOnUiThread {scene.value="settings"};compose.onNodeWithText("Keep history").performScrollTo();capture("17-setting-$theme")
        compose.runOnUiThread {scene.value="history"};compose.onNodeWithText("Clipboard history").assertIsDisplayed();capture("19-history-$theme")
        compose.mainClock.autoAdvance=false
        compose.onNodeWithText("Meet at the north entrance at 6").performClick()
        compose.waitUntil(5000) {compose.mainClock.advanceTimeByFrame();compose.onAllNodesWithText("Copied").fetchSemanticsNodes().isNotEmpty()}
        compose.onNodeWithText("Copied").assertIsDisplayed();capture("20-copied-$theme")
        compose.mainClock.autoAdvance=true
        compose.onNodeWithText("Clear all").performClick();compose.onNodeWithText("Clear clipboard history?").assertIsDisplayed();capture("clear-dialog-$theme")
        compose.onNodeWithText("Cancel").performClick()
        compose.runOnUiThread {scene.value="off"};capture("off-dialog-$theme")
        app.savedClips.clear();compose.runOnUiThread {scene.value="empty"};compose.onNodeWithText("Nothing sent or received yet").assertIsDisplayed();capture("22-empty-$theme")
    }
    @Test fun light()=scenes(false)
    @Test fun dark()=scenes(true)
}
