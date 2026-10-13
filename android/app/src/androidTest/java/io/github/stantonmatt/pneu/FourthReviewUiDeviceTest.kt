package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.features.PermissionRequests
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import java.io.File
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith

/** Host sets user-fixed denial before instrumentation (revocation can kill the test process). */
@RunWith(AndroidJUnit4::class)
class FourthReviewUiDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph;private val device=UiDevice.getInstance(i)
    private fun launch(extra:String) {
        c.startActivity(Intent(c,MainActivity::class.java).putExtra(extra,true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        val title=when(extra) {"home"->"Pneu";"pair"->"Pair with PC";else->"Permissions and battery"}
        assertTrue("Pneu $extra screen must appear",device.wait(Until.hasObject(By.pkg(c.packageName).text(title)),5000))
        device.waitForIdle(1000)
    }
    private fun visible(text:String)=device.findObject(UiSelector().packageName(c.packageName).text(text)).waitForExists(4000)
    private fun clickPneu(selector:BySelector,label:String) {
        var target=requireNotNull(device.wait(Until.findObject(selector.pkg(c.packageName)),5000)) {"Pneu $label must appear"}
        // Compose may expose the icon/text separately from its clickable parent.
        while(!target.isClickable)target=requireNotNull(target.parent) {"Pneu $label must have a clickable control"}
        device.waitForIdle(1000);target.click()
    }
    private fun settingsRow(title:String) {
        val scroll=UiScrollable(UiSelector().packageName(c.packageName).scrollable(true))
        assertTrue("Settings row '$title' must be reachable",scroll.scrollTextIntoView(title))
        assertTrue("Settings row '$title' must be visible",visible(title))
    }
    private fun rowSettings(title:String) {
        val target=device.findObject(UiSelector().packageName(c.packageName).text(title));assertTrue("Permission row '$title' must be visible",target.waitForExists(4000))
        val y=target.bounds.centerY()
        device.findObjects(By.pkg(c.packageName).text("Settings")).minBy {kotlin.math.abs(it.visibleBounds.centerY()-y)}.click()
    }
    private fun grant(permission:String)=i.uiAutomation.grantRuntimePermission(c.packageName,permission)
    @Test(timeout=90000) fun permanentDenialsRetainConfirmedPairingAndGenericKillCanBeAcknowledged()=runBlocking<Unit> {
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding()
        val denied=listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.CAMERA,Manifest.permission.POST_NOTIFICATIONS,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO)
        assertTrue(denied.none {c.granted(it)});PermissionRequests.mark(c,*denied.toTypedArray())
        app.preferences.toggle("setup",true);app.preferences.toggle("tailscale",true);app.preferences.toggle("wifi",false);app.preferences.toggle("photos",false);app.preferences.toggle("recordings",false)
        val old="pneu://pair?v=1&name=Trusted%20PC&fp=${"a".repeat(64)}&addr=10.0.2.2%3A47861&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(old,null));assertTrue(app.installBinding(Invitation.parse(old),"d".repeat(32)))
        // Even app-owned catalog access is filtered when the full grant is absent.
        app.preferences.toggle("paused",true);val paused=app.preferences.read()
        try {app.preferences.toggle("paused",false);fail("Permission-filtered Resume was saved")}catch(_:java.io.IOException) {}
        assertTrue(app.preferences.read().paused);assertEquals(paused.imageGeneration,app.preferences.read().imageGeneration)
        val code=old.replace("Trusted%20PC","New%20PC").replace("a".repeat(64),"c".repeat(64))
        try {
            c.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(code),c,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
            assertTrue(visible("Replace Trusted PC with New PC?"));assertNull(app.preferences.flow.first().pending)
            device.findObject(UiSelector().text("Open settings")).click()
            assertTrue(device.wait(Until.hasObject(By.pkg("com.android.settings")),4000));assertNull(app.preferences.flow.first().pending)
            grant(Manifest.permission.ACCESS_LOCAL_NETWORK);device.pressBack()
            withTimeout(7000) {while(app.preferences.flow.first().pending!=Invitation.parse(code))delay(30)}
            assertEquals("Trusted PC",app.preferences.flow.first().peer!!.name)
            app.cancelPairing();app.prefs.first {it.pending==null};app.connection.state.update {it.copy(pairing="")};launch("pair")
            assertTrue(visible("Camera access is off"));assertTrue(visible("Open settings"))
            device.findObject(UiSelector().text("Open settings")).click();assertTrue(device.wait(Until.hasObject(By.pkg("com.android.settings")),4000));device.pressBack()
            // Photos request only images, and a grant from Settings completes the intended toggle.
            launch("home");clickPneu(By.desc("Settings"),"Home Settings action")
            assertTrue("Pneu Settings screen must open",device.wait(Until.hasObject(By.pkg(c.packageName).text("Settings")),5000))
            settingsRow("Camera photos");clickPneu(By.text("Camera photos"),"Camera photos row")
            assertTrue(device.wait(Until.hasObject(By.pkg("com.android.settings")),4000));grant(Manifest.permission.READ_MEDIA_IMAGES);device.pressBack()
            assertTrue("Images grant",c.granted(Manifest.permission.READ_MEDIA_IMAGES));withTimeoutOrNull(5000) {app.prefs.first {it.photos}} ?: fail("Photos toggle did not resume after settings")
            assertFalse(c.granted(Manifest.permission.READ_MEDIA_VIDEO))
            val resumeGeneration=android.provider.MediaStore.getGeneration(c,"external_primary")
            app.preferences.toggle("paused",false);assertFalse(app.preferences.read().paused)
            assertTrue(app.preferences.read().imageFloor>resumeGeneration)
            settingsRow("Screen recordings");clickPneu(By.text("Screen recordings"),"Screen recordings row")
            assertTrue(device.wait(Until.hasObject(By.pkg("com.android.settings")),4000));grant(Manifest.permission.READ_MEDIA_VIDEO);device.pressBack()
            assertTrue("Video grant",c.granted(Manifest.permission.READ_MEDIA_VIDEO));withTimeoutOrNull(5000) {app.prefs.first {it.recordings}} ?: fail("Recording toggle did not resume after settings")
            launch("permissions");rowSettings("Notifications")
            assertTrue(device.wait(Until.hasObject(By.pkg("com.android.settings")),4000));grant(Manifest.permission.POST_NOTIFICATIONS);device.pressBack()
            // Camera stays on Pair; generic permissions UI can clear system-kill evidence.
            c.getSharedPreferences("service_evidence",Context.MODE_PRIVATE).edit().putBoolean("running",false).putLong("kill",System.currentTimeMillis()+1000).putLong("acknowledged",0).commit()
            assertTrue(ServiceEvidence.killed(c));launch("permissions")
            assertTrue(visible("Local network"));assertFalse(device.findObject(UiSelector().text("Camera")).exists());assertTrue(visible("Notifications"))
            val scroll=device.findObject(UiSelector().packageName(c.packageName).scrollable(true));if(scroll.exists())UiScrollable(UiSelector().packageName(c.packageName).scrollable(true)).scrollTextIntoView("Dismiss")
            assertTrue("Kill evidence must remain visible",visible("Pneu was stopped in the background"));assertTrue("Kill evidence must be dismissible",visible("Dismiss"));clickPneu(By.text("Dismiss"),"kill evidence Dismiss")
            withTimeout(4000) {while(ServiceEvidence.killed(c))delay(30)}
            assertFalse(c.health().killed)
            device.pressBack();launch("home");assertFalse(visible("Pneu was stopped in the background"))
        } catch(e:Throwable) {
            val dir=File(c.filesDir,"fourth-review-screens").apply {mkdirs()}
            device.takeScreenshot(File(dir,"ui-failure.png"));device.dumpWindowHierarchy(File(dir,"ui-failure.xml"));throw e
        } finally {
            denied.forEach {grant(it)};c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
        }
    }
}
