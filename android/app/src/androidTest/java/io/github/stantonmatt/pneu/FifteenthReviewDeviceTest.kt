package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean

@RunWith(AndroidJUnit4::class)
class FifteenthReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private fun raw(name:String,pin:Char)="pneu://pair?v=1&name=$name&fp=${pin.toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private suspend fun bind(name:String="Original",pin:Char='a',pcId:String=randomId()) {
        val uri=raw(name,pin);assertTrue(app.requestPairing(uri,app.preferences.read().peer?.pin));assertTrue(app.installBinding(Invitation.parse(uri),pcId))
    }
    private fun item(id:String=randomId())=Outgoing().apply {
        mediaId=id;transferId=randomId();automatic=false;kind="file";fileName="fixture.bin"
        path=File(c.filesDir,"outgoing/$transferId.bin").apply {parentFile!!.mkdirs();writeBytes(byteArrayOf(1,2,3,4))}.path
        app.queue.insert(this)
    }
    private fun visible(text:String)=device.findObject(UiSelector().text(text)).waitForExists(5000)
    private fun click(text:String) {assertTrue(text,visible(text));device.findObject(UiSelector().text(text)).click()}
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(200);app.forgetBinding();bind()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true)
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeReadStart={};app.connection.beforeUnpairRemoval={}
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
    }
    @Test(timeout=25000) fun unpairDialogKeepsTheOriginallyNamedPcAndRejectsReplacement()=runBlocking<Unit> {
        c.startActivity(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        assertTrue(device.findObject(UiSelector().description("Settings")).waitForExists(5000));device.findObject(UiSelector().description("Settings")).click()
        UiScrollable(UiSelector().scrollable(true)).scrollTextIntoView("Unpair");click("Unpair");assertTrue(visible("Unpair from Original?"))
        bind("Replacement",'c');val row=item();val replacement=app.preferences.read();delay(200)
        assertTrue(visible("Unpair from Original?"));click("Unpair")
        assertTrue(visible("Couldn't finish unpairing. Try again."));assertEquals(replacement.peer,app.preferences.read().peer);assertEquals(replacement.deviceId,app.preferences.read().deviceId);assertNotNull(app.queue.find(row.mediaId));assertTrue(File(row.path).exists())
        assertTrue(c.getSystemService(android.app.NotificationManager::class.java).activeNotifications.any {it.id==1})
        click("Try again");assertTrue(visible("Unpair from Replacement?"));click("Cancel")
    }
    @Test(timeout=15000) fun delayedUnpairRejectsSamePcRepairAndChangedPhoneIdentity()=runBlocking<Unit> {
        val original=app.preferences.read();val confirmed=ConfirmedBinding.from(original);val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        app.connection.beforeUnpairRemoval={entered.complete(Unit);release.await()}
        try {
            val result=async {app.connection.unpair(confirmed)};withTimeout(3000) {entered.await()}
            bind("Original",'a',original.peer!!.deviceId);val repaired=app.preferences.read();val row=item()
            release.complete(Unit);assertFalse(result.await());assertEquals(repaired.pairingRevision,app.preferences.read().pairingRevision);assertNotNull(app.queue.find(row.mediaId))
            app.connection.beforeUnpairRemoval={};app.forgetBinding();bind();val reinstalled=app.preferences.read()
            assertFalse(app.connection.unpair(confirmed));assertEquals(reinstalled.deviceId,app.preferences.read().deviceId);assertNotNull(app.preferences.read().peer)
        }finally {release.complete(Unit)}
    }
    @Test(timeout=20000) fun replacementUsesTheConfirmedIdentityAcrossItsInitialRead()=runBlocking<Unit> {
        val original=ConfirmedBinding.from(app.preferences.read());val once=AtomicBoolean();val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>();val invitation=raw("Proposed",'e')
        app.preferences.beforeReadStart={if(once.compareAndSet(false,true)) {entered.complete(Unit);release.await()}}
        try {
            val result=async {runCatching {beginPair(c,invitation,original)}};withTimeout(3000) {entered.await()}
            bind("Replacement",'a');val replacement=app.preferences.read();release.complete(Unit)
            assertTrue(result.await().isFailure);assertNull(app.preferences.read().pending);assertEquals(replacement.peer,app.preferences.read().peer)
        }finally {release.complete(Unit);app.preferences.beforeReadStart={}}
    }
    @Test(timeout=25000) fun replacementDialogAndRetryConfirmTheNamedPc()=runBlocking<Unit> {
        val proposed=raw("Proposed",'e')
        c.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(proposed),c,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        assertTrue(visible("Replace Original with Proposed?"));bind("Replacement",'a');val replacement=app.preferences.read();delay(200)
        assertTrue(visible("Replace Original with Proposed?"));click("Replace")
        assertTrue(visible("Couldn't start pairing. Try again."));assertNull(app.preferences.read().pending);assertEquals(replacement.peer,app.preferences.read().peer)
        click("Try again");assertTrue(visible("Replace Replacement with Proposed?"));click("Cancel")
    }
    @Test(timeout=15000) fun queueActionsNeverExpandTheirSelectionOrRemoveNewerRows()=runBlocking<Unit> {
        val first=item();val second=item();val selected=ConfirmedQueue.from(app.preferences.read(),listOf(first,second));val later=item()
        assertTrue(app.discardAll(selected));assertEquals("discarded",app.queue.find(first.mediaId)!!.state);assertEquals("discarded",app.queue.find(second.mediaId)!!.state);assertFalse(File(first.path).exists());assertTrue(File(later.path).exists());assertEquals("waiting",app.queue.find(later.mediaId)!!.state)
        val stale=ConfirmedQueue.from(app.preferences.read(),listOf(later));later.revision++;app.queue.update(later)
        assertFalse(app.discard(stale));assertFalse(app.retry(later.mediaId,stale));assertTrue(File(later.path).exists())
        val current=ConfirmedQueue.from(app.preferences.read(),listOf(later));later.transferId=randomId();app.queue.update(later);assertFalse(app.discard(current));assertFalse(app.retry(later.mediaId,current))
        val beforeAck=ConfirmedQueue.from(app.preferences.read(),listOf(later));later.state="sent";app.queue.update(later);assertFalse(app.discard(beforeAck));assertTrue(File(later.path).exists())
        val beforeReplacement=ConfirmedQueue.from(app.preferences.read(),listOf(item("same-media-id")));bind("Replacement",'c');val replacement=item("same-media-id")
        app.preferences.toggle("localCapacity",true)
        assertFalse(app.discardAll(beforeReplacement));assertFalse(app.retry(replacement.mediaId,beforeReplacement));assertTrue(app.preferences.read().localCapacity);assertTrue(File(replacement.path).exists());assertEquals("waiting",app.queue.find(replacement.mediaId)!!.state)
        val fresh=ConfirmedQueue.from(app.preferences.read(),listOf(replacement));assertTrue(app.retry(replacement.mediaId,fresh));assertEquals(1L,app.queue.find(replacement.mediaId)!!.revision)
        val final=ConfirmedQueue.from(app.preferences.read(),listOf(app.queue.find(replacement.mediaId)!!));assertTrue(app.discard(final));assertFalse(File(replacement.path).exists())
    }
    @Test(timeout=15000) fun cancelledAttemptMustMatchBothTheConfirmedBindingAndPendingInvitation()=runBlocking<Unit> {
        val original=ConfirmedBinding.from(app.preferences.read());val firstRaw=raw("First",'e');val secondRaw=raw("Second",'f')
        assertTrue(app.requestPairing(firstRaw,original));assertFalse(app.cancelPairing(null,original));assertTrue(app.requestPairing(secondRaw,original))
        assertFalse(app.cancelPairing(Invitation.parse(firstRaw),original));assertEquals(Invitation.parse(secondRaw),app.preferences.read().pending)
        bind("Replacement",'c');val replacement=ConfirmedBinding.from(app.preferences.read());assertFalse(app.cancelPairing(null,original));assertTrue(app.cancelPairing(null,replacement));assertNotNull(app.preferences.read().peer)
    }
    @Test(timeout=15000) fun transportRetryCannotRestartAReplacementOrNewAttempt()=runBlocking<Unit> {
        val original=ConfirmedBinding.from(app.preferences.read());bind("Replacement",'c')
        val marker=io.github.stantonmatt.pneu.connection.LinkState(status="Connected",pairing="Replacement marker")
        app.connection.state.value=marker
        assertFalse(app.retryConnection(original,null,feedback=true));assertEquals(marker,app.connection.state.value)
        val replacement=ConfirmedBinding.from(app.preferences.read());val proposed=raw("Proposed",'e')
        assertTrue(app.requestPairing(proposed,replacement));app.connection.state.value=marker
        assertFalse(app.retryConnection(replacement,null,feedback=true));assertEquals(marker,app.connection.state.value)
        assertTrue(app.retryConnection(replacement,Invitation.parse(proposed),feedback=true));assertEquals("Pairing…",app.connection.state.value.pairing)
    }

}
