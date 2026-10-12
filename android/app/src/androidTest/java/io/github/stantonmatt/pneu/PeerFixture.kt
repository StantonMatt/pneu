package io.github.stantonmatt.pneu

import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.TlsIdentity
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeout
import org.junit.Assert.*
import java.io.File
import java.net.InetSocketAddress
import java.net.Socket
import com.google.gson.JsonArray
import com.google.gson.JsonParser

/** The host prepares/exports this exact identity before configuring real mutual TLS. */
internal object PeerFixture {
    suspend fun preservePreparedIdentity(app:AppGraph) {
        app.initialized.await()
        assertEquals("Prepare/export identity before starting the peer","true",InstrumentationRegistry.getArguments().getString("identityPrepared"))
        val p=app.preferences.flow.first();assertNull(p.peer);assertNull(p.pending)
        assertArrayEquals(File(app.context.filesDir,"client.der").readBytes(),TlsIdentity(p.tlsAlias).certificate.encoded)
        for(key in listOf("wifi","tailscale","mobile"))app.preferences.toggle(key,true)
        app.preferences.toggle("paused",false)
    }
    fun evidence(expires:Long?=null):JsonArray {
        val port=requireNotNull(InstrumentationRegistry.getArguments().getString("witnessPort")).toInt()
        return Socket().use {socket->
            socket.connect(InetSocketAddress("10.0.2.2",port),1000);socket.soTimeout=1000
            socket.outputStream.write((if(expires==null)"{}\n" else "{\"expires\":$expires}\n").toByteArray())
            val line=socket.inputStream.bufferedReader().readLine();check(line.length<1024*1024)
            JsonParser.parseString(line).asJsonArray
        }
    }
    suspend fun acknowledged(id:String) = withTimeout(7000) {
        while(!evidence().any {it.asJsonObject.let {row->row["event"].asString=="PHONE_ACK" && row["transfer_id"].asString==id}})delay(50)
    }
}
