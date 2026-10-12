package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.features.ManualEntries
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

/** Fault peers are independent TLS servers. Invoke individual methods with the matching flags. */
@RunWith(AndroidJUnit4::class)
class ConnectionFaultDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation()
    private val c=i.targetContext
    private val app=c.graph
    private val args=InstrumentationRegistry.getArguments()
    private val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
    private suspend fun seed() {
        val pin=requireNotNull(args.getString("pcPin"));val port=requireNotNull(args.getString("pcPort"))
        val raw="pneu://pair?v=1&name=Fault%20PC&fp=$pin&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250)
        if(args.getString("identityPrepared")=="true") {
            assertNull(app.preferences.flow.first().peer);assertNull(app.preferences.flow.first().pending)
        } else app.forgetBinding()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        app.preferences.toggle("wifi",true);app.preferences.toggle("tailscale",true);app.preferences.toggle("mobile",true);app.preferences.toggle("paused",false);seed()
    }
    @After fun cleanup()=runBlocking<Unit> {app.connection.stop();app.forgetBinding();scope.cancel()}
    private suspend fun connect() {app.connection.start(scope);withTimeout(12000) {while(!app.connection.connected)delay(20)}}
    private suspend fun state(id:String,state:String)=withTimeout(10000) {while(app.queue.find(id)?.state!=state)delay(20)}
    private suspend fun file(bytes:Int=64,automatic:Boolean=false,uri:String=""):Outgoing {
        val row=Outgoing().apply {mediaId=if(automatic)"external_primary:${android.provider.MediaStore.getVersion(c,"external_primary")}:images:${System.nanoTime()}" else randomId();transferId=randomId();kind="file";this.automatic=automatic;this.uri=uri;fileName="Fault fixture.bin";mime="application/octet-stream";createdAt=System.currentTimeMillis()}
        val file=File(c.filesDir,"outgoing/${row.transferId}.bin").apply {parentFile!!.mkdirs()}
        val digest=java.security.MessageDigest.getInstance("SHA-256");val b=ByteArray(256*1024)
        file.outputStream().use {out->var left=bytes;while(left>0){val n=minOf(left,b.size);out.write(b,0,n);digest.update(b,0,n);left-=n}}
        row.path=file.path;row.size=bytes.toLong();row.hash=digest.digest().hex();assertTrue(app.enqueueFixture(row));return row
    }
    private suspend fun discard(vararg rows:Outgoing) {
        assertTrue(app.discard(io.github.stantonmatt.pneu.data.ConfirmedQueue.from(app.preferences.read(),rows.toList())))
    }
    @Test(timeout=100000) fun failedCompletionAndFallbackWritesRecoverBothLanesWithoutSpinning()=runBlocking<Unit> {
        connect();app.preferences.toggle("screenshots",false)
        val db=app.database.openHelper.writableDatabase
        val opens=java.util.concurrent.atomic.AtomicInteger()
        app.connection.openPayload={opens.incrementAndGet();it.inputStream()}
        db.execSQL("CREATE TRIGGER fail_delivery_completion BEFORE UPDATE ON outgoing WHEN OLD.state='sending' AND NEW.state IN ('sent','failed') BEGIN SELECT RAISE(ABORT,'completion and fallback fault'); END")
        try {
            val first=file();val text=ManualEntries.text(app,"Text with failed completion write","share")
            withTimeout(10000) {while(PeerFixture.evidence().count {it.asJsonObject["event"].asString in setOf("FILE","TEXT")}<2)delay(20)}
            // Failed completions become retryable while their own monotonic
            // cooldowns prevent replay. Neither lane blocks fresh work.
            state(first.mediaId,"waiting");state(text.mediaId,"waiting")
            assertEquals(1,opens.get());assertTrue(File(first.path).exists())
            db.execSQL("DROP TRIGGER fail_delivery_completion")
            val next=file(8);val latest=ManualEntries.text(app,"New text after failed completion write","share")
            withTimeout(8000) {while(app.queue.find(next.mediaId)?.state!="sent" || app.queue.find(latest.mediaId)?.state!="sent")delay(20)}
            assertEquals("waiting",app.queue.find(first.mediaId)!!.state);assertEquals(2,opens.get())
            // The cooled-down snapshot later retries without a reconnect.
            withTimeout(75000) {while(app.queue.find(first.mediaId)?.state!="sent")delay(50)}
            assertEquals("sent",app.queue.find(first.mediaId)!!.state)
            assertEquals("superseded",app.queue.find(text.mediaId)!!.state)
            assertEquals(first.transferId,app.queue.find(first.mediaId)!!.transferId)
            assertEquals(3,opens.get()) // initial attempt, ready sibling, then same snapshot replay
            assertTrue(app.connection.connected)
            // Host witness: one HELLO, repeated exact file ID/hash, newest text only after recovery.
        } finally {db.execSQL("DROP TRIGGER IF EXISTS fail_delivery_completion");app.connection.openPayload={it.inputStream()}}
    }
    @Test(timeout=30000) fun repeatedlyIneligibleSelectionBacksOffAndDoesNotStarveSibling()=runBlocking<Unit> {
        connect();app.preferences.toggle("screenshots",false)
        val rejected=CompletableDeferred<String>();val rejections=java.util.concurrent.atomic.AtomicInteger()
        app.connection.beforeFirstFrame={row->
            if(row.mediaId==rejected.await()) {
                rejections.incrementAndGet()
                app.queueMutex.lock()
                try {val current=app.queue.find(row.mediaId)!!;current.revision++;app.queue.update(current)} finally {app.queueMutex.unlock()}
            }
        }
        try {
            val first=file();rejected.complete(first.mediaId)
            val next=file(8);state(next.mediaId,"sent")
            val before=rejections.get();assertTrue(before>0)
            delay(2200)
            assertEquals("waiting",app.queue.find(first.mediaId)!!.state)
            assertFalse(app.queue.find(first.mediaId)!!.attempted)
            assertTrue("No tight rehash loop: ${rejections.get()-before}",rejections.get()-before in 0..3)
            assertFalse(PeerFixture.evidence().any {it.asJsonObject.let {r->r["event"].asString=="FILE" && r["transfer_id"].asString==first.transferId}})
            app.connection.beforeFirstFrame={};state(first.mediaId,"sent")
            assertTrue(app.connection.connected)
            // Host witness: sibling precedes rejected row, exactly one BEGIN per file.
        } finally {app.connection.beforeFirstFrame={}}
    }
    @Test(timeout=45000) fun discardedWriterWaitersNeverEmitAndDoNotResetEitherLane()=runBlocking<Unit> {
        connect()
        val locked=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        val selected=kotlinx.coroutines.channels.Channel<Outgoing>(2)
        app.connection.beforeFirstFrame={selected.send(it)}
        val holder=scope.launch {app.connection.withWriteLockForTest {locked.complete(Unit);release.await()}}
        try {
            withTimeout(5000) {locked.await()}
            val file=file();val text=ManualEntries.text(app,"Discarded writer waiter","share")
            val waiters=withTimeout(5000) {setOf(selected.receive().mediaId,selected.receive().mediaId)}
            assertEquals(setOf(file.mediaId,text.mediaId),waiters)
            discard(file,text)
            for(row in listOf(file,text)) {
                val current=app.queue.find(row.mediaId)!!
                assertEquals("discarded",current.state);assertFalse(current.attempted)
            }
            app.connection.beforeFirstFrame={};release.complete(Unit);holder.join();delay(300)
            assertTrue(app.connection.connected)
            val nextFile=file();val nextText=ManualEntries.text(app,"Live after discarded waiters","share")
            state(nextFile.mediaId,"sent");state(nextText.mediaId,"sent");assertTrue(app.connection.connected)
            // Host witness: exactly one HELLO, FILE_BEGIN, FILE and TEXT; neither removed ID.
        } finally {app.connection.beforeFirstFrame={};release.complete(Unit);holder.cancelAndJoin()}
    }
    @Test(timeout=45000) fun discardedEmittedTextDrainsReceiptWithoutResettingFileLane()=runBlocking<Unit> {
        // --text-ack-delay 2 leaves an emitted CLIPBOARD awaiting its correlated receipt.
        connect();val first=ManualEntries.text(app,"Removed emitted text","share");state(first.mediaId,"sending")
        withTimeout(5000) {while(!PeerFixture.evidence().any {it.asJsonObject.let {e->e["event"].asString=="TEXT" && e["transfer_id"].asString==first.transferId}})delay(20)}
        discard(app.queue.find(first.mediaId)!!)
        val next=ManualEntries.text(app,"Text after removed emitted text","share");val file=file()
        state(next.mediaId,"sent");state(file.mediaId,"sent")
        assertEquals("discarded",app.queue.find(first.mediaId)!!.state);assertTrue(app.connection.connected)
        // Host witness: one HELLO, two TEXT, one verified FILE.
    }
    @Test(timeout=45000) fun pauseDuringHashHoldsAutomaticButNotManual()=runBlocking<Unit> {
        connect();app.preferences.toggle("screenshots",false)
        val reading=CompletableDeferred<Unit>();val release=java.util.concurrent.CountDownLatch(1)
        app.connection.openPayload={path->object:java.io.FileInputStream(path) {
            private var first=true
            override fun read(bytes:ByteArray):Int {
                if(first) {first=false;reading.complete(Unit);check(release.await(10,java.util.concurrent.TimeUnit.SECONDS))}
                return super.read(bytes)
            }
        }}
        try {
            val automatic=file(automatic=true)
            withTimeout(5000) {reading.await()};app.preferences.toggle("paused",true);release.countDown()
            app.connection.openPayload={it.inputStream()};delay(500)
            val held=app.queue.find(automatic.mediaId)!!;assertEquals("waiting",held.state);assertFalse(held.attempted)
            val manual=file(8);val text=ManualEntries.text(app,"Manual while automatic paused","share")
            state(manual.mediaId,"sent");state(text.mediaId,"sent")
            assertEquals("waiting",app.queue.find(automatic.mediaId)!!.state)
            assertFalse(PeerFixture.evidence().any {it.asJsonObject.let {e->e["event"].asString=="FILE" && e["transfer_id"].asString==automatic.transferId}})
            app.preferences.toggle("paused",false);state(automatic.mediaId,"sent");assertTrue(app.connection.connected)
            // Host witness: manual file (8 bytes) precedes automatic file (64 bytes).
        } finally {release.countDown();app.connection.openPayload={it.inputStream()}}
    }
    @Test(timeout=45000) fun pauseDuringEncodingKeepsPublishedSnapshotUnattemptedUntilResume()=runBlocking<Unit> {
        connect();assertTrue("Encoding fixture requires a metered active route",app.connection.cellular())
        app.preferences.toggle("screenshots",false);app.preferences.toggle("originals",false)
        val row=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="photo";automatic=true;fileName="Prepared photo.png";mime="image/png";createdAt=System.currentTimeMillis()}
        val payload=File(c.filesDir,"outgoing/${row.transferId}.bin")
        val bitmap=android.graphics.Bitmap.createBitmap(100,100,android.graphics.Bitmap.Config.ARGB_8888)
        try {payload.outputStream().use {assertTrue(bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG,100,it))}} finally {bitmap.recycle()}
        row.path=payload.path;row.size=payload.length();row.hash=sha256(payload.readBytes())
        val encoding=CompletableDeferred<Unit>();val release=java.util.concurrent.CountDownLatch(1)
        app.staging.beforePayloadWrite={encoding.complete(Unit);check(release.await(10,java.util.concurrent.TimeUnit.SECONDS))}
        try {
            assertTrue(app.enqueueFixture(row));withTimeout(5000) {encoding.await()}
            app.preferences.toggle("paused",true);release.countDown();app.staging.beforePayloadWrite={}
            val held=withTimeout(5000) {var current=app.queue.find(row.mediaId)!!;while(!current.optimized) {delay(20);current=app.queue.find(row.mediaId)!!};current}
            delay(300);assertEquals("waiting",app.queue.find(row.mediaId)!!.state);assertFalse(held.attempted)
            assertNotEquals(row.transferId,held.transferId);assertEquals("image/jpeg",held.mime)
            val manual=file();state(manual.mediaId,"sent")
            assertFalse(PeerFixture.evidence().any {it.asJsonObject.let {e->e["event"].asString=="FILE" && e["transfer_id"].asString==held.transferId}})
            app.preferences.toggle("paused",false);state(row.mediaId,"sent")
            val sent=app.queue.find(row.mediaId)!!;assertEquals(held.transferId,sent.transferId);assertEquals(held.hash,sent.hash)
            assertTrue(app.connection.connected)
            // Host witness: manual file precedes one JPEG with the published ID/hash.
        } finally {release.countDown();app.staging.beforePayloadWrite={}}
    }
    @Test(timeout=180000) fun supersessionAndManualRetryPreserveLargeFileConnection()=runBlocking<Unit> {
        connect();val large=file(200*1024*1024);state(large.mediaId,"sending")
        val first=ManualEntries.text(app,"In-flight text fixture","share");state(first.mediaId,"sending")
        val pending=ManualEntries.text(app,"Replaced pending text fixture","share")
        val latest=ManualEntries.text(app,"Latest concurrent text fixture","share")
        assertEquals("sending",app.queue.find(first.mediaId)!!.state)
        assertEquals("superseded",app.queue.find(pending.mediaId)!!.state)
        val later=file(8);later.state="failed";later.nextAttempt=Long.MAX_VALUE;app.updateDelivery(later)
        app.retry(later.mediaId) // A row retry must not reset the large file lane.
        withTimeout(60000) {while(app.queue.find(large.mediaId)?.state!="sent")delay(20)}
        state(first.mediaId,"sent");state(latest.mediaId,"sent");state(later.mediaId,"sent")
        assertTrue(app.connection.connected)
        // Independent peer asserts one HELLO and one BEGIN per file, with exact hashes.
    }
    @Test(timeout=30000) fun readyFileDrainsBeforeEarlierRetryDelay()=runBlocking<Unit> {
        val delayed=file();delayed.state="failed";delayed.nextAttempt=System.currentTimeMillis()+86_400_000 // saved deadline from before a clock rollback
        app.updateDelivery(delayed)
        val ready=file();connect();state(ready.mediaId,"sent")
        assertEquals("failed",app.queue.find(delayed.mediaId)!!.state);assertTrue(app.connection.connected)
    }
    @Test(timeout=30000) fun localUnpairDurablyRemovesBindingBeforeCapturedNotification()=runBlocking<Unit> {
        connect();val old=app.preferences.flow.first();app.connection.unpair()
        val fresh=app.preferences.flow.first();assertNull(fresh.peer);assertNull(fresh.pending);assertFalse(fresh.deliveryReset)
        assertNotEquals(old.deviceId,fresh.deviceId);assertNotEquals(old.tlsAlias,fresh.tlsAlias)
        assertFalse(app.connection.connected);assertTrue(app.queue.all().isEmpty())
        // Independent peer must receive UNPAIR on this authenticated old session.
    }
    @Test(timeout=180000) fun progressingLargeFileReachesVerifiedPeerAck()=runBlocking<Unit> {
        connect();val large=file(200*1024*1024)
        withTimeout(120000) {while(app.queue.find(large.mediaId)?.state!="sent")delay(50)}
        val sent=app.queue.find(large.mediaId)!!
        assertEquals(209715200L,sent.size);assertEquals("",sent.error)
        assertFalse(File(sent.path).exists());assertTrue(app.connection.connected)
        // The independent TLS peer verifies every offset, total size and SHA before ACK.
    }
    @Test(timeout=360000) fun progressingUploadBeyondThreeMinutesReachesVerifiedPeerAck()=runBlocking<Unit> {
        // --chunk-delay 0.3 throttles 160 MiB to >192 s, with no PC traffic
        // between HELLO and ACK. The independent peer asserts that silence.
        connect();val large=file(160*1024*1024)
        val started=android.os.SystemClock.elapsedRealtime()
        withTimeout(300000) {while(app.queue.find(large.mediaId)?.state!="sent")delay(100)}
        assertTrue(android.os.SystemClock.elapsedRealtime()-started>180000)
        assertEquals("",app.queue.find(large.mediaId)!!.error)
        assertTrue(app.connection.connected);assertFalse(File(large.path).exists())
    }
    @Test(timeout=30000) fun remoteUnpairClearsSuccessAndBindingDerivedState()=runBlocking<Unit> {
        // --unpair-after-pair-result exercises a real successful pairing, then remote UNPAIR.
        val peer=app.preferences.flow.first().peer!!
        val raw="pneu://pair?v=1&name=Fault%20PC&fp=${peer.pin}&addr=10.0.2.2%3A${args.getString("pcPort")}&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,peer.pin));app.connection.start(scope)
        withTimeout(12000) {app.connection.state.first {it.pairingSuccess(app.prefs.value)}}
        withTimeout(10000) {app.preferences.flow.first {it.peer==null && it.pending==null && !it.deliveryReset}}
        val link=app.connection.state.value
        assertEquals("",link.pairing);assertEquals("",link.route);assertEquals("",link.transferMediaId)
        assertFalse(link.pairingSuccess(app.preferences.flow.first()))
        assertEquals(0L,app.preferences.flow.first().lastConnected)
        app.preferences.toggle("setup",true)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.CAMERA)
        c.startActivity(Intent(c,MainActivity::class.java).putExtra("pair",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        val device=androidx.test.uiautomator.UiDevice.getInstance(i)
        assertTrue(device.wait(androidx.test.uiautomator.Until.hasObject(androidx.test.uiautomator.By.text("Scan the QR code on your PC")),5000))
        assertFalse(device.hasObject(androidx.test.uiautomator.By.textStartsWith("Paired with ")))
        File(c.filesDir,"e2e-fix-screens").mkdirs()
        device.takeScreenshot(File(c.filesDir,"e2e-fix-screens/remote-unpair.png"))
    }
    @Test(timeout=30000) fun pinnedHandshakeFaultPreservesBinding()=runBlocking<Unit> {
        val before=app.preferences.read()
        app.connection.start(scope);delay(13000);app.connection.stop()
        val after=app.preferences.read()
        assertEquals(before.peer,after.peer);assertEquals(before.deviceId,after.deviceId);assertFalse(after.pairingLost)
    }
    @Test(timeout=60000) fun threePinnedClientRejectionsForgetBindingAndRetainLostName()=runBlocking<Unit> {
        // --unauthorized-hello from the authenticated pinned peer.
        app.connection.start(scope)
        withTimeout(50000) {app.preferences.flow.first {it.pairingLost && it.peer==null && !it.deliveryReset}}
        val p=app.preferences.flow.first()
        assertNull(p.pending);assertEquals("Fault PC",p.lostPcName)
        assertEquals("Pairing lost",app.connection.state.value.status)
        assertEquals("",app.connection.state.value.pairing);assertTrue(app.queue.pending().isEmpty())
        app.preferences.toggle("setup",true)
        c.startActivity(Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        val device=androidx.test.uiautomator.UiDevice.getInstance(i)
        assertTrue(device.wait(androidx.test.uiautomator.Until.hasObject(androidx.test.uiautomator.By.text("Pairing lost")),5000))
        assertTrue(device.hasObject(androidx.test.uiautomator.By.text("Fault PC doesn't recognize this phone")))
        File(c.filesDir,"e2e-fix-screens").mkdirs()
        device.takeScreenshot(File(c.filesDir,"e2e-fix-screens/pairing-lost.png"))
    }
    @Test(timeout=30000) fun bindingChangesInvalidateTransientState()=runBlocking<Unit> {
        val p=app.preferences.flow.first();val peer=p.peer!!
        app.connection.state.value=io.github.stantonmatt.pneu.connection.LinkState("Connected","Wi-Fi","Paired with Fault PC",1,2,"fixture",pairingPin=peer.pin)
        assertTrue(app.connection.state.value.pairingSuccess(p))
        app.forgetBinding();assertEquals(io.github.stantonmatt.pneu.connection.LinkState(),app.connection.state.value)
        seed();val replacement=app.preferences.flow.first().peer!!
        app.connection.state.value=io.github.stantonmatt.pneu.connection.LinkState(pairing="Paired with Fault PC",pairingPin=replacement.pin)
        val raw="pneu://pair?v=1&name=Replacement&fp=${replacement.pin}&addr=10.0.2.2%3A${args.getString("pcPort")}&token=${"c".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,replacement.pin));assertEquals("",app.connection.state.value.pairing)
        assertTrue(app.installBinding(Invitation.parse(raw),"e".repeat(32)));assertEquals("",app.connection.state.value.pairing)
        // A stale old session's UNPAIR/revocation must not forget the replacement.
        assertFalse(app.loseBinding(replacement));app.forgetBinding(replacement)
        assertEquals("e".repeat(32),app.preferences.flow.first().peer!!.deviceId)
        assertFalse(app.connection.connected)
        app.connection.unpair();assertEquals("",app.connection.state.value.pairing)
    }
    @Test(timeout=30000) fun pinFailureFallsBackWithoutLosingTrust()=runBlocking<Unit> {
        app.preferences.setString("lastLan","10.0.2.2:${requireNotNull(args.getString("rejectPort"))}")
        connect();assertFalse(app.preferences.flow.first().pairingLost);assertNotNull(app.preferences.flow.first().peer)
    }
    @Test(timeout=30000) fun currentTransportPolicyStopsSessionAndEveryRetry()=runBlocking<Unit> {
        connect();app.preferences.toggle("wifi",false)
        withTimeout(5000) {while(app.connection.connected)delay(20)}
        val row=ManualEntries.text(app,"Held by current Wi-Fi policy","share");delay(3000)
        assertFalse(app.connection.connected);assertEquals("waiting",app.queue.find(row.mediaId)!!.state)
        app.preferences.toggle("wifi",true);withTimeout(7000) {while(!app.connection.connected)delay(20)};state(row.mediaId,"sent")
        if(app.connection.cellular()) {
            app.preferences.toggle("mobile",false);withTimeout(5000) {while(app.connection.connected)delay(20)}
            val next=ManualEntries.text(app,"Held by mobile policy","share");delay(3000);assertEquals("waiting",app.queue.find(next.mediaId)!!.state)
        }
    }
    @Test(timeout=30000) fun missingFileCapabilityIsVisibleAndDoesNotBlockText()=runBlocking<Unit> {
        connect();val file=file();state(file.mediaId,"failed");assertEquals("unsupported_kind",app.queue.find(file.mediaId)!!.error)
        val next=ManualEntries.text(app,"Independent latest text","share");state(next.mediaId,"sent")
        val before=app.queue.find(file.mediaId)!!.rejectedCapabilities;app.connection.retry();withTimeout(7000) {while(!app.connection.connected)delay(20)}
        assertEquals(before,app.queue.find(file.mediaId)!!.rejectedCapabilities);assertEquals(Long.MAX_VALUE,app.queue.find(file.mediaId)!!.nextAttempt)
    }
    @Test(timeout=30000) fun unsupportedKindIsNotRetriedOnSameCapabilities()=runBlocking<Unit> {
        connect();val file=file();state(file.mediaId,"failed");assertEquals("unsupported_kind",app.queue.find(file.mediaId)!!.error)
        val before=app.queue.find(file.mediaId)!!.revision;app.connection.retry();withTimeout(7000) {while(!app.connection.connected)delay(20)};delay(1000)
        assertEquals("failed",app.queue.find(file.mediaId)!!.state);assertEquals(before,app.queue.find(file.mediaId)!!.revision)
        assertEquals(Long.MAX_VALUE,app.queue.find(file.mediaId)!!.nextAttempt)
        val next=ManualEntries.text(app,"Text after permanent rejection","share");state(next.mediaId,"sent")
    }
    @Test(timeout=30000) fun revokedSourceStillSendsAdmittedSnapshot()=runBlocking<Unit> {
        connect();val denied=file(automatic=true,uri="content://io.github.stantonmatt.pneu.test.sources/denied");state(denied.mediaId,"sent")
        assertEquals("",app.queue.find(denied.mediaId)!!.error);assertTrue(app.connection.connected)
        val next=ManualEntries.text(app,"Text after revoked source","share");state(next.mediaId,"sent")
        val later=file();state(later.mediaId,"sent")
    }
    @Test(timeout=30000) fun unpairClosesStalledWritePromptly()=runBlocking<Unit> {
        connect();val large=file(64*1024*1024);state(large.mediaId,"sending");delay(1500)
        val start=android.os.SystemClock.elapsedRealtime();app.connection.unpair()
        assertTrue(android.os.SystemClock.elapsedRealtime()-start<2500);assertNull(app.preferences.flow.first().peer)
    }
    @Test(timeout=30000) fun helloDripCannotExtendAbsoluteDeadline()=runBlocking<Unit> {
        app.connection.start(scope);withTimeout(5000) {app.connection.state.first {it.status=="Connecting…"}}
        withTimeout(14000) {app.connection.state.first {it.status=="PC unreachable"}}
        assertFalse(app.connection.connected);assertFalse(app.preferences.flow.first().pairingLost)
    }
    @Test(timeout=30000) fun pairResultDripCannotExtendAbsoluteDeadline()=runBlocking<Unit> {
        val peer=app.preferences.flow.first().peer!!
        val raw="pneu://pair?v=1&name=Fault%20PC&fp=${peer.pin}&addr=10.0.2.2%3A${args.getString("pcPort")}&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        app.requestPairing(raw,peer.pin);app.connection.start(scope)
        withTimeout(5000) {app.connection.state.first {it.status=="Connecting…"}}
        withTimeout(14000) {app.connection.state.first {it.status=="PC unreachable"}}
        assertFalse(app.connection.connected);assertEquals(peer,app.preferences.flow.first().peer);assertNotNull(app.preferences.flow.first().pending)
    }
    @Test(timeout=30000) fun healthySessionsResetBackoffOnReadyEvenAfterEof()=runBlocking<Unit> {
        val times=mutableListOf<Long>();app.connection.start(scope)
        withTimeout(20000) {app.connection.state.map {it.status}.distinctUntilChanged().filter {it=="Connected"}.take(4).collect {times.add(android.os.SystemClock.elapsedRealtime())}}
        assertTrue(times.zipWithNext().all {(a,b)->b-a<5500})
    }
    @Test(timeout=30000) fun pauseDuringHelloIsReconciledAtReady()=runBlocking<Unit> {
        app.connection.start(scope);withTimeout(5000) {app.connection.state.first {it.status=="Connecting…"}};delay(800)
        app.preferences.toggle("paused",true);withTimeout(8000) {while(!app.connection.connected)delay(20)}
        assertTrue(app.preferences.flow.first().paused)
        // Independent peer event log asserts STATUS paused=true after its delayed HELLO.
        delay(300)
    }
}
