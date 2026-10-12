package io.github.stantonmatt.pneu

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.data.Prefs
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
        app.queue.insert(Outgoing().apply {mediaId="fixture-reset";this.path=path.path})
        app.queue.rememberOutgoing(1234,5678)
        app.database.openHelper.writableDatabase.execSQL("CREATE TRIGGER fail_fixture_reset BEFORE DELETE ON outgoing BEGIN SELECT RAISE(ABORT,'Synthetic reset failure'); END")
        app.preferences.beforeMutation={throw IOException("Synthetic leftover fault")}
        DeviceFixture.reset()
        val current=app.preferences.read()
        assertNotEquals(original.deviceId,current.deviceId)
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
}
