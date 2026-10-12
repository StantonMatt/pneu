package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.protocol.Invitation
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.net.Socket

/** The host owns a real pneud and runs its CLI unpair only after this phone stops. */
@RunWith(AndroidJUnit4::class)
class TenthReviewDaemonDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
    @After fun cleanup()=runBlocking<Unit> {app.connection.stop();c.stopService(Intent(c,PneuService::class.java));app.forgetBinding();scope.cancel();app.connection.onUnauthorized=null}
    @Test(timeout=90000) fun realDaemonForgetsOfflinePhoneAndThreeUnauthorizedFramesBecomePairingLost()=runBlocking<Unit> {
        app.initialized.await();c.stopService(Intent(c,PneuService::class.java));app.connection.stop()
        assertNull(app.preferences.read().peer)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        app.preferences.toggle("wifi",true);app.preferences.toggle("tailscale",true);app.preferences.toggle("mobile",true)
        val args=InstrumentationRegistry.getArguments();val raw=requireNotNull(args.getString("pcUri"));val invitation=Invitation.parse(raw)
        assertTrue(app.requestPairing(raw,null));app.connection.start(scope)
        withTimeout(20000) {app.connection.state.first {it.pairingSuccess(app.prefs.value)}}
        val paired=app.preferences.read();assertEquals(invitation.fingerprint,paired.peer!!.pin)
        assertEquals(invitation.addresses.single().toString(),paired.lastLan);assertTrue(paired.lastConnected>0)
        app.connection.stop();delay(300)
        withContext(Dispatchers.IO) {
            Socket().use {socket->
                socket.connect(java.net.InetSocketAddress("10.0.2.2",requireNotNull(args.getString("controlPort")).toInt()),5000);socket.soTimeout=10000
                socket.getOutputStream().write("{\"device_id\":\"${paired.deviceId}\"}\n".toByteArray())
                assertEquals("{\"ok\":true}",socket.getInputStream().bufferedReader().readLine())
            }
        }
        assertNotNull(app.preferences.read().peer) // Offline phone hasn't received UNPAIR.
        val rejections=java.util.concurrent.atomic.AtomicInteger()
        app.connection.onUnauthorized={assertNotNull(app.prefs.value.peer);assertFalse(app.prefs.value.pairingLost);rejections.incrementAndGet()}
        app.connection.start(scope)
        val lost=withTimeout(50000) {app.preferences.flow.first {it.pairingLost && it.peer==null && !it.deliveryReset}}
        assertEquals(invitation.name,lost.lostPcName);assertEquals("Pairing lost",app.connection.state.value.status)
        assertEquals(3,rejections.get())
        i.sendStatus(0,android.os.Bundle().apply {putString("stream","Received exactly ${rejections.get()} pinned ERROR unauthorized frames; retained trust until the third\n")})
        app.preferences.toggle("setup",true)
        c.startActivity(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        val device=androidx.test.uiautomator.UiDevice.getInstance(i)
        assertTrue(device.wait(androidx.test.uiautomator.Until.hasObject(androidx.test.uiautomator.By.text("Pairing lost")),5000))
        assertTrue(device.hasObject(androidx.test.uiautomator.By.text("${invitation.name} doesn't recognize this phone")))
        assertNotEquals(paired.deviceId,lost.deviceId);assertNull(lost.pending)
    }
}
