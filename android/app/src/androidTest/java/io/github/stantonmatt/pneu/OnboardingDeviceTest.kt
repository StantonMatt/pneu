package io.github.stantonmatt.pneu

import android.Manifest
import android.content.ClipboardManager
import androidx.activity.ComponentActivity
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.Invitation
import io.github.stantonmatt.pneu.ui.PneuTheme
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

/** First run without the computer app, first pairing errors, and an unreachable computer. */
@RunWith(AndroidJUnit4::class)
class OnboardingDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val failures=mutableListOf<String>()
    private val raw="pneu://pair?v=1&name=Onboarding%20PC&fp=${"c".repeat(64)}&addr=127.0.0.1%3A1&token=${"e".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private fun content(body:@Composable ()->Unit) {
        compose.setContent {CompositionLocalProvider(LocalUiActions provides UiActions {message,_->failures+=message}) {body()}}
    }
    private fun awaitText(text:String) {
        compose.waitUntil(5000) {compose.onAllNodesWithText(text).fetchSemanticsNodes().isNotEmpty()}
        compose.onNodeWithText(text).assertIsDisplayed()
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        DeviceFixture.stopPneuService();app.connection.stop();app.initialized.await();delay(200);app.forgetBinding()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.CAMERA)
    }
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.setWifi(i,true);DeviceFixture.stopPneuService();app.connection.stop();app.forgetBinding()}

    @Test fun scannerOffersTheDownloadPageToCopyOrShare()=runBlocking<Unit> {
        val prefs=app.preferences.read()
        content {PairScreen(prefs,LinkState(),{},{},{},cameraPreview={_,_->})}
        awaitText("Scan the QR code in Pneu on your computer")
        compose.onNodeWithText("Get Pneu for your computer").performClick()
        awaitText("For Windows, Mac and Linux.")
        compose.onNodeWithText("Share link").assertIsDisplayed()
        compose.onNodeWithText("stantonmatt.github.io/pneu").performClick()
        compose.runOnIdle {
            assertEquals("https://stantonmatt.github.io/pneu/",c.getSystemService(ClipboardManager::class.java).primaryClip?.getItemAt(0)?.text?.toString())
        }
        assertTrue(failures.toString(),failures.isEmpty())
    }
    @Test fun scannerHidesGetPneuWhilePairing()=runBlocking<Unit> {
        val prefs=app.preferences.read()
        assertTrue(app.requestPairing(raw,null))
        content {PairScreen(prefs,LinkState(),{},{},{},cameraPreview={_,_->})}
        awaitText("Pairing…")
        compose.onNodeWithText("Get Pneu for your computer").assertDoesNotExist()
        assertTrue(app.cancelPairing(app.connection.attempt.value))
        awaitText("Get Pneu for your computer")
    }
    @Test(timeout=60000) fun cantReachNamesTheComputerAndFollowsThePhonesNetwork()=runBlocking<Unit> {
        val prefs=app.preferences.read()
        val tailscale=ReachabilityHints.tailscaleInstalled(c)
        var link by mutableStateOf(LinkState())
        DeviceFixture.setWifi(i,true)
        content {PairScreen(prefs,link,{},{},{},lastAttemptedInvitation=raw,cameraPreview={_,_->})}
        compose.runOnIdle {link=LinkState(pairing="Can't reach your PC")}
        awaitText("Can't reach Onboarding PC")
        awaitText(if(tailscale)"Make sure Onboarding PC is on this Wi-Fi, or that Tailscale is on for both." else "Make sure Onboarding PC is on this Wi-Fi.")
        compose.onNodeWithText("Try again").assertIsDisplayed()
        // Leaving Wi-Fi changes the likely cause and the action while the dialog is open.
        DeviceFixture.setWifi(i,false)
        awaitText("Your phone isn't on Wi-Fi. Connect it to the same Wi-Fi as Onboarding PC.")
        compose.onNodeWithText("Wi-Fi settings").assertIsDisplayed()
        compose.onNodeWithText("Try again").assertDoesNotExist()
        DeviceFixture.setWifi(i,true)
        awaitText("Try again")
        compose.onNodeWithText("Cancel").performClick()
        compose.waitUntil(5000) {compose.onAllNodesWithText("Can't reach Onboarding PC").fetchSemanticsNodes().isEmpty()}
        assertTrue(failures.toString(),failures.isEmpty())
    }
    @Test(timeout=20000) fun expiredCodeAsksForTheNewOneUntilOk()=runBlocking<Unit> {
        app.preferences.toggle("setup",true)
        content {PneuScreen()}
        awaitText("Scan the QR code in Pneu on your computer")
        val expired=raw.replace(Regex("exp=\\d+"),"exp=${System.currentTimeMillis()-1000}")
        // Two identical rejections are two events: each shows the dialog again.
        repeat(2) {
            assertFalse(beginPair(c,expired))
            awaitText("This code expired")
            compose.onNodeWithText("Scan the new one.").assertIsDisplayed()
            compose.onNodeWithText("OK").performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("This code expired").fetchSemanticsNodes().isEmpty()}
        }
        compose.onNodeWithText("Scan the QR code in Pneu on your computer").assertIsDisplayed()
    }
    @Test(timeout=60000) fun unreachableHomeSaysWhatThePhoneCanTellWithoutTailscale()=runBlocking<Unit> {
        Assume.assumeFalse("Tailscale installed on this device",ReachabilityHints.tailscaleInstalled(c))
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"f".repeat(32)))
        val p=app.preferences.read()
        DeviceFixture.setWifi(i,true)
        content {PneuTheme {HomeScreen(p,LinkState(status="PC unreachable"),emptyList(),Health(true,true,true,true),Modifier,{},{},{})}}
        awaitText("Unreachable")
        awaitText("Onboarding PC may be off or on another Wi-Fi")
        DeviceFixture.setWifi(i,false)
        awaitText("Connects on the same Wi-Fi")
        // Tailscale isn't installed: never offer to open it.
        compose.onNodeWithText("Open Tailscale").assertDoesNotExist()
        compose.onNodeWithText("Connect anywhere").performClick()
        awaitText("Connect from anywhere")
        compose.onNodeWithText("Free app · install on each device").assertIsDisplayed()
        compose.onNodeWithText("Get").assertIsDisplayed()
        assertTrue(failures.toString(),failures.isEmpty())
    }
}
