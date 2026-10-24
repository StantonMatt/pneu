package io.github.stantonmatt.pneu

import android.content.Intent
import androidx.datastore.preferences.core.*
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

/** Deterministic transaction interleavings. No timing-dependent socket success
 * is needed to exercise stale HELLO, installation, loss and removal callbacks. */
@RunWith(AndroidJUnit4::class)
class PairingAttemptDeviceTest {
    private val c get()=InstrumentationRegistry.getInstrumentation().targetContext
    private val app get()=c.graph
    private val a="a".repeat(32)
    private val b="b".repeat(32)
    private fun raw(pin:Char='a',name:String="A")="pneu://pair?v=1&name=$name&fp=${pin.toString().repeat(64)}&addr=127.0.0.1%3A47861&token=${"e".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private suspend fun start(uri:String)=app.requestPairing(uri,ConfirmedBinding.forInvitation(app.preferences.read(),Invitation.parse(uri))).also {assertTrue(it)}
    private suspend fun bind(uri:String=raw(),id:String=a) {start(uri);assertTrue(app.installBinding(Invitation.parse(uri),id))}
    private suspend fun lost(id:String=a) {app.preferences.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).map {if(it.deviceId==id)it.copy(pairingLost=true) else it})}}
    @Before fun setup()=runBlocking<Unit> {DeviceFixture.reset();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.connection.awaitStopped()}
    @After fun cleanup()=runBlocking<Unit> {app.cancelPairing();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.connection.awaitStopped()}

    @Test fun targetRemovedBeforeOrAfterHelloCannotBeRestoredByDelayedSuccess()=runBlocking<Unit> {
        for(afterHello in listOf(false,true)) {
            bind();lost();start(raw('c'));val scan=app.connection.attempt.value!!
            val callback=if(afterHello)app.pairingHello(scan,a)!! else scan
            assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),a)))
            val resolved=if(afterHello)callback else app.pairingHello(callback,a)
            assertNull(resolved?.let {app.installSessionBinding(it,a,it.scanned.deviceId)})
            assertTrue(app.preferences.read().devices.isEmpty());assertNull(app.connection.attempt.value)
        }
    }
    @Test fun explicitUnpairOfAnAbsentTargetStillCancelsItsCapturedAttempt()=runBlocking<Unit> {
        for(afterHello in listOf(false,true)) {
            val uri=raw();start(uri);val scan=app.connection.attempt.value!!
            val callback=if(afterHello)app.pairingHello(scan,a)!! else scan
            assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),a)))
            val resolved=if(afterHello)callback else app.pairingHello(callback,a)
            assertNull(resolved?.let {app.installSessionBinding(it,a,it.scanned.deviceId)})
            assertNull(app.connection.attempt.value);assertTrue(app.preferences.read().devices.isEmpty())
        }
    }
    @Test fun oldConnectionLossAndRemovalCannotEraseANewerRepair()=runBlocking<Unit> {
        bind();val old=app.preferences.read();val peer=old.devices.single();val token=app.captureOutcome(a)
        start(raw());val attempt=app.connection.attempt.value!!
        assertFalse(app.loseBinding(peer,old.deviceId,token))
        assertFalse(app.forgetBinding(peer,old.deviceId,internalOutcome=token))
        assertEquals(attempt,app.connection.attempt.value);assertFalse(app.preferences.read().devices.single().pairingLost)
        assertTrue(app.installBinding(attempt.invitation,a))
    }
    @Test fun lossNeverCancelsAnAttemptAndUnrelatedRemovalDoesNotChangeItsFence()=runBlocking<Unit> {
        bind();bind(raw('b',"B"),b)
        val p=app.preferences.read();val peer=p.devices.first {it.deviceId==b}
        start(raw('c'));val scan=app.connection.attempt.value!!
        assertTrue(app.loseBinding(peer,p.deviceId,app.captureOutcome(b)))
        assertEquals(scan,app.connection.attempt.value)
        assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),b)))
        assertEquals(scan,app.connection.attempt.value)
        assertTrue(app.installBinding(scan.invitation,"c".repeat(32)))
        assertEquals(setOf(a,"c".repeat(32)),app.preferences.read().devices.map {it.deviceId}.toSet())
    }
    @Test fun identicalRescanRejectsEveryOldCallback()=runBlocking<Unit> {
        start(raw());val first=app.pairingHello(app.connection.attempt.value!!,a)!!
        assertTrue(app.cancelPairing(first));start(raw());val second=app.connection.attempt.value!!
        assertNotEquals(first.id,second.id)
        assertFalse(app.cancelPairing(first));assertFalse(app.rejectPairing(first,"old failure"))
        assertNull(app.pairingHello(first,a));assertNull(app.installSessionBinding(first,a,first.scanned.deviceId))
        assertEquals(second,app.connection.attempt.value);assertTrue(app.installBinding(second.invitation,a))
    }
    @Test fun cancellingEndedRejectionIsIdempotentAndCannotDismissANewerScan()=runBlocking<Unit> {
        val uri=raw();start(uri);val first=app.connection.attempt.value!!
        assertTrue(app.rejectPairing(first,"Can't reach your PC"));assertNull(app.connection.attempt.value)
        assertTrue(app.cancelPairing(null));assertEquals("",app.connection.state.value.pairing)
        start(uri);val second=app.connection.attempt.value!!
        assertFalse(app.cancelPairing(null));assertFalse(app.cancelPairing(first))
        assertEquals(second,app.connection.attempt.value)
    }
    @Test fun sameIdentityRepairPreservesGenerationSwitchesDeliveriesAndReceipts()=runBlocking<Unit> {
        bind();app.setPairSwitch(a,"sendClipboard",false);app.setPairSwitch(a,"automaticMedia",false)
        val before=app.preferences.read();val binding=DeliveryBinding.from(before,a)!!
        val row=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="file";automatic=false;recipients=listOf(binding)}
        assertTrue(app.enqueueFixture(row));app.queue.insertReceipt(Receipt().apply {peerId=a;transferId="receipt";hash="hash";state="applied"})
        lost();val uri=raw();start(uri);assertTrue(app.installBinding(Invitation.parse(uri),a))
        val peer=app.preferences.read().devices.single()
        assertEquals(binding.generation,peer.generation);assertFalse(peer.pairingLost)
        assertFalse(peer.clipboardSend);assertFalse(peer.automaticMedia)
        assertEquals(row.transferId,app.queue.delivery(row.mediaId,a).transferId);assertNotNull(app.queue.receipt(a,"receipt"))
        assertTrue(app.preferences.read().resetPeers.isEmpty())
    }
    @Test fun selfIdAndKnownCertificateWithAnotherIdPreserveAllExistingAuthority()=runBlocking<Unit> {
        bind();app.setPairSwitch(a,"sendClipboard",false)
        val row=DeviceFixture.text(app,"Keep queued","share");val before=app.preferences.read()
        for(id in listOf(before.deviceId,b)) {
            start(raw());val scan=app.connection.attempt.value!!
            assertNull(app.pairingHello(scan,id));assertNull(app.connection.attempt.value)
            assertEquals(before.devices,app.preferences.read().devices)
            assertNotNull(app.queue.find(row.mediaId));assertFalse(app.preferences.read().devices.single().clipboardSend)
        }
        assertThrows(IllegalArgumentException::class.java) {runBlocking {app.preferences.paired(Invitation.parse(raw('c')),before.deviceId)}}
        assertEquals(before.devices,app.preferences.read().devices)
    }
    @Test fun lostCertificateReplacementUsesExistingSlotAtCapacityAndHealthyOneRefuses()=runBlocking<Unit> {
        bind();app.preferences.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p)+(1 until 16).map {n->Peer("Other $n",n.toString(16).padStart(64,'0'),n.toString(16).padStart(32,'0'),emptyList(),generation=n+1L)})}
        app.preferences.setLong("pairingRevision",16);app.setPairSwitch(a,"sendClipboard",false)
        val before=app.preferences.read();start(raw('c'));val healthy=app.connection.attempt.value!!
        assertNull(app.pairingHello(healthy,a));assertNull(app.connection.attempt.value)
        assertEquals("Already paired as A. Unpair it first.",app.connection.state.value.pairing)
        assertEquals(before.devices,app.preferences.read().devices)
        lost();val uri=raw('c');start(uri);assertTrue(app.installBinding(Invitation.parse(uri),a))
        val after=app.preferences.read();assertEquals(16,after.devices.size)
        assertEquals(before.devices.filter {it.deviceId!=a},after.devices.filter {it.deviceId!=a})
        val replaced=after.devices.first {it.deviceId==a};assertEquals("c".repeat(64),replaced.pin);assertFalse(replaced.clipboardSend)
        assertNotEquals(before.devices.first {it.deviceId==a}.generation,replaced.generation)
    }
    @Test fun healthyConnectedRescanReportsAlreadyPairedAndRetainsQueuedFiles()=runBlocking<Unit> {
        bind();app.setPairSwitch(a,"sendClipboard",false)
        LocalPeerFixture(app).use {fixture->
            fixture.connect(a)
            val uri=fixture.invitation(a,"A")
            val before=app.preferences.read();val binding=DeliveryBinding.from(before,a)!!
            val row=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="file";automatic=false;recipients=listOf(binding)}
            assertTrue(app.enqueueFixture(row))
            start(uri)
            assertNull(app.connection.attempt.value)
            assertEquals("Already paired with A",app.connection.state.value.pairing)
            assertEquals(binding.generation,app.preferences.read().devices.single().generation)
            assertEquals(row.transferId,app.queue.delivery(row.mediaId,a).transferId)
            assertFalse(app.preferences.read().devices.single().clipboardSend)
            assertTrue(app.preferences.read().resetPeers.isEmpty())
            app.connection.stop();app.connection.awaitStopped()
        }
    }
    @Test fun activityAndServiceRecreationKeepTheSameInMemoryAttempt()=runBlocking<Unit> {
        app.preferences.toggle("setup",true);start(raw());val attempt=app.connection.attempt.value!!
        androidx.test.core.app.ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("pair",true)).use {scenario->
            scenario.recreate();assertEquals(attempt.id,app.connection.attempt.value?.id)
            DeviceFixture.stopPneuService();assertEquals(attempt.id,app.connection.attempt.value?.id)
            PneuService.start(c)
            withTimeout(5000) {while(app.connection.discoveryWatchCount==0 && app.connection.state.value.pairing.isEmpty())delay(20)}
            assertEquals(attempt.id,app.connection.attempt.value?.id)
            app.cancelPairing(attempt)
        }
    }
    @Test fun selfHelloOnRealTlsSessionNeverSendsPairRequestOrLosesExistingState()=runBlocking<Unit> {
        bind();app.setPairSwitch(a,"sendClipboard",false)
        val row=DeviceFixture.text(app,"Keep","share");val before=app.preferences.read()
        LocalPeerFixture(app).use {fixture->
            val uri=fixture.invitation(before.deviceId,"Self")
            start(uri);app.connection.start(app.scope)
            withTimeout(10000) {while(app.connection.attempt.value!=null)delay(20)}
            assertEquals(0,fixture.pairRequests.get());assertTrue(fixture.hellos.get()>0)
            assertEquals(before.devices,app.preferences.read().devices);assertNotNull(app.queue.find(row.mediaId))
            assertFalse(app.preferences.read().devices.single().pairingLost)
            app.connection.stop();app.connection.awaitStopped()
        }
    }
}
