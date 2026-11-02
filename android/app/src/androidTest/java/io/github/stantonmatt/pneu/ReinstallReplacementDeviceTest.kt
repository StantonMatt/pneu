package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.datastore.preferences.core.*
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.atomic.AtomicInteger

/** Real settings interactions use an empty rule and MainActivity: never reuse a
 * fixture rule whose clock was frozen by a screenshot test. */
@RunWith(AndroidJUnit4::class)
class ReinstallReplacementDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val instrumentation=InstrumentationRegistry.getInstrumentation()
    private val context=instrumentation.targetContext;private val app=context.graph
    private val device=UiDevice.getInstance(instrumentation)
    private val oldId="a".repeat(32);private val freshId="b".repeat(32)
    private val name="MJStanton-Desktop"
    private val localPeers=mutableListOf<LocalPeerFixture>()
    private val phase=File(context.filesDir,"replacement-capture-phase")
    private val release=File(context.filesDir,"replacement-capture-release")
    private suspend fun seed(implied:Boolean=false):ReplacementChoice {
        val old=Peer(name,"a".repeat(64),oldId,emptyList(),generation=1,lastConnected=System.currentTimeMillis()-7*86400000L,
            clipboardSend=false,clipboardReceive=true,automaticMedia=false)
        app.preferences.edit {p->
            p[longPreferencesKey("pairingRevision")]=1
            PeerRegistry.write(p,listOf(old))
            // The snackbar only appears when the replaced entry was mirroring.
            p[booleanPreferencesKey("mirrorInitialized")]=true;p[booleanPreferencesKey("mirrorEnabled")]=true
            p[stringSetPreferencesKey("mirrorComputers")]=setOf(io.github.stantonmatt.pneu.features.NotificationConsent.computer(old))
        }
        val payload=File(context.cacheDir,"replacement-report.pdf").apply {writeText("Replacement resend fixture")}
        app.queue.insert(Outgoing().apply {mediaId="replacement-report";transferId=randomId();peerId=oldId;bindingGeneration=1;kind="file";fileName="Report.pdf";automatic=false;path=payload.absolutePath;size=payload.length();hash=sha256(payload.readBytes());mime="application/pdf"})
        val fixture=LocalPeerFixture(app).also {localPeers+=it}
        val uri=fixture.invitation(freshId,name)
        val before=app.preferences.read()
        assertTrue(app.requestPairing(uri,ConfirmedBinding.from(before),replaceOld=if(implied)ConfirmedBinding.from(before,oldId) else null))
        app.connection.beforeFirstFrame={awaitCancellation()}
        app.connection.start(app.scope)
        val p=withTimeout(10000) {app.prefs.first {if(implied)it.replacements.replaced.isNotEmpty() else it.replacements.offers.isNotEmpty()}}
        withTimeout(10000) {while(!app.connection.connected(freshId))delay(20)}
        assertEquals(1,fixture.pairRequests.get())
        return if(implied)p.replacements.replaced.single() else p.replacements.offers.single()
    }

    private fun launch()=ActivityScenario.launch<MainActivity>(Intent(context,MainActivity::class.java).putExtra("home",true))
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset();app.initialized.await()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))
            instrumentation.uiAutomation.grantRuntimePermission(context.packageName,permission)
        app.preferences.toggle("setup",true)
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,false)
        app.preferences.toggle("queue",true)
    }
    @After fun cleanup()=runBlocking<Unit> {
        phase.delete();release.delete()
        app.preferences.beforeMutation={}
        app.replacementNotificationAction=ReplacementNotificationAction {}
        device.executeShellCommand("settings delete secure accessibility_interactive_ui_timeout_ms")
        device.executeShellCommand("cmd uimode night no")
        app.connection.stop();app.connection.awaitStopped();localPeers.forEach {it.close()}
        DeviceFixture.reset()
    }
    private fun captureBarrier(value:String) {
        if(InstrumentationRegistry.getArguments().getString("replacementCaptures")!="true")return
        release.delete();phase.writeText(value)
        val until=android.os.SystemClock.elapsedRealtime()+45000
        while(!release.exists() && android.os.SystemClock.elapsedRealtime()<until)Thread.sleep(100)
        assertTrue("Host captured $value",release.exists())
        release.delete();phase.delete()
    }
    @Test(timeout=180000) fun replaceDialogSnackbarAndExplicitResend()=runBlocking<Unit> {
        // Keep the snackbar available for host screenshots through the device panel.
        device.executeShellCommand("settings put secure accessibility_interactive_ui_timeout_ms 120000")
        val choice=seed();val turnedOn=AtomicInteger()
        app.replacementNotificationAction=ReplacementNotificationAction {binding->
            assertEquals(freshId,binding.pcId);assertEquals(choice.fresh.pin,binding.pin);turnedOn.incrementAndGet()
        }
        launch().use {
            compose.waitUntil(5000) {compose.onAllNodesWithText("Replace the old $name?").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Keep both").assertIsDisplayed()
            compose.onNodeWithText("Replace").assertIsDisplayed()
            for(theme in listOf("light","dark")) {
                device.executeShellCommand("cmd uimode night ${if(theme=="dark")"yes" else "no"}")
                compose.waitForIdle();captureBarrier("dialog-$theme")
            }
            compose.onNodeWithText("Replace").performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Notifications are off for $name").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Turn on").assertIsDisplayed()
            assertEquals(0,turnedOn.get())
            val replaced=app.preferences.read()
            assertEquals(listOf(freshId),replaced.devices.map {it.deviceId})
            assertFalse(replaced.devices.single().clipboardSend);assertFalse(replaced.devices.single().automaticMedia)
            compose.onNodeWithText("Not sent · old device replaced").assertIsDisplayed()
            for(theme in listOf("light","dark")) {
                device.executeShellCommand("cmd uimode night ${if(theme=="dark")"yes" else "no"}")
                compose.waitForIdle();captureBarrier("snackbar-$theme")
            }
            compose.onNodeWithText("Turn on").performClick()
            withTimeout(5000) {while(turnedOn.get()==0)delay(10)}
            assertEquals(1,turnedOn.get())
            val cancelled=app.queue.delivery("replacement-report",oldId)
            assertEquals(ReinstallReplacement.CANCELLED,cancelled.error)
            assertNull(app.queue.delivery(cancelled.mediaId,freshId))
            assertTrue(app.retry(cancelled.mediaId,ConfirmedQueue.from(app.preferences.read(),listOf(cancelled))))
            assertNull(app.queue.delivery(cancelled.mediaId,oldId))
            assertNull(app.queue.delivery(cancelled.mediaId,freshId))
            val resend=app.queue.pendingPeer(freshId).single {it.fileName==cancelled.fileName}
            assertNotEquals(cancelled.mediaId,resend.mediaId);assertNotEquals(cancelled.transferId,resend.transferId)
            assertTrue(resend.orderId>cancelled.orderId)
            assertEquals("waiting",resend.state)
            assertEquals(choice.fresh.revision,resend.bindingGeneration)
        }
    }
    @Test(timeout=30000) fun pairAgainCommitsWithoutPromptAndStaleResendCannotTargetAnotherIdentity()=runBlocking<Unit> {
        val choice=seed(implied=true)
        assertTrue(choice.implied);assertTrue(app.preferences.read().replacements.offers.isEmpty())
        assertEquals(listOf(freshId),app.preferences.read().devices.map {it.deviceId})
        // A queued cancellation is still removable if the replacement is repaired again.
        val row=Outgoing().apply {mediaId="stale-resend";transferId=randomId();peerId=oldId;bindingGeneration=1;state="failed";error=ReinstallReplacement.CANCELLED;nextAttempt=Long.MAX_VALUE;kind="file";automatic=false}
        app.queue.insert(row)
        val selected=ConfirmedQueue.from(app.preferences.read(),listOf(row))
        app.preferences.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).map {it.copy(pin="c".repeat(64),generation=it.generation+1)})}
        assertFalse(app.retry(row.mediaId,selected))
        assertEquals(ReinstallReplacement.CANCELLED,app.queue.delivery(row.mediaId,oldId).error)
        assertTrue(app.discard(selected))
        assertEquals("discarded",app.queue.delivery(row.mediaId,oldId).state)
    }
    @Test(timeout=60000) fun folderResendRetainsSafSourceAndFrozenDescriptor()=runBlocking<Unit> {
        seed(implied=true)
        val grants=android.net.Uri.parse("content://io.github.stantonmatt.pneu.test.folder-grants")
        val provider=android.net.Uri.parse("content://io.github.stantonmatt.pneu.test.folders")
        val uri=android.provider.DocumentsContract.buildTreeDocumentUri(provider.authority,"root")
        context.contentResolver.call(grants,"grant",null,null)
        try {
            context.contentResolver.call(provider,"fixture","abc",null)
            val tree=io.github.stantonmatt.pneu.features.SafFolderTree(context,uri)
            val capture=io.github.stantonmatt.pneu.features.FolderBuilder.prepare(tree,tree.rootId) {_,_,_->}
            val row=app.queue.delivery("replacement-report",oldId)
            row.origin="folder";row.mime=FOLDER_MIME;row.uri=uri.toString();row.size=capture.size;row.hash=capture.hash
            row.text=com.google.gson.Gson().toJson(capture.summary)
            File(row.path).writeText(capture.encode());app.queue.replacePayload(row)
            assertTrue(app.retry(row.mediaId,ConfirmedQueue.from(app.preferences.read(),listOf(row))))
            val resend=app.queue.pendingPeer(freshId).single {it.origin=="folder"}
            assertEquals(uri.toString(),resend.uri)
            assertNotEquals(row.path,resend.path)
            app.folderSends.open(resend).use {it.verify()}
        } finally {context.contentResolver.call(grants,"revoke",null,null)}
    }

    @Test(timeout=30000) fun finishedRecentAndClipboardHistoryFollowReplacement()=runBlocking<Unit> {
        val choice=seed()
        app.queue.insert(Outgoing().apply {mediaId="finished";transferId=randomId();peerId=oldId;bindingGeneration=1;state="sent";kind="file";fileName="Finished.pdf"})
        app.queue.insertReceivedFile(ReceivedFile().apply {peerId=oldId;transferId="finished-incoming";generation=1;state="complete";senderName=name;fileName="Received.pdf"})
        app.savedClips.setEnabled(true)
        app.savedClips.recordText("Finished clipboard",name,source=oldId)
        app.replaceReinstalledDevice(choice)
        assertEquals("sent",app.queue.delivery("finished",freshId).state)
        assertEquals(choice.fresh.revision,app.queue.delivery("finished",freshId).bindingGeneration)
        assertNull(app.queue.delivery("finished",oldId))
        assertEquals(name,app.queue.receivedFile(freshId,"finished-incoming").senderName)
        assertNull(app.queue.receivedFile(oldId,"finished-incoming"))
        val clip=app.savedClips.state.value.items.single {it.text=="Finished clipboard"}
        assertEquals(freshId,clip.source);assertEquals(name,clip.sender)
        assertEquals(ReinstallReplacement.CANCELLED,app.queue.delivery("replacement-report",oldId).error)
        assertNull(app.queue.delivery("replacement-report",freshId))
        app.completePeerReset(oldId)
        assertNotNull(app.queue.delivery("finished",freshId));assertNotNull(app.queue.receivedFile(freshId,"finished-incoming"))
    }
    @Test(timeout=60000) fun connectedRetryPreparesThenDevicePageShowsTransferAndReturnsToNormal()=runBlocking<Unit> {
        seed(implied=true)
        val old=app.queue.delivery("replacement-report",oldId)
        assertTrue(app.discard(ConfirmedQueue.from(app.preferences.read(),listOf(old))))
        val file=File(context.cacheDir,"live-progress.pdf").apply {writeText("Live progress fixture")}
        val row=Outgoing().apply {
            mediaId="live-progress";transferId=randomId();peerId=freshId;bindingGeneration=app.preferences.read().devices.single().generation
            kind="file";fileName="Progress.pdf";automatic=false;state="failed";error="disk_full";nextAttempt=Long.MAX_VALUE
            path=file.path;size=file.length();hash=sha256(file.readBytes());mime="application/pdf"
        }
        val allowSend=CompletableDeferred<Unit>()
        app.connection.beforeFirstFrame={allowSend.await()}
        DeviceFixture.insert(app,row)
        launch().use {
            compose.waitUntil(5000) {compose.onAllNodesWithContentDescription("Retry").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithContentDescription("Retry").performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Preparing").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Waiting").assertDoesNotExist();compose.onNodeWithText("1 waiting").assertDoesNotExist()
            allowSend.complete(Unit)
            // The fixture deliberately withholds ACK, holding a real in-flight transfer.
            withTimeout(10000) {app.connection.devices.first {it[freshId]?.transferMediaId==row.mediaId}}
            compose.onAllNodesWithText(name).onLast().performClick()
            val progress="Sending Progress.pdf · ${io.github.stantonmatt.pneu.features.UiPresentation.sizePair(row.size,row.size)}"
            compose.waitUntil(5000) {compose.onAllNodesWithText(progress).fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText(progress).assertIsDisplayed();captureBarrier("live-device-progress")
            localPeers.last().sendControl(Frame.Control(Type.ACK,json("transfer_id" to row.transferId,"deduplicated" to false)))
            withTimeout(5000) {while(app.queue.delivery(row.mediaId,freshId).state!="sent")delay(20)}
            compose.waitUntil(5000) {compose.onAllNodesWithText("Connected").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Send files").assertIsDisplayed();compose.onNodeWithText(progress).assertDoesNotExist()
        }
    }
    @Test(timeout=60000) fun knownPairedAddressIsNeutralWhilePromptedAndAfterKeepBoth()=runBlocking<Unit> {
        seed()
        val peers=app.preferences.read().devices
        val addresses=peers.single {it.deviceId==freshId}.addresses
        app.preferences.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).map {if(it.deviceId==oldId)it.copy(addresses=addresses) else it})}
        app.connection.retryPeer("a".repeat(64))
        withTimeout(15000) {app.connection.devices.first {it[oldId]?.mismatchPin==peers.single {peer->peer.deviceId==freshId}.pin}}
        launch().use {
            compose.waitUntil(5000) {compose.onAllNodesWithText("Keep both").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Couldn't verify computer").assertDoesNotExist()
            captureBarrier("known-address-prompt")
            compose.onNodeWithText("Keep both").performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Not connected").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Couldn't verify computer").assertDoesNotExist()
            captureBarrier("known-address-kept")
        }
    }
    @Test(timeout=30000) fun backMeansKeepBoth()=runBlocking<Unit> {
        seed()
        launch().use {
            compose.waitUntil(5000) {compose.onAllNodesWithText("Keep both").fetchSemanticsNodes().isNotEmpty()}
            device.pressBack()
            withTimeout(5000) {app.prefs.first {it.replacements.offers.isEmpty()}}
            assertEquals(2,app.preferences.read().devices.size)
            assertEquals("waiting",app.queue.delivery("replacement-report",oldId).state)
        }
    }
    @Test(timeout=30000) fun keepBothKeepsNewPairAndSuppressesOfferAcrossRecreation()=runBlocking<Unit> {
        seed()
        launch().use {scenario->
            compose.waitUntil(5000) {compose.onAllNodesWithText("Keep both").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Keep both").performClick()
            withTimeout(5000) {app.prefs.first {it.replacements.offers.isEmpty()}}
            scenario.recreate()
            compose.waitForIdle();compose.onNodeWithText("Replace the old $name?").assertDoesNotExist()
            val p=app.preferences.read()
            assertEquals(2,p.devices.size);assertEquals(1,p.replacements.declined.size)
            assertEquals("waiting",app.queue.delivery("replacement-report",oldId).state)
        }
    }
}
