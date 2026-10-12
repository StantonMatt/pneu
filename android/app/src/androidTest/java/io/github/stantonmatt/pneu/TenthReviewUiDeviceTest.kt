package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import androidx.activity.ComponentActivity
import androidx.compose.runtime.*
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.Invitation
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class TenthReviewUiDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private fun content(body:@Composable ()->Unit) {
        compose.setContent {
            CompositionLocalProvider(LocalUiActions provides UiActions {_,_->fail("Unexpected UI failure")}) {body()}
        }
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(200);app.forgetBinding()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.CAMERA)
    }
    @After fun cleanup()=runBlocking<Unit> {c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()}
    @Test(timeout=20000) fun repeatedCameraResultsCannotReadmitInvitationDuringSuccessTransition()=runBlocking<Unit> {
        val raw="pneu://pair?v=1&name=Tenth&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        val invitation=Invitation.parse(raw)
        assertTrue(app.requestPairing(raw,null))
        var prefs by mutableStateOf(app.preferences.read());var link by mutableStateOf(LinkState(status="Connecting…",pairing="Pairing…"))
        var scan:((String)->Unit)?=null;var admitted=0;var home=0;var scannerSession by mutableIntStateOf(0)
        compose.mainClock.autoAdvance=false
        content {
            CompositionLocalProvider(LocalUiActions provides UiActions {_,_->fail("Unexpected UI failure")}) {
                key(scannerSession) {PairScreen(prefs,link,{home++},{},{admitted++},cameraPreview={_,onCode->scan=onCode})}
            }
        }
        compose.waitForIdle();compose.runOnIdle {requireNotNull(scan)(raw)};assertEquals(0,admitted)
        assertTrue(app.installBinding(invitation,"d".repeat(32)))
        val paired=app.preferences.read()
        // DataStore can publish the binding before either READY or its marker.
        compose.runOnIdle {prefs=paired}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.runOnIdle {requireNotNull(scan)(raw)};assertEquals(0,admitted)
        compose.runOnIdle {link=LinkState(status="Connecting…")}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.runOnIdle {requireNotNull(scan)(raw)};assertEquals(0,admitted)
        compose.runOnIdle {link=LinkState(status="Connected",pairing="Paired with Tenth",pairingPin=invitation.fingerprint)}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        // Both the cleared pending state and changed peer pin reset the old guard.
        compose.runOnIdle {repeat(5) {requireNotNull(scan)(raw)}}
        assertEquals(0,admitted);assertEquals(0,home)
        compose.mainClock.advanceTimeBy(850);compose.waitForIdle();compose.runOnIdle {assertEquals(1,home)}
        // Reopening Pair after Home starts a fresh scanner even if the old
        // session still carries its success marker.
        compose.runOnIdle {scannerSession++}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.runOnIdle {requireNotNull(scan)(raw)};assertEquals(1,admitted)
    }
    @Test(timeout=15000) fun retryOffersTheConfirmedAttemptInsteadOfANewerCameraRead()=runBlocking<Unit> {
        val raw=invitationUri(invitation())
        val scanned=raw.replace("name=Twelfth","name=NewScan")
        val p=app.preferences.read();var link by mutableStateOf(LinkState(status="PC unreachable"))
        var scan:((String)->Unit)?=null;val offered=mutableListOf<String>()
        content {PairScreen(p,link,{},{},{offered.add(it)},lastAttemptedInvitation=raw,cameraPreview={_,code->scan=code})}
        compose.waitForIdle();compose.runOnIdle {requireNotNull(scan)(scanned)}
        assertEquals(listOf(scanned),offered)
        compose.runOnIdle {link=LinkState(status="PC unreachable",pairing="Can't reach your PC")}
        compose.waitUntil(5000) {compose.onAllNodesWithText("Try again").fetchSemanticsNodes().isNotEmpty()}
        compose.onNodeWithText("Try again").performClick()
        compose.waitUntil(5000) {offered.size==2}
        assertEquals(listOf(scanned,raw),offered);assertNull(app.preferences.read().pending)
    }
    private fun invitationUri(invitation:Invitation)="pneu://pair?v=1&name=${invitation.name}&fp=${invitation.fingerprint}&addr=127.0.0.1%3A1&token=${invitation.token}&exp=${invitation.expires}"
    private fun invitation()=Invitation.parse("pneu://pair?v=1&name=Twelfth&fp=${"c".repeat(64)}&addr=127.0.0.1%3A1&token=${"e".repeat(64)}&exp=${System.currentTimeMillis()+300000}")
    private suspend fun disconnectDuringTransition(at:Long) {
        val invitation=invitation()
        assertTrue(app.requestPairing(invitationUri(invitation),null))
        var prefs by mutableStateOf(app.preferences.read());var link by mutableStateOf(LinkState(status="Connecting…"))
        var home=0;var admitted=0;var scan:((String)->Unit)?=null
        compose.mainClock.autoAdvance=false
        content {PairScreen(prefs,link,{home++},{},{admitted++},cameraPreview={_,code->scan=code})}
        compose.waitForIdle()
        assertTrue(app.installBinding(invitation,"f".repeat(32)))
        compose.runOnIdle {prefs=app.prefs.value;link=LinkState(status="Connected",pairing="Paired with Twelfth",pairingPin=invitation.fingerprint)}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.mainClock.advanceTimeBy(at);compose.waitForIdle()
        compose.runOnIdle {assertEquals(0,home);link=LinkState(status="PC unreachable")}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.runOnIdle {assertEquals(1,home);requireNotNull(scan)(invitationUri(invitation))}
        assertEquals(0,admitted)
        compose.mainClock.advanceTimeBy(1000);compose.waitForIdle();assertEquals(1,home)
    }
    @Test(timeout=15000) fun earlyDisconnectEndsDurablePairingTransition()=runBlocking<Unit> {disconnectDuringTransition(100)}
    @Test(timeout=15000) fun lateDisconnectEndsDurablePairingTransition()=runBlocking<Unit> {disconnectDuringTransition(650)}
    @Test(timeout=15000) fun durableInstallNavigatesWithoutPendingOrSuccessMarkerObserved()=runBlocking<Unit> {
        var prefs by mutableStateOf(app.preferences.read());var home=0
        compose.mainClock.autoAdvance=false
        content {PairScreen(prefs,LinkState(status="PC unreachable"),{home++},{},{},cameraPreview={_,_->})}
        compose.waitForIdle()
        val invitation=invitation();assertTrue(app.requestPairing(invitationUri(invitation),null));assertTrue(app.installBinding(invitation,"f".repeat(32)))
        compose.runOnIdle {prefs=app.prefs.value}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle();assertEquals(1,home)
    }
    @Test(timeout=15000) fun cancelledSamePcAttemptDoesNotNavigateAndSuccessfulRepairDoes()=runBlocking<Unit> {
        val invitation=invitation();assertTrue(app.requestPairing(invitationUri(invitation),null));assertTrue(app.installBinding(invitation,"f".repeat(32)))
        var prefs by mutableStateOf(app.preferences.read());var home=0;var scan:((String)->Unit)?=null;var admitted=0
        compose.mainClock.autoAdvance=false
        content {PairScreen(prefs,LinkState(status="PC unreachable"),{home++},{},{admitted++},cameraPreview={_,code->scan=code})}
        compose.waitForIdle()
        assertTrue(app.requestPairing(invitationUri(invitation),invitation.fingerprint));val pending=app.preferences.read();compose.runOnIdle {prefs=pending}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        app.cancelPairing(invitation);val cancelled=app.preferences.read()
        compose.runOnIdle {prefs=cancelled};compose.mainClock.advanceTimeBy(1000);compose.waitForIdle()
        compose.runOnIdle {assertEquals(0,home);requireNotNull(scan)(invitationUri(invitation))};assertEquals(1,admitted)
        assertTrue(app.requestPairing(invitationUri(invitation),invitation.fingerprint));assertTrue(app.installBinding(invitation,"f".repeat(32)))
        compose.runOnIdle {prefs=app.prefs.value};compose.mainClock.advanceTimeByFrame();compose.waitForIdle();assertEquals(1,home)
    }

}
