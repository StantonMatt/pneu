package io.github.stantonmatt.pneu

import android.Manifest
import android.content.pm.PackageManager
import android.net.Uri
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.net.InetSocketAddress
import java.net.Socket

/** Real independent host computer, exercised in both legacy and mesh modes by
 * the canonical runner. No UI dependency and no media read/storage permission. */
@RunWith(AndroidJUnit4::class)
class ReceiveFilesDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val peer="d".repeat(32);private val owned=mutableSetOf<Uri>()
    @Before fun setup()=runBlocking<Unit> {DeviceFixture.reset();PeerFixture.preservePreparedIdentity(app)}
    @After fun cleanup()=runBlocking<Unit> {
        app.connection.stop();app.connection.awaitStopped()
        for(row in app.receivedRecent.first())owned+=Uri.parse(row.uri)
        owned.forEach {c.contentResolver.delete(it,null,null)}
        DeviceFixture.reset(preserveIdentity=false)
    }
    private fun send(name:String) {
        val port=requireNotNull(InstrumentationRegistry.getArguments().getString("witnessPort")).toInt()
        Socket().use {socket->
            socket.connect(InetSocketAddress("10.0.2.2",port),1000);socket.soTimeout=3000
            socket.outputStream.write("{\"receive_file\":\"$name\"}\n".toByteArray())
            assertTrue(socket.inputStream.bufferedReader().readLine().startsWith("["))
        }
    }
    private suspend fun received(id:String)=withTimeout(20000) {
        while(true) {
            app.queue.receivedFile(peer,id)?.takeIf {it.state=="complete"}?.let {owned+=Uri.parse(it.uri);return@withTimeout it}
            delay(25)
        }
        error("unreachable")
    }
    private fun verify(row:ReceivedFile,folder:String) {
        c.contentResolver.query(Uri.parse(row.uri),arrayOf("relative_path","is_pending","_size"),null,null,null)!!.use {
            assertTrue(it.moveToFirst());assertEquals(folder,it.getString(0));assertEquals(0,it.getInt(1));assertEquals(row.size,it.getLong(2))
        }
        assertTrue(c.contentResolver.openInputStream(Uri.parse(row.uri))!!.use {it.readBytes()}.isNotEmpty())
    }
    @Test(timeout=120000) fun computerPhotoPdfAndLostAckReplayUseOnePublishedDestination()=runBlocking<Unit> {
        for(permission in listOf(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))
            assertEquals("Receive must not need $permission",PackageManager.PERMISSION_DENIED,c.checkSelfPermission(permission))
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        val args=InstrumentationRegistry.getArguments();val pin=requireNotNull(args.getString("pcPin"));val port=requireNotNull(args.getString("pcPort"))
        val raw="pneu://pair?v=1&name=Computer%20A&fp=$pin&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        app.connection.start(app.scope)
        withTimeout(15000) {while(!app.connection.connected(peer))delay(25)}
        // The host sends its mesh settings before content; allow their ACK to settle.
        delay(500)
        val sending=async(Dispatchers.IO) {send("photo")}
        val incoming=withTimeout(10000) {app.connection.incomingTransfers.first {it[peer]?.received?.let {bytes->bytes>0}==true}}.getValue(peer)
        assertEquals("Pneu received fixture.png",incoming.fileName);assertEquals(incoming.total,incoming.received)
        assertNotNull(app.connection.state.value.incoming)
        sending.await()
        val photo=received("1".repeat(32));verify(photo,"Pictures/Pneu/")
        withTimeout(15000) {
            while(PeerFixture.evidence().count {it.asJsonObject["event"].asString=="HELLO"}<2 || !app.connection.connected(peer))delay(50)
        }
        send("photo");PeerFixture.acknowledged("1".repeat(32))
        val replay=received("1".repeat(32));assertEquals(photo.uri,replay.uri)
        c.contentResolver.query(MediaStore.Images.Media.getContentUri(MediaCatalog.VOLUME),arrayOf("_id"),"relative_path=? AND _display_name=?",arrayOf("Pictures/Pneu/",photo.fileName),null)!!.use {
            assertEquals("Lost ACK replay must not create another photo",1,it.count)
        }
        send("pdf");val pdf=received("2".repeat(32));verify(pdf,"Download/Pneu/");PeerFixture.acknowledged("2".repeat(32))
        assertEquals(2,app.receivedRecent.first().size)
        assertEquals("Test PC",photo.senderName)
        assertTrue(app.queue.pending().isEmpty())
        assertEquals("applied",app.queue.receipt(peer,photo.transferId).state)
    }
}
