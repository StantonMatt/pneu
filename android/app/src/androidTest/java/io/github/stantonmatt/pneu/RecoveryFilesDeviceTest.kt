package io.github.stantonmatt.pneu

import android.Manifest
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

/** Both legacy and mesh peers send complete file/image frames before the pairing
 * result and finish another image after it. Recovery must never publish them. */
@RunWith(AndroidJUnit4::class)
class RecoveryFilesDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    @Before fun setup()=runBlocking<Unit> {DeviceFixture.reset();PeerFixture.preservePreparedIdentity(app)}
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.reset(preserveIdentity=false)}
    @Test(timeout=90000) fun lostResultAndRescanDrainBothFileKindsAcrossBindingCommit()=runBlocking<Unit> {
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val args=InstrumentationRegistry.getArguments();val pin=requireNotNull(args.getString("pcPin"));val port=requireNotNull(args.getString("pcPort"))
        val raw="pneu://pair?v=1&name=Recovery%20PC&fp=$pin&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        suspend fun paired(expectedNacks:Int) {
            withTimeout(25000) {
                while(!app.connection.connected("d".repeat(32)) || PeerFixture.evidence().count {it.asJsonObject["event"].asString=="PHONE_NACK"}<expectedNacks)delay(50)
            }
            assertNull(app.connection.attempt.value)
            assertEquals(0,app.queue.receivedFileCount())
            for(id in listOf("6","7","8"))assertNull(app.queue.receipt("d".repeat(32),id.repeat(32)))
            assertNull(app.connection.state.value.incoming)
        }
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        app.connection.start(app.scope);paired(3)
        val binding=DeliveryBinding.from(app.preferences.read(),"d".repeat(32))!!
        app.connection.stop();app.connection.awaitStopped()
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        app.connection.start(app.scope);paired(6)
        assertEquals(binding.generation,DeliveryBinding.from(app.preferences.read(),binding.pcId)!!.generation)
    }
}
