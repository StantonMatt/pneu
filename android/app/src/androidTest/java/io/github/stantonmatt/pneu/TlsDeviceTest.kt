package io.github.stantonmatt.pneu

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import kotlinx.coroutines.flow.first
import org.junit.Test
import org.junit.runner.RunWith

/** Run only with the independent TLS peer; no production daemon is required. */
@RunWith(AndroidJUnit4::class)
class TlsDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    @Test fun exportPublicClientLeaf() {
        val context=InstrumentationRegistry.getInstrumentation().targetContext
        val cert=kotlinx.coroutines.runBlocking {
            context.graph.initialized.await()
            TlsIdentity(context.graph.preferences.flow.first().tlsAlias).certificate
        }
        assertEquals("EC",cert.publicKey.algorithm)
        java.io.File(context.filesDir,"client.der").writeBytes(cert.encoded)
    }
    @Test fun keystoreMutualTlsHandshake() {
        val args=InstrumentationRegistry.getArguments()
        org.junit.Assume.assumeTrue("Requires a real TLS test peer and pcPin",args.containsKey("pcPin"))
        val pin=args.getString("pcPin")!!
        val host=args.getString("pcHost") ?: "10.0.2.2"
        val port=args.getString("pcPort")?.toInt() ?: 47825
        TlsIdentity(kotlinx.coroutines.runBlocking {val app=InstrumentationRegistry.getInstrumentation().targetContext.graph;app.initialized.await();app.preferences.flow.first().tlsAlias}).socket(Endpoint(host,port),pin) {}.use { socket ->
            assertEquals("TLSv1.3",socket.session.protocol)
            assertEquals(1,socket.session.localCertificates.size)
            Protocol.write(socket.outputStream,Frame.Control(Type.HELLO,json("version" to 1,"device_id" to "1".repeat(32),"name" to "TLS test","capabilities" to emptyList<String>())))
            assertEquals(Type.HELLO,(Protocol.read(socket.inputStream) as Frame.Control).type)
            Protocol.write(socket.outputStream,Frame.Control(Type.ERROR,json("code" to "closing")))
        }
    }
    @Test fun rejectedHandshake() {
        val args=InstrumentationRegistry.getArguments()
        org.junit.Assume.assumeTrue("Requires rejectPort fault peer",args.containsKey("rejectPort"))
        val pin=requireNotNull(args.getString("pcPin"))
        val encoded=android.util.Base64.decode(requireNotNull(args.getString("pcCert")),android.util.Base64.DEFAULT)
        val cert=java.security.cert.CertificateFactory.getInstance("X.509").generateCertificate(encoded.inputStream()) as java.security.cert.X509Certificate
        io.github.stantonmatt.pneu.connection.PinnedTrustManager(pin).checkServerTrusted(arrayOf(cert),"EC")
        val started=android.os.SystemClock.elapsedRealtime()
        assertThrows(javax.net.ssl.SSLHandshakeException::class.java) {TlsIdentity(kotlinx.coroutines.runBlocking {val app=InstrumentationRegistry.getInstrumentation().targetContext.graph;app.initialized.await();app.preferences.flow.first().tlsAlias}).socket(Endpoint(args.getString("pcHost") ?: "10.0.2.2",args.getString("rejectPort")!!.toInt()),pin) {}.close()}
        assertTrue(android.os.SystemClock.elapsedRealtime()-started<15000)
    }
}
