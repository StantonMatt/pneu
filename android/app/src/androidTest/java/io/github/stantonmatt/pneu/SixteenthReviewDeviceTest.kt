package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Notification
import android.app.NotificationManager
import android.content.Intent
import android.graphics.Bitmap
import android.net.Uri
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.DeliveryPolicy
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class SixteenthReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private fun raw()="pneu://pair?v=1&name=Ready%20PC&fp=${"a".repeat(64)}&addr=100.64.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private fun item(order:Long)=Outgoing().apply {mediaId=randomId();transferId=randomId();orderId=order;kind="file";automatic=false;fileName="fixture-$order.bin";nextAttempt=Long.MAX_VALUE}
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(200);app.forgetBinding()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true)
        app.preferences.toggle("wifi",true)
        // Keep a valid transport policy but no eligible route to the fixture PC.
        for(key in listOf("tailscale","screenshots","photos","recordings"))app.preferences.toggle(key,false)
    }
    @After fun cleanup()=runBlocking<Unit> {
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.preferences.ensureIdentity();app.forgetBinding();app.staging.cleanup()
    }
    @Test(timeout=20000) fun pairingCompositionWaitsForIdentityAndThenFreezesItsConfirmation()=runBlocking<Unit> {
        app.preferences.setString("deviceId","");withTimeout(3000) {app.prefs.first {it.loaded && it.deviceId.isEmpty()}}
        c.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(raw()),c,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        delay(800);assertFalse(device.findObject(UiSelector().text("Pair with Ready PC?")).exists())
        assertTrue(device.findObject(UiSelector().className("android.widget.ProgressBar")).exists())
        // Bounded readiness failure is visible; the identity retry can still recover.
        assertTrue(device.findObject(UiSelector().text("Couldn't load Pneu settings")).waitForExists(6500))
        app.preferences.ensureIdentity()
        assertTrue(device.findObject(UiSelector().text("Pair with Ready PC?")).waitForExists(5000))
        val original=app.preferences.read();app.preferences.setString("deviceId",randomId());withTimeout(3000) {app.prefs.first {it.deviceId!=original.deviceId}}
        device.findObject(UiSelector().text("Pair")).click()
        assertTrue(device.findObject(UiSelector().text("Couldn't start pairing. Try again.")).waitForExists(5000));assertNull(app.preferences.read().pending)
    }
    @Test(timeout=15000) fun interruptedOptimizationReusesExactPublishedSnapshot()=runBlocking<Unit> {
        val row=item(1).apply {kind="photo";mime="image/png";fileName="fixture.png"}
        val source=File(c.filesDir,"outgoing/${row.transferId}.bin").apply {parentFile!!.mkdirs()}
        source.outputStream().use {out->Bitmap.createBitmap(128,128,Bitmap.Config.ARGB_8888).apply {eraseColor(0xff286ad8.toInt());compress(Bitmap.CompressFormat.PNG,100,out);recycle()}}
        row.path=source.path;row.size=source.length();row.hash=sha256(source.readBytes());app.queue.insert(row)
        val encoded=app.staging.optimize(row);assertTrue(encoded.optimized);assertFalse(encoded.attempted);assertFalse(source.exists())
        val bytes=File(encoded.path).readBytes();val id=encoded.transferId;val hash=encoded.hash;val path=encoded.path
        // Simulate cancellation/death between the durable encoding commit and attempted=true.
        repeat(3) {
            app.recoverOutgoing();val recovered=app.queue.find(row.mediaId)!!
            assertFalse(DeliveryPolicy.optimize(recovered.kind,recovered.mime,true,false,recovered.attempted,recovered.optimized))
            val again=app.staging.optimize(recovered)
            assertEquals(id,again.transferId);assertEquals(hash,again.hash);assertEquals(path,again.path);assertArrayEquals(bytes,File(again.path).readBytes());assertFalse(again.attempted)
        }
        assertEquals(1,File(path).parentFile!!.listFiles()!!.count {it.extension=="bin"})
    }

}

