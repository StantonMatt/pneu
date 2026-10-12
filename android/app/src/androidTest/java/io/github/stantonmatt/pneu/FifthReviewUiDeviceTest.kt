package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.io.IOException
import java.security.GeneralSecurityException

@RunWith(AndroidJUnit4::class)
class FifthReviewUiDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    @Test(timeout=60000) fun failingLifecycleAndSettingsWritesReportWithoutDiscardingPersistedState()=runBlocking<Unit> {
        val i=InstrumentationRegistry.getInstrumentation();val c=i.targetContext;val app=c.graph;val device=UiDevice.getInstance(i)
        fun visible(text:String)=device.findObject(UiSelector().text(text)).waitForExists(5000)
        fun click(text:String) {assertTrue(text,visible(text));device.findObject(UiSelector().text(text)).click()}
        fun screen(name:String) {device.takeScreenshot(File(c.filesDir,"fifth-review-screens/$name.png").apply {parentFile!!.mkdirs()})}
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        app.preferences.toggle("setup",true);app.preferences.toggle("quickSettings",true)
        val raw="pneu://pair?v=1&name=Trusted%20PC&fp=${"a".repeat(64)}&addr=10.0.2.2%3A47861&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
        val before=app.preferences.flow.first()
        try {
            c.startActivity(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
            assertTrue(device.findObject(UiSelector().description("Settings")).waitForExists(5000));device.findObject(UiSelector().description("Settings")).click()
            val failedPauses=java.util.concurrent.atomic.AtomicInteger()
            app.preferences.beforeMutation={if(it=="paused") {failedPauses.incrementAndGet();throw IOException("Synthetic pause write failure")}}
            val notification=withTimeout(5000) {while(c.getSystemService(android.app.NotificationManager::class.java).activeNotifications.none {it.id==1})delay(20);c.getSystemService(android.app.NotificationManager::class.java).activeNotifications.single {it.id==1}.notification}
            notification.actions.single {it.title.toString()=="Pause"}.actionIntent.send()
            withTimeout(4000) {while(failedPauses.get()<1)delay(20)};assertFalse(app.preferences.flow.first().paused)
            fun shell(command:String) {i.uiAutomation.executeShellCommand(command).use {android.os.ParcelFileDescriptor.AutoCloseInputStream(it).readBytes()}}
            shell("cmd statusbar add-tile io.github.stantonmatt.pneu/.features.PneuTile");device.openQuickSettings();delay(1000)
            shell("cmd statusbar click-tile io.github.stantonmatt.pneu/.features.PneuTile")
            withTimeout(4000) {while(failedPauses.get()<2)delay(20)};assertFalse(app.preferences.flow.first().paused);device.pressBack()
            app.preferences.beforeMutation={if(it=="forget")throw GeneralSecurityException("Synthetic Keystore preparation failure")}
            UiScrollable(UiSelector().scrollable(true)).scrollTextIntoView("Unpair");click("Unpair");click("Unpair")
            assertTrue(visible("Couldn't unpair"));assertEquals(before.peer,app.preferences.flow.first().peer);assertEquals(before.deviceId,app.preferences.flow.first().deviceId);screen("unpair-write-failed")
            click("Close");assertTrue(visible("Trusted PC"))
            app.preferences.beforeMutation={if(it=="shareSheet")throw IOException("Synthetic setting write failure")}
            UiScrollable(UiSelector().scrollable(true)).scrollTextIntoView("Show in share sheet");val original=app.preferences.flow.first().shareSheet
            click("Show in share sheet");assertTrue(visible("Couldn't save setting"));assertEquals(original,app.preferences.flow.first().shareSheet)
            app.preferences.beforeMutation={};click("Try again");withTimeout(5000) {app.prefs.first {it.shareSheet!=original}}
            // Failed replacement retains the same approved URI and existing binding.
            app.preferences.beforeMutation={if(it=="pending")throw IOException("Synthetic pairing write failure")}
            val replacement=raw.replace("Trusted%20PC","New%20PC").replace("a".repeat(64),"c".repeat(64))
            c.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(replacement),c,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
            click("Replace");assertTrue(visible("Couldn't start pairing"));assertEquals(before.peer,app.preferences.flow.first().peer);assertNull(app.preferences.flow.first().pending);screen("pair-write-failed")
            app.preferences.beforeMutation={};click("Try again");assertTrue(visible("Replace Trusted PC with New PC?"));click("Cancel")
            // Partial durable revocation: failed cleanup must show the actual unpaired state.
            c.startActivity(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK));assertTrue(device.findObject(UiSelector().description("Settings")).waitForExists(5000));device.findObject(UiSelector().description("Settings")).click()
            app.queue.insert(io.github.stantonmatt.pneu.data.Outgoing().apply {mediaId=randomId();transferId=randomId()})
            val db=app.database.openHelper.writableDatabase;db.execSQL("CREATE TRIGGER fail_cleanup BEFORE DELETE ON outgoing BEGIN SELECT RAISE(ABORT,'Synthetic cleanup failure'); END")
            UiScrollable(UiSelector().scrollable(true)).scrollTextIntoView("Unpair");click("Unpair");click("Unpair");assertTrue(visible("Couldn't unpair"));assertNull(app.preferences.flow.first().peer);assertTrue(app.preferences.flow.first().deliveryReset);screen("unpair-cleanup-failed")
            db.execSQL("DROP TRIGGER fail_cleanup")
            // The foreground service stops on local unpair; explicitly own a
            // background recovery loop to coordinate cleanup before this retry.
            app.connection.start(this);app.connection.kick()
            // Background recovery can beat the user's retry. The completed
            // revocation must remain idempotent, including its new TLS identity.
            val revoked=withTimeout(5000) {app.preferences.flow.first {!it.deliveryReset}}
            click("Try again");assertTrue(visible("Finish unpairing?"));click("Unpair")
            withTimeout(5000) {app.prefs.first {it.peer==null && !it.deliveryReset}}
            assertTrue(visible("Pair with PC"))
            assertEquals(revoked.deviceId,app.preferences.flow.first().deviceId)
            assertEquals(revoked.tlsAlias,app.preferences.flow.first().tlsAlias)
        } catch(e:Throwable) {screen("ui-failure");device.dumpWindowHierarchy(File(c.filesDir,"fifth-review-screens/ui-failure.xml"));throw e
        } finally {
            app.preferences.beforeMutation={};app.database.openHelper.writableDatabase.execSQL("DROP TRIGGER IF EXISTS fail_cleanup");device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
        }
    }
}
