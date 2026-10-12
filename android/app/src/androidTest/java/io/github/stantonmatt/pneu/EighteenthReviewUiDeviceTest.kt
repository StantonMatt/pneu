package io.github.stantonmatt.pneu

import android.content.Intent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.protocol.Invitation
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

/** Real lost binding (peer cleared), Home and installed SystemUI tile recovery. */
@RunWith(AndroidJUnit4::class)
class EighteenthReviewUiDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation()
    private val c=i.targetContext
    private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private fun shell(command:String) {i.uiAutomation.executeShellCommand(command).use {android.os.ParcelFileDescriptor.AutoCloseInputStream(it).readBytes()}}
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome()
        app.preferences.toggle("setup",true);app.preferences.toggle("screenshots",false)
        val raw="pneu://pair?v=1&name=Lost%20PC&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"c".repeat(32)))
        assertTrue(app.loseBinding(app.preferences.read().peer))
        app.preferences.toggle("paused",true)
        withTimeout(5000) {app.prefs.first {it.pairingLost && it.peer==null && !it.deliveryReset && it.paused}}
    }
    @After fun cleanup()=runBlocking<Unit> {
        shell("cmd statusbar collapse")
        for(tile in listOf("PneuTile","ClipboardTile"))shell("cmd statusbar remove-tile io.github.stantonmatt.pneu/.features.$tile")
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
    }
    @Test(timeout=45000) fun bothLostPairingTilesOpenHomeRecoveryEvenWhenPausedAndPeerIsGone()=runBlocking<Unit> {
        for(tile in listOf("PneuTile","ClipboardTile"))shell("cmd statusbar add-tile io.github.stantonmatt.pneu/.features.$tile")
        for(tile in listOf("PneuTile","ClipboardTile")) {
            device.pressHome();device.openQuickSettings();delay(1200)
            shell("cmd statusbar click-tile io.github.stantonmatt.pneu/.features.$tile")
            assertTrue(device.wait(Until.hasObject(By.text("Pairing lost")),5000))
            assertTrue(device.wait(Until.hasObject(By.text("Pair again")),5000))
            assertTrue(device.hasObject(By.text("Lost PC doesn't recognize this phone")))
            assertTrue(app.preferences.read().paused);assertTrue(app.queue.all().isEmpty())
        }
    }
}
