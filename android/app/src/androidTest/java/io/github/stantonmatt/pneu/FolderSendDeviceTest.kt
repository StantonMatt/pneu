package io.github.stantonmatt.pneu

import android.Manifest
import android.net.Uri
import android.provider.DocumentsContract
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream

@RunWith(AndroidJUnit4::class)
class FolderSendDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val app=i.targetContext.graph
    private val pc="a".repeat(32)
    private val uri=DocumentsContract.buildTreeDocumentUri("io.github.stantonmatt.pneu.test.folders","root")
    private var fixture:LocalPeerFixture?=null
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset(preserveIdentity=false)
        i.uiAutomation.grantRuntimePermission(i.targetContext.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        i.uiAutomation.grantRuntimePermission(i.targetContext.packageName,Manifest.permission.POST_NOTIFICATIONS)
        app.context.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folder-grants"),"grant",null,null)
        app.context.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folders"),"fixture","abc",null)
    }
    @After fun cleanup()=runBlocking {
        app.connection.stop();app.connection.awaitStopped();fixture?.close()
        app.context.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folder-grants"),"revoke",null,null)
        DeviceFixture.reset(preserveIdentity=false)
    }
    private suspend fun connect(handle:(Frame.Control,(Frame.Control)->Unit)->Boolean):LocalPeerFixture {
        val f=LocalPeerFixture(app,setOf("files",CAP_FILE_RESUME,CAP_FOLDER),handle=handle);fixture=f
        assertTrue(app.requestPairing(f.invitation(pc,"Folder PC"),ConfirmedBinding.from(app.preferences.read())))
        app.connection.start(app.scope)
        withTimeout(15000) {while(!app.connection.connected(pc) || !app.connection.devices.value[pc]?.capabilities.orEmpty().containsAll(setOf(CAP_FILE_RESUME,CAP_FOLDER)))delay(20)}
        return f
    }
    private fun authority()=CapturedAuthority(requireNotNull(app.connection.freezeSession(requireNotNull(DeliveryBinding.from(app.prefs.value,pc)))))
    @Test(timeout=90000) fun safTreeSendsExactVectorFromResumedOffset()=runBlocking<Unit>(Dispatchers.IO) {
        val complete=CompletableDeferred<Unit>();val expected=("{\"entries\":[{\"path\":\"empty\",\"type\":\"directory\"},{\"path\":\"a.txt\",\"type\":\"file\",\"size\":3,\"sha256\":\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\"}]}"+"abc").toByteArray()
        val output=ByteArrayOutputStream().apply {write(expected,0,167)}
        var begin:Frame.Control?=null
        val f=connect {frame,send->when(frame.type) {
            Type.FILE_BEGIN->{begin=frame;assertEquals(165L,frame.body["folder"].asJsonObject["manifest_len"].asLong);assertEquals(168L,frame.body["size"].asLong)
                assertFalse(frame.body.has("source_media_id"));assertFalse(frame.body.has("hlc"))
                send(Frame.Control(Type.FILE_READY,json("transfer_id" to frame.body["transfer_id"].asString,"offset" to 167)));true}
            Type.FILE_END->{
                assertArrayEquals(expected,output.toByteArray());assertEquals(begin!!.body["sha256"].asString,sha256(output.toByteArray()))
                FolderManifest.parse(output.toByteArray().copyOf(165),begin!!.body)
                send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)));complete.complete(Unit);true}
            else->false
        }}
        f.observeFrame={frame->if(frame is Frame.Chunk) {assertEquals(output.size().toLong(),frame.offset);output.write(frame.data)}}
        app.folderSends.send(uri,authority())
        withTimeout(15000) {complete.await()}
        withTimeout(10000) {while(app.queue.recentDeliveries().first().none {it.isFolder() && it.state=="sent"})delay(20)}
        assertTrue(app.folderSends.preparing.value.isEmpty())
        withTimeout(10000) {
            val manager=app.context.getSystemService(android.app.NotificationManager::class.java)
            while(manager.activeNotifications.none {it.tag=="folder:${begin!!.body["transfer_id"].asString}:$pc" && it.notification.extras.getCharSequence(android.app.Notification.EXTRA_TEXT)?.toString()=="1 item couldn't be sent: locked.txt\n2 links and special files skipped"})delay(20)
        }
    }
    @Test(timeout=90000) fun changedAfterPreparingAtZeroCancelsBeforeEnd()=runBlocking<Unit>(Dispatchers.IO) {
        val began=Channel<Frame.Control>(1);val cancelled=CompletableDeferred<Unit>();var chunks=0;var ends=0
        val f=connect {frame,_->when(frame.type) {
            Type.FILE_BEGIN->{began.trySend(frame);true}
            Type.CANCEL->{cancelled.complete(Unit);true}
            Type.FILE_END->{ends++;true}
            else->false
        }}
        f.observeFrame={if(it is Frame.Chunk)chunks++}
        app.folderSends.send(uri,authority())
        val begin=withTimeout(15000) {began.receive()}
        app.context.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folders"),"fixture","xyz",null)
        f.sendControl(Frame.Control(Type.FILE_READY,json("transfer_id" to begin.body["transfer_id"].asString,"offset" to 0)))
        withTimeout(15000) {cancelled.await();while(app.queue.recentDeliveries().first().none {it.error=="folder_changed"})delay(20)}
        assertTrue(chunks>0);assertEquals(0,ends)
    }
    @Test(timeout=90000) fun pipeBackedDocumentSendsAndPersistableGrantIsReleased()=runBlocking<Unit>(Dispatchers.IO) {
        app.context.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folders"),"pipe-backed",null,null)
        val began=Channel<Frame.Control>(1);val complete=CompletableDeferred<Unit>();val output=ByteArrayOutputStream()
        val f=connect {frame,send->when(frame.type) {
            Type.FILE_BEGIN->{began.trySend(frame);true}
            Type.FILE_END->{assertEquals("abc",String(output.toByteArray().takeLast(3).toByteArray()))
                assertEquals(sha256(output.toByteArray()),app.queue.pending().first().hash)
                send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)));complete.complete(Unit);true}
            else->false
        }}
        f.observeFrame={if(it is Frame.Chunk)output.write(it.data)}
        app.folderSends.send(uri,authority())
        val begin=withTimeout(15000) {began.receive()}
        assertTrue(app.context.contentResolver.persistedUriPermissions.any {it.uri==uri && it.isReadPermission})
        f.sendControl(Frame.Control(Type.FILE_READY,json("transfer_id" to begin.body["transfer_id"].asString,"offset" to 0)))
        withTimeout(15000) {complete.await();while(app.context.contentResolver.persistedUriPermissions.any {it.uri==uri})delay(20)}
    }

    @Test(timeout=90000) fun offlineFolderQueuesThenOldPeerFailsAndReleasesGrant()=runBlocking<Unit>(Dispatchers.IO) {
        val f=LocalPeerFixture(app,setOf("files",CAP_FILE_RESUME));fixture=f
        assertTrue(app.requestPairing(f.invitation(pc,"Old PC"),ConfirmedBinding.from(app.preferences.read())))
        // Pairing is committed only after the authenticated connection, not
        // when requestPairing merely accepts the invitation.
        app.connection.start(app.scope)
        withTimeout(15000) {while(!app.connection.connected(pc))delay(20)}
        app.connection.stop();app.connection.awaitStopped()
        val binding=requireNotNull(DeliveryBinding.from(app.preferences.read(),pc))
        app.folderSends.send(uri,CapturedAuthority(binding))
        assertEquals("waiting",app.queue.pending().single().state)
        assertTrue(app.context.contentResolver.persistedUriPermissions.any {it.uri==uri})
        app.connection.start(app.scope)
        withTimeout(15000) {while(app.queue.pending().none {it.error=="folder_update_required"})delay(20)
            while(app.context.contentResolver.persistedUriPermissions.any {it.uri==uri})delay(20)}
    }

    @Test(timeout=90000) fun pipeResumeRetriesFromZeroWithoutResettingConnection()=runBlocking<Unit>(Dispatchers.IO) {
        app.context.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folders"),"pipe-backed",null,null)
        val first=CompletableDeferred<Unit>();val complete=CompletableDeferred<Unit>();val output=ByteArrayOutputStream()
        var begins=0;var ends=0;var cancels=0
        val f=connect {frame,send->when(frame.type) {
            Type.FILE_BEGIN->{begins++;send(Frame.Control(Type.FILE_READY,json("transfer_id" to frame.body["transfer_id"].asString,"offset" to if(begins==1)167 else 0)));true}
            Type.FILE_END->{ends++
                if(ends==1) {assertEquals(0,output.size());send(Frame.Control(Type.NACK,json("transfer_id" to frame.body["transfer_id"].asString,"code" to "size_mismatch","retryable" to true)));first.complete(Unit)}
                else {assertEquals("abc",String(output.toByteArray().takeLast(3).toByteArray()));send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)));complete.complete(Unit)}
                true}
            Type.CANCEL->{cancels++;true}
            else->false
        }}
        f.observeFrame={if(it is Frame.Chunk)output.write(it.data)}
        app.folderSends.send(uri,authority())
        withTimeout(15000) {first.await();while(app.queue.pending().none {it.error=="local_read"})delay(20)}
        assertTrue(app.connection.connected(pc))
        assertTrue(app.retry(app.queue.pending().single().mediaId))
        withTimeout(15000) {complete.await()}
        assertEquals(2,begins);assertEquals(0,cancels)
    }

    @Test fun skippedDirectoriesUseItemCopyAndSummariesUseContentBytes() {
        val skipped=FolderSkipped(unreadable=1,names=listOf("locked.txt"),directories=1,directoryNames=listOf("private"))
        assertEquals("2 items couldn't be sent: locked.txt and private",skipped.notification(app.context))
        assertEquals(2,skipped.count)
        assertEquals("1 item, 3 B · now",FolderSummary(165,1,FolderSkipped()).sent(app.context,"now","3 B"))
        assertEquals("To PC · ",app.context.getString(R.string.folder_to,"PC"))
    }

}
