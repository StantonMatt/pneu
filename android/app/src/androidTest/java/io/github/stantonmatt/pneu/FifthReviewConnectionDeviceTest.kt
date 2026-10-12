package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.features.ManualEntries
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.net.Socket
import java.net.InetSocketAddress

@RunWith(AndroidJUnit4::class)
class FifthReviewConnectionDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    @Test(timeout=45000) fun eofAndCancellationSupersedeInterruptedTextBeforeReconnect()=runBlocking<Unit> {
        val i=InstrumentationRegistry.getInstrumentation();val c=i.targetContext;val app=c.graph;val args=InstrumentationRegistry.getArguments()
        val port=requireNotNull(args.getString("pcPort"));val pin=requireNotNull(args.getString("pcPin"));val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        suspend fun sent(id:String)=withTimeout(12000) {while(app.queue.find(id)?.state!="sent")delay(20)}
        suspend fun observed(id:String)=withTimeout(8000) {while(!PeerFixture.evidence().any {it.asJsonObject.let {r->r["event"].asString=="TEXT" && r["transfer_id"].asString==id}})delay(20)}
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();delay(250);PeerFixture.preservePreparedIdentity(app)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        try {
            val raw="pneu://pair?v=1&name=Text%20PC&fp=$pin&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
            assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)));app.connection.start(scope)
            withTimeout(10000) {while(!app.connection.connected)delay(20)}
            val old=ManualEntries.text(app,"EOF older value","share");observed(old.transferId)
            val latest=ManualEntries.text(app,"EOF latest value","share")
            assertEquals("sending",app.queue.find(old.mediaId)!!.state)
            Socket().use {s->s.connect(InetSocketAddress("10.0.2.2",requireNotNull(args.getString("witnessPort")).toInt()),1000);s.soTimeout=1000;s.outputStream.write("{\"close_text\":true}\n".toByteArray());s.inputStream.bufferedReader().readLine()}
            sent(latest.mediaId);assertEquals("superseded",app.queue.find(old.mediaId)!!.state)
            val cancelled=ManualEntries.text(app,"Cancelled older value","share");observed(cancelled.transferId)
            val newest=ManualEntries.text(app,"Cancelled latest value","share")
            assertEquals("sending",app.queue.find(cancelled.mediaId)!!.state)
            app.connection.stop() // CancellationException, not a transport wrapper.
            withTimeout(5000) {while(app.queue.find(cancelled.mediaId)?.state!="superseded")delay(20)}
            app.connection.start(scope);sent(newest.mediaId)
            val texts=PeerFixture.evidence().filter {it.asJsonObject["event"].asString=="TEXT"}.map {it.asJsonObject["transfer_id"].asString}
            assertEquals(listOf(old.transferId,latest.transferId,cancelled.transferId,newest.transferId),texts)
            assertEquals(listOf("superseded","sent","superseded","sent"),listOf(old,latest,cancelled,newest).map {app.queue.find(it.mediaId)!!.state})
        } finally {app.connection.stop();app.forgetBinding();scope.cancel()}
    }
}
