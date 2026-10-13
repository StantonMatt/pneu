package io.github.stantonmatt.pneu

import android.Manifest
import android.graphics.Bitmap
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.Endpoint
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

/** Controlled presentation fixtures. Real pairing/transfers remain covered by the peer suites.
 * Separate cases ensure a failed capture does not prevent the remaining states from running. */
@RunWith(AndroidJUnit4::class)
class V2ScreensDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val instrumentation=InstrumentationRegistry.getInstrumentation()
    private val context=instrumentation.targetContext
    private val app=context.graph
    private enum class Scene { CONNECTED,SENDING,CONNECTING,FAILED,WAITING_MENU,UNPAIR,PAIR,PERMISSIONS,SETUP }
    private val sent=Outgoing().apply {
        mediaId="v2-sent";transferId="v2-sent-transfer";kind="text";mime="text/plain"
        text="A note from this phone";state="sent";automatic=false;createdAt=System.currentTimeMillis()-600000
    }
    private val sending=Outgoing().apply {
        mediaId="v2-sending";transferId="v2-sending-transfer";kind="screenshot";state="sending"
        size=1048576;createdAt=System.currentTimeMillis()
    }
    private val failed=Outgoing().apply {
        mediaId="v2-failed";transferId="v2-failed-transfer";kind="file";mime="application/pdf"
        fileName="notes.pdf";state="failed";error="disk_full";automatic=false;createdAt=System.currentTimeMillis()-300000
    }
    private val waiting=Outgoing().apply {
        mediaId="v2-waiting";transferId="v2-waiting-transfer";kind="recording";mime="video/mp4"
        state="waiting";createdAt=System.currentTimeMillis()-120000
    }

    @Before fun reset()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,
            Manifest.permission.POST_NOTIFICATIONS,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))
            instrumentation.uiAutomation.grantRuntimePermission(context.packageName,permission)
    }
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.reset()}

    private fun capture(name:String) {
        compose.waitForIdle()
        Thread.sleep(350) // Let the native dialog/system-bar animation and drawing settle.
        val bitmap=requireNotNull(instrumentation.uiAutomation.takeScreenshot()) {"Screenshot unavailable: $name"}
        try {
            val target=File(context.filesDir,"v2-screens/$name.png").apply {parentFile!!.mkdirs()}
            target.outputStream().use {assertTrue("PNG save failed: $name",bitmap.compress(Bitmap.CompressFormat.PNG,100,it))}
        } finally {bitmap.recycle()}
    }

    @OptIn(ExperimentalMaterial3Api::class)
    private suspend fun render(scene:Scene) {
        val prefs=app.preferences.read().copy(setup=true,peer=Peer("Studio PC","a".repeat(64),"d".repeat(32),
            listOf(Endpoint("192.168.1.20",47821),Endpoint("100.67.144.28",47821))))
        val rows=when(scene) {
            Scene.SENDING->listOf(sending,sent)
            Scene.CONNECTING->listOf(waiting,sent)
            Scene.FAILED,Scene.WAITING_MENU,Scene.UNPAIR->listOf(waiting,failed,sent)
            else->listOf(sent)
        }
        val pending=rows.filter {it.state!="sent"}
        withContext(Dispatchers.IO) {pending.forEach {app.queue.insert(it)}}
        withTimeout(5000) {app.waitingCount.first {it==pending.size}}
        // Infinite animation policy cancels loops under autoAdvance; explicitly
        // drive and sample the capsule/dashes instead of capturing their first frame.
        compose.mainClock.autoAdvance=scene!=Scene.SENDING && scene!=Scene.CONNECTING
        val transparent=android.graphics.Color.TRANSPARENT
        compose.activityRule.scenario.onActivity {
            val style=if(scene==Scene.SETUP)SystemBarStyle.light(transparent,transparent) else SystemBarStyle.dark(transparent)
            it.enableEdgeToEdge(style,style)
        }
        compose.setContent {
            CompositionLocalProvider(LocalUiActions provides UiActions {message,_->fail("Unexpected fixture action: $message")}) {
                PneuTheme(dark=scene!=Scene.SETUP) {
                    Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {
                        when(scene) {
                            Scene.SETUP,Scene.PERMISSIONS->SetupScreen(scene==Scene.SETUP,context.health(),0,{},{},{})
                            Scene.PAIR-> {
                                PairScreen(prefs.copy(peer=null),LinkState(),{},{},{},cameraPreview={modifier,_->Box(modifier.background(Color(0xFF303236)))})
                                PneuDialog(pairingHeader=true,onDismissRequest={},title={Text(context.getString(R.string.pair_confirm_title,"Studio PC"))},
                                    confirmButton={PneuTextButton({}){Text(context.getString(R.string.action_pair))}},
                                    dismissButton={PneuTextButton({}){Text(context.getString(R.string.action_cancel))}})
                            }
                            else-> {
                                var scrolled by remember {mutableStateOf(false)}
                                val link=when(scene) {
                                    Scene.SENDING->LinkState("Connected","Wi-Fi",sent=524288,total=1048576,transferMediaId=sending.mediaId,transferKind="screenshot")
                                    Scene.CONNECTING->LinkState("Connecting…")
                                    else->LinkState("Connected","Wi-Fi")
                                }
                                Scaffold(containerColor=LocalPneuColors.current.bg,contentWindowInsets=WindowInsets.statusBars,topBar={HomeTopBar(scrolled){}}) {padding->
                                    HomeScreen(prefs,link,rows,Health(true,true,true,true),Modifier.padding(padding),{},{},{scrolled=it})
                                }
                                if(scene==Scene.UNPAIR)PneuDialog(destructive=true,onDismissRequest={},title={Text(context.getString(R.string.unpair_title,"Studio PC"))},
                                    text={Text(context.resources.getQuantityString(R.plurals.unpair_text,pending.size,pending.size))},
                                    confirmButton={PneuTextButton({}){Text(context.getString(R.string.action_unpair))}},
                                    dismissButton={PneuTextButton({}){Text(context.getString(R.string.action_cancel))}})
                            }
                        }
                    }
                }
            }
        }
        if(!compose.mainClock.autoAdvance)compose.mainClock.advanceTimeByFrame()
    }

    @Test(timeout=20000) fun darkConnectedHomeAndStableRecentHeader()=runBlocking<Unit> {
        render(Scene.CONNECTED)
        compose.onNodeWithText("Connected").assertIsDisplayed()
        capture("v2-dark-home-connected")
        val before=compose.onNodeWithText("Recent").fetchSemanticsNode().boundsInRoot
        withContext(Dispatchers.IO) {app.queue.insert(waiting)}
        compose.waitUntil(5000) {app.waitingCount.value==1 && compose.onAllNodesWithText("1 waiting").fetchSemanticsNodes().isNotEmpty()}
        val after=compose.onNodeWithText("Recent").fetchSemanticsNode().boundsInRoot
        assertEquals("Recent title must not jump when waiting appears",before.top,after.top,.5f)
    }
    @Test(timeout=20000) fun sendingCapsuleAndProgress()=runBlocking<Unit> {
        render(Scene.SENDING)
        val start=compose.mainClock.currentTime
        compose.waitUntil(5000) {
            compose.mainClock.advanceTimeByFrame()
            compose.onAllNodesWithText("Sending ·",substring=true).fetchSemanticsNodes().isNotEmpty()
        }
        val elapsed=compose.mainClock.currentTime-start
        compose.mainClock.advanceTimeBy(1400-elapsed%1400+700)
        compose.onNodeWithText("Sending ·",substring=true).assertIsDisplayed()
        capture("v2-dark-sending")
    }
    @Test(timeout=20000) fun connectingDashes()=runBlocking<Unit> {
        render(Scene.CONNECTING);compose.mainClock.advanceTimeBy(400)
        compose.onNodeWithText("Connecting…").assertIsDisplayed();capture("v2-dark-connecting")
    }
    @Test(timeout=20000) fun failedRowAndRecordingWithoutPreview()=runBlocking<Unit> {
        render(Scene.FAILED)
        compose.onNodeWithText("Failed · PC disk is full").assertIsDisplayed();capture("v2-dark-failed-row")
    }
    @Test(timeout=20000) fun waitingMenu()=runBlocking<Unit> {
        render(Scene.WAITING_MENU);compose.onNodeWithText("2 waiting").performClick()
        compose.onNodeWithText("Send now").assertIsDisplayed();capture("v2-dark-waiting-menu")
    }
    @Test(timeout=20000) fun unpairConfirmation()=runBlocking<Unit> {
        render(Scene.UNPAIR)
        compose.onNodeWithText("Unpair from Studio PC?").assertIsDisplayed();capture("v2-dark-unpair-confirm")
    }
    @Test(timeout=20000) fun pairConfirmation()=runBlocking<Unit> {
        render(Scene.PAIR)
        compose.onNodeWithText("Pair with Studio PC?").assertIsDisplayed();capture("v2-dark-pair-confirm")
    }
    @Test(timeout=20000) fun permissionsAndBattery()=runBlocking<Unit> {
        render(Scene.PERMISSIONS)
        compose.onNodeWithText("Permissions and battery").assertIsDisplayed();capture("v2-dark-permissions-battery")
    }
    @Test(timeout=20000) fun lightSetupCroppedIcon()=runBlocking<Unit> {
        render(Scene.SETUP)
        compose.onNodeWithText("Set up Pneu").assertIsDisplayed();capture("v2-light-setup")
    }
}
