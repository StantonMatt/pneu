package io.github.stantonmatt.pneu

import android.Manifest
import android.content.ContentValues
import android.content.Intent
import android.graphics.Bitmap
import android.provider.MediaStore
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.protocol.Invitation
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.Test
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

/** Emulator smoke checks are independent of the concurrently built desktop daemon. */
@RunWith(AndroidJUnit4::class)
class CoreDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    private val instrumentation=InstrumentationRegistry.getInstrumentation()
    private val context=instrumentation.targetContext
    private fun screenshot(name: String) {
        val dir=File(context.filesDir,"screens").apply {mkdirs()}
        instrumentation.uiAutomation.takeScreenshot().let { bitmap -> File(dir,"$name.png").outputStream().use {bitmap.compress(Bitmap.CompressFormat.PNG,100,it)};bitmap.recycle() }
    }
    private fun launch() {context.startActivity(Intent(context,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK));Thread.sleep(1200)}
    private fun night(enabled:Boolean) {
        instrumentation.uiAutomation.executeShellCommand("cmd uimode night ${if(enabled)"yes" else "no"}").use {
            android.os.ParcelFileDescriptor.AutoCloseInputStream(it).readBytes()
        }
        androidx.test.uiautomator.UiDevice.getInstance(instrumentation).waitForIdle()
        instrumentation.waitForIdleSync()
    }
    private fun click(text: String) {
        val device=androidx.test.uiautomator.UiDevice.getInstance(instrumentation)
        device.waitForIdle();instrumentation.waitForIdleSync()
        val objectToClick=device.findObject(androidx.test.uiautomator.UiSelector().text(text))
        assertTrue("Control $text",objectToClick.waitForExists(3000)); objectToClick.click();Thread.sleep(700)
    }
    @Test fun onboardingPairHomeAndPersistentScreenshotQueue() = runBlocking<Unit> {
        val automation=instrumentation.uiAutomation
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.CAMERA)) automation.grantRuntimePermission(context.packageName,permission)
        context.stopService(Intent(context,PneuService::class.java));context.graph.connection.stop()
        context.graph.preferences.forget();context.graph.preferences.toggle("setup",false);context.graph.queue.clear()
        context.graph.preferences.toggle("queue",true);context.graph.preferences.toggle("screenshots",true)
        context.graph.preferences.toggle("photos",false);context.graph.preferences.toggle("recordings",false)
        context.graph.preferences.toggle("mobile",true)
        night(false)
        launch();screenshot("setup-light");night(true);Thread.sleep(900);screenshot("setup-dark");night(false);Thread.sleep(900);click("Continue");withTimeout(5000) {context.graph.prefs.first {it.setup}};screenshot("pair-light")
        night(true);Thread.sleep(1000);screenshot("pair-dark")
        // Export a public leaf for the orchestrator's independent TLS harness.
        val cert=TlsIdentity(context.graph.preferences.flow.first().tlsAlias).certificate
        File(context.filesDir,"client.der").writeBytes(cert.encoded)
        val invitation=Invitation.parse("pneu://pair?v=1&name=Test%20PC&fp=${"a".repeat(64)}&addr=10.0.2.2%3A47826&token=${"b".repeat(64)}&exp=1800000300000")
        context.graph.preferences.paired(invitation,"c".repeat(32));context.graph.preferences.setLong("mediaSince",System.currentTimeMillis()/1000-2)
        DeviceFixture.connectLocalPeer(context.graph)
        context.startActivity(Intent(context,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK));Thread.sleep(1500)
        screenshot("home-dark");clickSettings();screenshot("settings-dark");androidx.test.uiautomator.UiDevice.getInstance(instrumentation).pressBack();Thread.sleep(400)
        night(false);Thread.sleep(1200);screenshot("home-light");clickSettings();screenshot("settings-light")
        // Close the UI first: observation must belong to the service, not the activity.
        androidx.test.uiautomator.UiDevice.getInstance(instrumentation).pressHome();Thread.sleep(500)
        instrumentation.runOnMainSync {
            androidx.test.runner.lifecycle.ActivityLifecycleMonitorRegistry.getInstance()
                .getActivitiesInStage(androidx.test.runner.lifecycle.Stage.STOPPED).filterIsInstance<MainActivity>()
                .forEach { it.finishAndRemoveTask() }
        }
        Thread.sleep(300)
        val values=ContentValues().apply {
            put(MediaStore.Images.Media.DISPLAY_NAME,"Screenshot_Pneu_${System.currentTimeMillis()}.png")
            put(MediaStore.Images.Media.MIME_TYPE,"image/png");put(MediaStore.Images.Media.RELATIVE_PATH,"Pictures/Screenshots/")
            put(MediaStore.Images.Media.IS_PENDING,1)
        }
        val uri=context.contentResolver.insert(MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY),values)!!
        val image=Bitmap.createBitmap(32,32,Bitmap.Config.ARGB_8888).apply {eraseColor(android.graphics.Color.rgb(15,110,104))}
        context.contentResolver.openOutputStream(uri)!!.use {image.compress(Bitmap.CompressFormat.PNG,100,it)};image.recycle()
        // Pending images must not be enqueued before their bytes are complete.
        delay(500)
        val id="external_primary:${MediaStore.getVersion(context,"external_primary")}:images:${android.content.ContentUris.parseId(uri)}"
        assertNull(context.graph.queue.find(id))
        context.contentResolver.update(uri,ContentValues().apply {put(MediaStore.Images.Media.IS_PENDING,0)},null,null)
        // The shared image budget may queue pre-send preparation. Optimization
        // may replace the transfer ID once; immutable bytes/ID start afterward.
        val row=withTimeout(8000) {
            while(true) {
                val current=context.graph.queue.find(id)
                if(current!=null && !io.github.stantonmatt.pneu.features.DeliveryPolicy.optimize(current.kind,current.mime,
                    context.graph.connection.cellular(),context.graph.preferences.read().originals,
                    current.attempted || current.payloadAttempted,current.optimized))break
                delay(20)
            }
            context.graph.queue.find(id)!!
        }
        assertEquals("waiting",row.state);assertTrue(File(row.path).exists());assertEquals(64,row.hash.length)
        delay(600);assertEquals(row.transferId,context.graph.queue.find(id)!!.transferId)
        withContext(Dispatchers.Main) {context.graph.retry(id)}
        val selected=io.github.stantonmatt.pneu.data.ConfirmedQueue.from(context.graph.preferences.read(),context.graph.queue.pending())
        withContext(Dispatchers.Main) {assertTrue(context.graph.discardAll(selected))}
        assertEquals("discarded",context.graph.queue.find(id)!!.state)
        // UI callers run on Main: unpair must dispatch blocking Room/file cleanup to IO.
        launch();DeviceFixture.openDevice(instrumentation,context.graph.prefs.value.peer!!.name)
        val device=androidx.test.uiautomator.UiDevice.getInstance(instrumentation)
        assertTrue(device.findObject(androidx.test.uiautomator.UiSelector().text("Unpair")).waitForExists(4000))
        click("Unpair");click("Unpair")
        withTimeout(3000) { while(context.graph.prefs.value.peer != null) delay(50) }
        delay(1200)
        assertTrue(device.findObject(androidx.test.uiautomator.UiSelector().text("Scan the QR code in Pneu on your computer")).exists())
        assertTrue(context.graph.queue.pending().isEmpty())
        assertNull(context.graph.queue.find(id))
        assertFalse(File(row.path).exists())
        context.stopService(Intent(context,PneuService::class.java))
        context.contentResolver.delete(uri,null,null)
    }
    private fun clickSettings() {
        val device=androidx.test.uiautomator.UiDevice.getInstance(instrumentation)
        val button=device.findObject(androidx.test.uiautomator.UiSelector().description("Settings"))
        assertTrue(button.waitForExists(3000));button.click();Thread.sleep(600)
    }
}
