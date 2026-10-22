package io.github.stantonmatt.pneu

import android.app.Application
import android.content.Intent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class TwelfthReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val lan=Endpoint.parse("192.0.2.1:47821")
    private val tailscale=Endpoint.parse("100.67.144.28:47821")
    private fun uri(pin:String,endpoint:Endpoint=lan)="pneu://pair?v=1&name=Twelfth&fp=$pin&addr=${endpoint.toString().replace(":","%3A")}&token=${"e".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private suspend fun install(pin:String,pcId:String,endpoint:Endpoint=lan):Prefs {
        val raw=uri(pin,endpoint);assertTrue(app.requestPairing(raw,app.preferences.read().peer?.pin))
        assertTrue(DeviceFixture.installApprovedBinding(app,Invitation.parse(raw),pcId));return app.preferences.read()
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        UiDevice.getInstance(i).pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(200);app.forgetBinding()
        app.preferences.toggle("wifi",true);app.preferences.toggle("tailscale",true)
        install("a".repeat(64),"d".repeat(32))
    }
    @After fun cleanup()=runBlocking<Unit> {c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()}
    private suspend fun seed():DeliveryBinding {
        val binding=DeliveryBinding.from(app.preferences.read())!!
        assertTrue(app.preferences.connected(binding,lan,1234));app.preferences.setString("lastReceivedHash","received");app.preferences.setString("lastSentHash","sent")
        return binding
    }
    @Test(timeout=15000) fun addingTailscalePeerPreservesFirstPcCacheAndSessionAuthority()=runBlocking<Unit> {
        val old=seed();val before=app.preferences.read()
        val next=install("b".repeat(64),"c".repeat(32),tailscale)
        assertEquals(2,next.devices.size);assertEquals(before.peer,next.devices.first())
        assertTrue(next.resetPeers.isEmpty());assertTrue(old.matches(next))
        val second=DeliveryBinding.from(next,"c".repeat(32))!!
        assertTrue(app.preferences.connected(second,tailscale,5678))
        assertTrue(app.preferences.connected(old,lan,9999))
        val ready=app.preferences.read()
        assertEquals(lan.toString(),ready.forPeer(old.pcId).lastLan)
        assertEquals(9999L,ready.forPeer(old.pcId).lastConnected)
        assertEquals("",ready.forPeer(second.pcId).lastLan)
        assertEquals(5678L,ready.forPeer(second.pcId).lastConnected)
        assertEquals(before.lastReceivedHash,ready.lastReceivedHash)
        assertEquals(before.lastSentHash,ready.lastSentHash)
        assertEquals(listOf(tailscale),app.connection.candidates(ready.forPeer(second.pcId)))
    }
    @Test(timeout=15000) fun sameBindingRepairPreservesCacheGenerationAndDeliveryAuthority()=runBlocking<Unit> {
        val old=seed();val before=app.preferences.read();val after=install(before.peer!!.pin,before.peer.deviceId,tailscale)
        assertEquals(1,after.devices.size);assertEquals(before.pairingRevision,after.pairingRevision)
        assertEquals(before.lastLan,after.lastLan);assertEquals(before.lastConnected,after.lastConnected)
        assertTrue(old.matches(after));assertTrue(app.preferences.connected(old,lan,9999))
        assertEquals(before.lastReceivedHash,after.lastReceivedHash);assertEquals(before.lastSentHash,after.lastSentHash)
    }
    @Test(timeout=15000) fun knownCertificateWithDifferentIdPreservesCacheUntilExplicitForget()=runBlocking<Unit> {
        val old=seed();val before=app.preferences.read();val raw=uri(old.pin,tailscale)
        assertTrue(app.requestPairing(raw,old.pin))
        assertFalse(app.installBinding(Invitation.parse(raw),"f".repeat(32)))
        val next=app.preferences.read();assertEquals(before.devices,next.devices)
        assertEquals(before.lastLan,next.lastLan);assertEquals(before.lastConnected,next.lastConnected)
        assertNull(app.connection.attempt.value);assertTrue(old.matches(next))
        app.forgetBinding();assertFalse(app.preferences.connected(old,lan,9999))
        val forgotten=app.preferences.read();assertEquals("",forgotten.lastLan);assertEquals(0L,forgotten.lastConnected)
    }
    @Test(timeout=15000) fun clipboardRecipientIsRetainedBeforeFocusAndOldInternalActionsFailClosed()=runBlocking<Unit> {
        val before=app.preferences.read();val original=DeliveryBinding.from(before)!!
        val intent=ManualEntries.clipboardIntent(c,before)
        val operation=ManualEntryViewModel(c.applicationContext as Application)
        operation.captureClipboardBinding(CompletableDeferred(original))
        val next=install("b".repeat(64),before.peer!!.deviceId,tailscale)
        operation.captureClipboardBinding(CompletableDeferred(DeliveryBinding.from(next))) // Recreation before focus.
        assertEquals(original,operation.clipboardBinding!!.await());assertFalse(operation.clipboardBinding!!.await()!!.matches(next))
        assertEquals(original,ManualEntries.admission(intent,next));assertNotEquals(intent.identifier,ManualEntries.clipboardIntent(c,next).identifier)
        val unpaired=ManualEntryViewModel(c.applicationContext as Application);unpaired.captureClipboardBinding(CompletableDeferred<DeliveryBinding?>().apply {complete(null)});unpaired.captureClipboardBinding(CompletableDeferred(DeliveryBinding.from(next)));assertNull(unpaired.clipboardBinding!!.await())
    }
}