@RunWith(AndroidJUnit4::class)
class SixteenthRecentDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private fun raw()="pneu://pair?v=1&name=Ready%20PC&fp=${"a".repeat(64)}&addr=100.64.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private fun item(order:Long)=Outgoing().apply {mediaId=randomId();transferId=randomId();orderId=order;kind="file";automatic=false;fileName="fixture-$order.bin";nextAttempt=Long.MAX_VALUE}
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(200);app.forgetBinding()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true)
        app.preferences.toggle("wifi",true)
        // Keep a valid transport policy but no eligible route to the fixture PC.
        for(key in listOf("tailscale","screenshots","photos","recordings"))app.preferences.toggle(key,false)
    }
    @After fun cleanup()=runBlocking<Unit> {
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.preferences.ensureIdentity();app.forgetBinding();app.staging.cleanup()
    }
    @Test(timeout=45000) fun recentReservesActiveSlotsAndNotificationUsesTransferMetadata()=runBlocking<Unit> {
        val invitation=raw()
        assertTrue(app.requestPairing(invitation,null));assertTrue(app.installBinding(Invitation.parse(invitation),randomId()))
        repeat(60) {n->app.queue.insert(item(n+1L).apply {state=if(n%2==0)"failed" else "waiting"})}
        val active=item(61).apply {state="sending";attempted=true;fileName="Active-report.pdf";size=8192}
        val text=item(62).apply {state="sending";kind="text";text="active clipboard"};app.queue.insert(active);app.queue.insert(text)
        val recent=withTimeout(3000) {app.queue.recent().first {rows->rows.any {it.mediaId==active.mediaId}}}
        assertEquals(50,recent.size);assertTrue(recent.any {it.mediaId==text.mediaId});assertEquals((1L..48L).toList(),recent.take(48).map {it.orderId})
        withTimeout(3000) {app.recent.first {rows->rows.any {it.mediaId==active.mediaId} && rows.any {it.mediaId==text.mediaId}}}
        PneuService.start(c);delay(300)
        val marker=LinkState(status="Connected",transferMediaId="outside-recent",transferKind="file",transferFileName=active.fileName,total=active.size,sent=2048)
        app.connection.state.value=marker
        withTimeout(3000) {while(c.getSystemService(NotificationManager::class.java).activeNotifications.none {it.id==1 && it.notification.extras.getString(Notification.EXTRA_TITLE)=="Sending Active-report.pdf"})delay(50)}
        assertFalse(app.recent.value.any {it.mediaId==marker.transferMediaId})
        app.connection.state.value=marker.copy(transferMediaId=active.mediaId)
        c.startActivity(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        compose.waitForIdle();compose.onNodeWithText("Recent").assertExists()
        // Exercise the production LazyColumn directly; platform UiScrollable on
        // API 37 keeps stale accessibility positions after the first scroll.
        compose.onNode(hasScrollToIndexAction()).performScrollToNode(hasText(active.fileName))
        compose.onNodeWithText(active.fileName).assertIsDisplayed()
        compose.onNode(hasText("Sending ·",substring=true)).assertIsDisplayed()
        compose.onNodeWithContentDescription("Cancel").assertIsDisplayed()
        device.dumpWindowHierarchy(File(c.cacheDir,"sixteenth-recent.xml"))
        compose.onNodeWithContentDescription("Cancel").performClick()
        withTimeout(3000) {while(app.queue.find(active.mediaId)?.state!="discarded")delay(50)}
        assertEquals("sending",app.queue.find(text.mediaId)!!.state)
        app.queue.clearOutgoing()
        repeat(70) {n->app.queue.insert(item(100+n.toLong()).apply {state="sent"})}
        val history=withTimeout(3000) {app.queue.recent().first {it.size==50 && it.all {row->row.state=="sent"}}}
        assertEquals((169L downTo 120L).toList(),history.map {it.orderId})
        val waiting=item(200);val sending=item(201).apply {state="sending"}
        app.queue.insert(waiting);app.queue.insert(sending)
        val mixed=withTimeout(3000) {app.queue.recent().first {rows->rows.any {it.mediaId==sending.mediaId}}}
        assertEquals(50,mixed.size);assertEquals(listOf(200L,201L),mixed.take(2).map {it.orderId})
        assertEquals((169L downTo 122L).toList(),mixed.drop(2).map {it.orderId})
    }
}

/** Run only after the host has cleared data and launched a real deep link, before
 * any instrumentation can initialize the phone identity on its behalf. */
@RunWith(AndroidJUnit4::class)
class SixteenthFreshPairingDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.resumeFreshPairing()
    }

    @Test(timeout=10000) fun freshDeepLinkCreatedAReadyIdentity()=runBlocking<Unit> {
        val i=InstrumentationRegistry.getInstrumentation();val c=i.targetContext;val app=c.graph
        // The host already witnessed the confirmation. Starting instrumentation
        // restarts the target process and removes its activity; inspect durable state.
        val p=app.awaitPrefs();assertTrue(p.loaded);assertTrue(p.deviceId.isNotEmpty());assertTrue(p.tlsAlias.isNotEmpty());assertNull(p.peer);assertNull(p.pending)
        UiDevice.getInstance(i).pressHome()
    }
}
