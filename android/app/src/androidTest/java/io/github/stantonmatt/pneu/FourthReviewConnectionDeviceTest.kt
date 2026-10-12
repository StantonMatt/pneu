package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.io.FileInputStream
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class FourthReviewConnectionDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
    private val provider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
    private fun control(command:String)=c.contentResolver.call(provider,command,null,null)!!
    private suspend fun state(id:String,value:String)=withTimeout(20000) {while(app.queue.find(id)?.state!=value)delay(20)}
    private suspend fun file(source:String="",bytes:Int=8,admit:Boolean=true):Outgoing {
        val row=Outgoing().apply {mediaId=if(source.isEmpty())randomId() else "external_primary:${android.provider.MediaStore.getVersion(c,"external_primary")}:images:${System.nanoTime()}";transferId=randomId();kind="file";uri=source;automatic=source.isNotEmpty();mime="application/octet-stream";fileName="Fourth review fixture.bin";createdAt=System.currentTimeMillis()}
        val path=File(c.filesDir,"outgoing/${row.transferId}.bin").apply {parentFile!!.mkdirs();writeBytes(ByteArray(bytes))}
        row.path=path.path;row.size=bytes.toLong();row.hash=sha256(ByteArray(bytes));if(admit)assertTrue(app.enqueueFixture(row));return row
    }
    @Test(timeout=90000) fun resolvedFallbackAndLocalFailuresPreserveBothLanes()=runBlocking<Unit> {
        val args=InstrumentationRegistry.getArguments();val pin=requireNotNull(args.getString("pcPin"));val port=requireNotNull(args.getString("pcPort")).toInt()
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();delay(300);PeerFixture.preservePreparedIdentity(app)
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        val poolRelease=CountDownLatch(1)
        try {
            // First resolved IP is refused; second succeeds with the same exported leaf and pin.
            var sockets=0
            TlsIdentity(app.preferences.flow.first().tlsAlias).socketWithResolver(Endpoint("fixture.invalid",port),pin,register={sockets++}) {_,_->listOf(Endpoint.literalAddress("127.0.0.2")!!,Endpoint.literalAddress("10.0.2.2")!!)}.use {tls->
                assertEquals("TLSv1.3",tls.session.protocol)
                Protocol.write(tls.outputStream,Frame.Control(Type.HELLO,json("version" to 1,"device_id" to app.prefs.value.deviceId,"name" to "DNS fixture","capabilities" to emptyList<String>())))
                assertEquals(Type.HELLO,(Protocol.read(tls.inputStream) as Frame.Control).type)
                Protocol.write(tls.outputStream,Frame.Control(Type.ERROR,json("code" to "closing")))
            }
            assertEquals(4,sockets) // DNS marker, refused TCP, connected TCP, verified TLS
            val raw="pneu://pair?v=1&name=Fourth%20PC&fp=$pin&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
            assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
            app.preferences.toggle("originals",true);app.connection.start(scope)
            withTimeout(12000) {while(!app.connection.connected)delay(20)}
            control("reset")
            // Delivery reads admitted private bytes, never the provider again.
            val snapshot=file(provider.buildUpon().appendPath("slow-open").build().toString())
            state(snapshot.mediaId,"sent");val later=file();state(later.mediaId,"sent")
            val text=ManualEntries.text(app,"Text alongside immutable snapshots","share");state(text.mediaId,"sent")
            for(collection in listOf("images","video")) {
                val obsolete=file(provider.buildUpon().appendPath("image").build().toString(),admit=false)
                obsolete.mediaId="external_primary:obsolete-${randomId()}:$collection:42"
                assertTrue(app.enqueueFixture(obsolete));state(obsolete.mediaId,"sent")
            }
            val current=file(provider.buildUpon().appendPath("image").build().toString());state(current.mediaId,"sent")
            assertEquals(0,control("stats").getInt("opens"));assertTrue(app.connection.connected)
            val unreadable=file(admit=false);File(unreadable.path).delete();File(unreadable.path).mkdirs();unreadable.size=File(unreadable.path).length();assertTrue(app.enqueueFixture(unreadable))
            state(unreadable.mediaId,"failed");assertEquals("local_read",app.queue.find(unreadable.mediaId)!!.error)
            val partial=file(bytes=2*CHUNK_SIZE,admit=false)
            app.connection.openPayload={path->if(path.path!=partial.path)path.inputStream() else object:FileInputStream(path) {
                var hashed=false;var chunks=0
                override fun read(buffer:ByteArray):Int=super.read(buffer).also {if(it<0)hashed=true}
                override fun read(buffer:ByteArray,offset:Int,length:Int):Int {
                    if(hashed && chunks++>0)throw java.io.IOException("Synthetic local read failure")
                    return super.read(buffer,offset,length)
                }
            }}
            assertTrue(app.enqueueFixture(partial));state(partial.mediaId,"failed");assertEquals("local_read",app.queue.find(partial.mediaId)!!.error)
            val next=file();state(next.mediaId,"sent")
            val invalid=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="text";this.text="\ud800";mime="text/plain";createdAt=System.currentTimeMillis()}
            assertTrue(app.enqueueFixture(invalid));state(invalid.mediaId,"failed");assertEquals("invalid_payload",app.queue.find(invalid.mediaId)!!.error)
            val last=ManualEntries.text(app,"Text after local failures","share");state(last.mediaId,"sent");val lastFile=file();state(lastFile.mediaId,"sent");assertTrue(app.connection.connected)
        } finally {
            poolRelease.countDown();control("release");app.connection.openPayload={it.inputStream()};app.connection.stop();app.forgetBinding();scope.cancel()
        }
    }
}
