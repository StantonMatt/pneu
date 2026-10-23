package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Activity
import android.app.Instrumentation
import android.content.Intent
import android.content.IntentFilter
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.compose.runtime.*
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.StateRestorationTester
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.features.PermissionRequests
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

/** Host revokes permissions before instrumentation; revoking during a test kills its process. */
@RunWith(AndroidJUnit4::class)
class Sweep3PairingRestoreDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private fun stage(value:String) {i.sendStatus(0,android.os.Bundle().apply {putString("stream","Pairing restore: $value\n")})}
    @Before fun setup()=runBlocking<Unit> {
        stage("fixture reset");DeviceFixture.reset();stage("settings");app.preferences.toggle("setup",true);app.preferences.toggle("screenshots",false)
        stage("await settings");withTimeout(5000) {app.prefs.first {it.setup && !it.screenshots}}
        assertFalse(c.granted(Manifest.permission.ACCESS_LOCAL_NETWORK))
        PermissionRequests.mark(c,Manifest.permission.ACCESS_LOCAL_NETWORK)
    }
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.reset()}
    @Test(timeout=45000) fun restoredPairingPreviewNeedsFreshConfirmationAfterPermissionGrant()=runBlocking<Unit> {
        val raw="pneu://pair?v=1&name=Restore%20PC&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        var candidate by mutableStateOf<String?>(raw);var refresh by mutableIntStateOf(0)
        val restore=StateRestorationTester(compose)
        val filter=IntentFilter(Settings.ACTION_APPLICATION_DETAILS_SETTINGS).apply {addDataScheme("package")}
        val monitor=i.addMonitor(filter,Instrumentation.ActivityResult(Activity.RESULT_CANCELED,Intent()),true)
        try {
            stage("show preview");restore.setContent {PneuScreen(requested="home",refresh=refresh,candidate=candidate,offerPair={candidate=it})}
            compose.onNodeWithText("Pair with Restore PC?").assertIsDisplayed()
            stage("approve permission");compose.onNode(hasText("Open settings") and hasAnyAncestor(isDialog())).performClick()
            assertEquals(1,monitor.hits);assertNull(app.connection.attempt.value)
            // Same saved-state path used when Android recreates the process:
            // candidate/screen restore, but an accepted permission approval must not.
            stage("restore preview");restore.emulateSavedInstanceStateRestore()
            i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
            refresh++;compose.waitForIdle();delay(300)
            assertNull("A restored permission result cannot approve pairing",app.connection.attempt.value)
            compose.onNodeWithText("Pair with Restore PC?").assertIsDisplayed()
            stage("fresh Pair tap");compose.onNodeWithText("Pair").performClick()
            compose.waitUntil(5000) {app.connection.attempt.value!=null}
            stage("fresh approval accepted")
            assertNotNull(app.connection.attempt.value)
        }finally {i.removeMonitor(monitor)}
    }
}
