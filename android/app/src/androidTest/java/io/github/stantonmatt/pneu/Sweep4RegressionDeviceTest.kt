package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Application
import androidx.activity.ComponentActivity
import androidx.datastore.preferences.core.edit
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class Sweep4RegressionDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val pc="a".repeat(32)
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.CAMERA,Manifest.permission.POST_NOTIFICATIONS))
            i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true);app.preferences.toggle("screenshots",false)
    }
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.reset()}
    private suspend fun bind() {
        val raw="pneu://pair?v=1&name=Sweep4&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"c".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(raw),pc))
        withTimeout(5000) {app.prefs.first {it.devices.any {peer->peer.deviceId==pc}}}
    }
    private fun text(id:String,peer:String=pc)=Outgoing().apply {
        mediaId=id;transferId=randomId();peerId=peer;kind="text";state="waiting";automatic=false
        bindingGeneration=app.prefs.value.devices.firstOrNull {it.deviceId==peer}?.generation ?: 1
    }
    private suspend fun disable()=withContext(Dispatchers.Main) {
        val authority=CapturedAuthority(requireNotNull(DeliveryBinding.from(app.prefs.value,pc)))
        DevicesViewModel(c.applicationContext as Application).setPairSwitch(authority,"sendClipboard",false)
    }
    @Test(timeout=30000) fun devicePageDisablesClipboardOnMainAndSupersedesOnlyItsText()=runBlocking<Unit> {
        bind();app.queue.insert(text("target"));app.queue.insert(text("other","b".repeat(32)))
        app.queue.insert(text("file").apply {kind="file"})
        compose.setContent {PneuScreen(DEVICE+pc)}
        compose.onNodeWithText("Send clipboard").performScrollTo().performClick()
        withTimeout(5000) {while(app.preferences.read().devices.single().clipboardSend)delay(20)}
        assertEquals("superseded",app.queue.delivery("target",pc).state)
        assertEquals("waiting",app.queue.delivery("other","b".repeat(32)).state)
        assertEquals("waiting",app.queue.delivery("file",pc).state)
        compose.onNodeWithText(c.getString(R.string.ui_error_setting)).assertDoesNotExist()
    }
    @Test(timeout=30000) fun roomStillRejectsMainThreadQueries()=runBlocking<Unit> {
        bind()
        withContext(Dispatchers.Main) {
            try {app.queue.all();fail("Room main-thread guard must stay enabled")}
            catch(e:IllegalStateException) {assertTrue(e.message.orEmpty().contains("main thread"))}
        }
    }
    @Test(timeout=30000) fun queueFailureLeavesSwitchAndTextUnchangedThenRetrySucceeds()=runBlocking<Unit> {
        bind();app.queue.insert(text("target"))
        val db=app.database.openHelper.writableDatabase
        db.execSQL("CREATE TRIGGER fail_switch BEFORE UPDATE ON deliveries WHEN NEW.state='superseded' BEGIN SELECT RAISE(ABORT,'injected queue failure'); END")
        try {
            try {disable();fail("Queue failure must be reported")}catch(_:android.database.SQLException) {}
            assertTrue(app.preferences.read().devices.single().clipboardSend)
            assertEquals("waiting",app.queue.delivery("target",pc).state)
        }finally {db.execSQL("DROP TRIGGER fail_switch")}
        disable();assertFalse(app.preferences.read().devices.single().clipboardSend)
        assertEquals("superseded",app.queue.delivery("target",pc).state)
    }
    @Test(timeout=30000) fun preferenceFailureRollsBackQueueAndMainThreadRetrySucceeds()=runBlocking<Unit> {
        bind();app.queue.insert(text("target"))
        app.preferences.beforeMutation={if(it=="pairSwitch")throw java.io.IOException("Injected preference failure")}
        try {
            try {disable();fail("Preference failure must be reported")}catch(_:java.io.IOException) {}
            assertTrue(app.preferences.read().devices.single().clipboardSend)
            assertEquals("waiting",app.queue.delivery("target",pc).state)
        }finally {app.preferences.beforeMutation={}}
        disable();assertFalse(app.preferences.read().devices.single().clipboardSend)
        assertEquals("superseded",app.queue.delivery("target",pc).state)
    }
    @Test(timeout=30000) fun interruptedCrossStoreCommitReconcilesDisabledPeersText()=runBlocking<Unit> {
        bind();app.queue.insert(text("target"));app.preferences.setPairSwitch(pc,"sendClipboard",false)
        app.reconcileDelivery()
        assertEquals("superseded",app.queue.delivery("target",pc).state)
    }
    private suspend fun pairingFailure(peer:LocalPeerFixture,status:ConnectionStatus,wrongPin:Boolean=false) {
        peer.use {
            var raw=peer.invitation(pc,"Incompatible PC")
            if(wrongPin)raw=raw.replace(Regex("fp=[0-9a-f]+"),"fp=${"f".repeat(64)}")
            // The second endpoint must never replace an authenticated typed failure.
            raw=raw.replace("&token=","&addr=127.0.0.2%3A1&token=")
            assertTrue(withContext(Dispatchers.Main) {app.requestPairing(raw,ConfirmedBinding.from(app.prefs.value))})
            compose.setContent {PneuScreen("pair")}
            app.connection.start(app.scope)
            withTimeout(10000) {app.connection.state.first {it.pairingFeedback?.status==status}}
            assertNull(app.connection.attempt.value)
            assertTrue(app.preferences.read().devices.isEmpty())
            compose.onNodeWithText(UiPresentation.status(c,status)).assertIsDisplayed()
            compose.onNodeWithText(c.getString(R.string.pair_unreachable_title,"Incompatible PC")).assertDoesNotExist()
            delay(2500)
            assertEquals(status,app.connection.state.value.connectionStatus)
            assertEquals(status,app.connection.state.value.pairingFeedback!!.status)
            app.connection.stop();app.connection.awaitStopped()
            assertEquals(status,app.connection.state.value.connectionStatus)
        }
    }
    @Test(timeout=30000) fun incompatibleHelloEndsAttemptWithUpdateRequired()=runBlocking<Unit> {
        pairingFailure(LocalPeerFixture(app,helloVersion=2),ConnectionStatus.UPDATE_REQUIRED)
    }
    @Test(timeout=30000) fun unsupportedHelloCapabilitiesEndAttemptWithUpdateRequired()=runBlocking<Unit> {
        pairingFailure(LocalPeerFixture(app,capabilities=setOf("future_feature")),ConnectionStatus.UPDATE_REQUIRED)
    }
    @Test(timeout=30000) fun explicitVersionMismatchEndsAttemptWithUpdateRequired()=runBlocking<Unit> {
        pairingFailure(LocalPeerFixture(app,helloError="version_mismatch"),ConnectionStatus.UPDATE_REQUIRED)
    }
    @Test(timeout=30000) fun savedPeerTypedFailureSurvivesLaterUnreachableReconnect()=runBlocking<Unit> {
        bind()
        LocalPeerFixture(app,helloVersion=2).use {peer->
            val raw=peer.invitation(pc,"Saved PC")
            val invitation=Invitation.parse(raw)
            app.preferences.store.edit {p->
                PeerRegistry.write(p,PeerRegistry.read(p).map {it.copy(pin=invitation.fingerprint,addresses=invitation.addresses)})
            }
            withTimeout(5000) {app.prefs.first {it.devices.single().pin==invitation.fingerprint}}
            app.connection.start(app.scope)
            withTimeout(10000) {app.connection.devices.first {it[pc]?.connectionStatus==ConnectionStatus.UPDATE_REQUIRED}}
        }
        // A refused retry cannot erase a known incompatibility. A successful
        // compatible session or a binding change clears that failure.
        app.connection.retry()
        delay(500)
        withTimeout(5000) {app.connection.devices.first {it[pc]?.connectionStatus!=ConnectionStatus.CONNECTING}}
        assertEquals(ConnectionStatus.UPDATE_REQUIRED,app.connection.devices.value[pc]!!.connectionStatus)
        assertFalse(app.preferences.read().devices.single().pairingLost)
    }
    @Test(timeout=30000) fun wrongFingerprintEndsAttemptWithTypedFailure()=runBlocking<Unit> {
        pairingFailure(LocalPeerFixture(app),ConnectionStatus.FINGERPRINT_MISMATCH,wrongPin=true)
    }
}
