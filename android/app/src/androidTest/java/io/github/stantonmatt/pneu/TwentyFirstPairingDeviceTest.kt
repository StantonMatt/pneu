package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import android.net.Uri
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.protocol.Invitation
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class TwentyFirstPairingDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    @Before fun reset()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true);app.preferences.toggle("screenshots",false)
    }
    @After fun cleanup()=runBlocking<Unit> {
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.connection.awaitStopped();app.forgetBinding()
    }
    private fun visible(text:String)=compose.onAllNodesWithText(text).fetchSemanticsNodes().isNotEmpty()
    @Test(timeout=60000) fun rejectedDeepLinkRetainsInvitationAndRetryRequiresFreshConfirmation()=runBlocking<Unit> {
        val args=InstrumentationRegistry.getArguments()
        val raw="pneu://pair?v=1&name=Retry%20PC&fp=${requireNotNull(args.getString("pcPin"))}&addr=10.0.2.2%3A${requireNotNull(args.getString("pcPort"))}&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        val invitation=Invitation.parse(raw)
        ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).setAction(Intent.ACTION_VIEW).setData(Uri.parse(raw))).use {scenario->
            compose.waitUntil(10000) {visible("Pair with Retry PC?")}
            compose.onNodeWithText("Pair").performClick()
            compose.waitUntil(20000) {visible("Can't reach your PC") && visible("Try again")}
            assertNull(app.preferences.read().pending);assertNull(app.preferences.read().peer)
            assertEquals(1,PeerFixture.evidence().count {it.asJsonObject["event"].asString=="PAIR_REJECTED"})
            scenario.recreate()
            compose.waitUntil(10000) {visible("Can't reach your PC") && visible("Try again")}
            compose.onNodeWithText("Try again").performClick()
            compose.waitUntil(5000) {visible("Pair with Retry PC?")}
            // Re-presenting the invitation is not permission to send another request.
            delay(300);assertNull(app.preferences.read().pending)
            assertEquals(1,PeerFixture.evidence().count {it.asJsonObject["event"].asString=="PAIR_REJECTED"})
            assertEquals(0,PeerFixture.evidence().count {it.asJsonObject["event"].asString=="PAIR"})
            compose.onNodeWithText("Pair").performClick()
            compose.waitUntil(5000) {app.prefs.value.pending==invitation || app.prefs.value.peer?.pin==invitation.fingerprint}
            withTimeout(20000) {while(app.preferences.read().peer?.pin!=invitation.fingerprint || !app.connection.connected)delay(50)}
            assertNull(app.preferences.read().pending)
            assertEquals(1,PeerFixture.evidence().count {it.asJsonObject["event"].asString=="PAIR"})
        }
    }
}
