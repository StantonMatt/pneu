package io.github.stantonmatt.pneu

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import java.io.IOException

@RunWith(AndroidJUnit4::class)
class DeviceFixtureDeviceTest {
    private val c=InstrumentationRegistry.getInstrumentation().targetContext
    private val app=c.graph
    @Before fun setup()=runBlocking<Unit> {DeviceFixture.reset()}
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.reset()}

    @Test(timeout=20000) fun resetClearsPersistedPolicyQueueAndFailureHooks()=runBlocking<Unit> {
        val original=app.preferences.read()
        app.preferences.toggle("tailscale",false)
        app.preferences.toggle("mobile",false)
        app.preferences.toggle("shareSheet",false)
        app.preferences.toggle("photos",false)
        app.preferences.toggle("screenshots",false)
        app.preferences.toggle("paused",true)
        app.preferences.toggle("setup",true)
        app.preferences.toggle("localCapacity",true)
        val path=File(c.filesDir,"outgoing/fixture-reset.bin").apply {parentFile!!.mkdirs();writeBytes(byteArrayOf(1))}
        DeviceFixture.insert(app,Outgoing().apply {mediaId="fixture-reset";peerId="fixture-reset-peer";this.path=path.path})
        app.queue.rememberOutgoing(1234,5678)
        app.database.openHelper.writableDatabase.execSQL("CREATE TRIGGER fail_fixture_reset BEFORE DELETE ON artifacts BEGIN SELECT RAISE(ABORT,'Synthetic reset failure'); END")
        app.preferences.beforeMutation={throw IOException("Synthetic leftover fault")}
        DeviceFixture.reset()
        val current=app.preferences.read()
        assertEquals(original.deviceId,current.deviceId);assertEquals(original.tlsAlias,current.tlsAlias)
        assertEquals(Prefs(deviceId=current.deviceId,tlsAlias=current.tlsAlias),current)
        assertTrue(app.queue.all().isEmpty());assertEquals(0L,app.queue.latestTextTime());assertFalse(path.exists())
    }

    @Test(timeout=20000) fun resetForPreparedPeerRetainsExactExportedIdentity()=runBlocking<Unit> {
        val original=app.preferences.read()
        val leaf=TlsIdentity(original.tlsAlias).certificate.encoded
        File(c.filesDir,"client.der").writeBytes(leaf)
        app.preferences.toggle("tailscale",false);app.preferences.toggle("mobile",false)
        DeviceFixture.reset(preserveIdentity=true)
        val current=app.preferences.read()
        assertEquals(original.deviceId,current.deviceId);assertEquals(original.tlsAlias,current.tlsAlias)
        assertArrayEquals(leaf,TlsIdentity(current.tlsAlias).certificate.encoded)
        assertTrue(current.wifi);assertTrue(current.tailscale);assertTrue(current.mobile)
    }

    @Test(timeout=20000) fun onlyDisablingTheLastTransportIsRejectedIncludingConcurrentEdits()=runBlocking<Unit> {
        app.preferences.toggle("tailscale",false) // First of two is allowed.
        try {app.preferences.toggle("wifi",false);fail("Disabled last transport")}
        catch(_:IllegalArgumentException) {}
        assertTrue(app.preferences.read().wifi);assertFalse(app.preferences.read().tailscale)
        app.preferences.toggle("tailscale",true) // Enable destination before disabling source.
        app.preferences.toggle("wifi",false)
        try {app.preferences.toggle("tailscale",false);fail("Disabled last transport")}
        catch(_:IllegalArgumentException) {}
        assertFalse(app.preferences.read().wifi);assertTrue(app.preferences.read().tailscale)
        app.preferences.toggle("wifi",true)
        val edits=awaitAll(async {runCatching {app.preferences.toggle("wifi",false)}},async {runCatching {app.preferences.toggle("tailscale",false)}})
        assertEquals(1,edits.count {it.isSuccess})
        val current=app.preferences.read();assertTrue(current.wifi xor current.tailscale)
    }

    private val first="c".repeat(32)
    private val second="d".repeat(32)
    private suspend fun pair(id:String,pin:Char):DeliveryBinding {
        val raw="pneu://pair?v=1&name=PC-$pin&fp=${pin.toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,app.preferences.read().peer?.pin))
        assertTrue(app.installBinding(Invitation.parse(raw),id))
        return DeliveryBinding.from(app.preferences.read(),id)!!
    }
    @Test fun pairingSecondComputerAddsPeerAndPreservesFirstQueueAndPhoneIdentity()=runBlocking<Unit> {
        val original=pair(first,'a');val before=app.preferences.read()
        val row=DeviceFixture.text(app,"First computer only","share",original)
        val added=pair(second,'e');val after=app.preferences.read()
        assertEquals(listOf(first,second),after.devices.map {it.deviceId})
        assertEquals(before.devices.single(),after.devices.first())
        assertEquals(before.deviceId,after.deviceId);assertEquals(before.tlsAlias,after.tlsAlias)
        assertTrue(original.matches(after));assertTrue(added.matches(after))
        assertTrue(after.resetPeers.isEmpty())
        assertEquals(first,app.queue.delivery(row.mediaId,first)!!.peerId)
        assertNull(app.queue.delivery(row.mediaId,second)) // Existing work never expands its recipient set.
    }
    @Test fun recipientUpdatesAndRemovalPreserveOtherDeliveryAndSharedBytes()=runBlocking<Unit> {
        val a=pair(first,'a');val b=pair(second,'e')
        val path=File(app.context.filesDir,"outgoing/${randomId()}.bin").apply {parentFile!!.mkdirs();writeText("shared snapshot")}
        val row=Outgoing().apply {
            mediaId=randomId();transferId=randomId();automatic=false;kind="file"
            this.path=path.path;size=path.length();hash=sha256(path.readBytes());recipients=listOf(a,b)
        }
        assertTrue(app.enqueue(row,a))
        val one=app.queue.delivery(row.mediaId,first)!!;val two=app.queue.delivery(row.mediaId,second)!!
        assertEquals(one.transferId,two.transferId);assertEquals(one.path,two.path)
        val db=app.database.openHelper.readableDatabase
        db.query("SELECT COUNT(*) FROM artifacts WHERE mediaId=?",arrayOf(row.mediaId)).use {it.moveToFirst();assertEquals(1,it.getInt(0))}
        one.state="failed";one.error="fixture-error";assertTrue(app.updateDelivery(one))
        val unchanged=app.queue.delivery(row.mediaId,second)!!
        assertEquals("waiting",unchanged.state);assertEquals("",unchanged.error)
        assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),first)))
        assertNull(app.queue.delivery(row.mediaId,first));assertNotNull(app.queue.delivery(row.mediaId,second))
        assertTrue(path.exists());assertTrue(b.matches(app.preferences.read()))
        assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),second)))
        assertNull(app.queue.find(row.mediaId));assertFalse(path.exists())
    }
    @Test fun fixtureResetRemovesAllPeersAndPerRecipientState()=runBlocking<Unit> {
        pair(first,'a');pair(second,'e')
        val identity=app.preferences.read()
        DeviceFixture.reset()
        val clean=app.preferences.read()
        assertTrue(clean.devices.isEmpty());assertTrue(clean.resetPeers.isEmpty())
        assertEquals(identity.deviceId,clean.deviceId);assertEquals(identity.tlsAlias,clean.tlsAlias)
        assertTrue(app.queue.all().isEmpty())
    }
}
