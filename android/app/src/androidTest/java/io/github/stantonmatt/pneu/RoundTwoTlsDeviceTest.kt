package io.github.stantonmatt.pneu

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.flow.first
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class RoundTwoTlsDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    private fun fallback(argument:String)=runBlocking<Unit> {
        val i=InstrumentationRegistry.getInstrumentation();val app=i.targetContext.graph;val args=InstrumentationRegistry.getArguments()
        app.initialized.await();PeerFixture.preservePreparedIdentity(app)
        val port=requireNotNull(args.getString(argument)).toInt();val pin=requireNotNull(args.getString("pcPin"))
        var sockets=0;val verified=mutableListOf<Boolean>()
        // Two resolver alternatives share a controlled listener: its first
        // accept rejects authentication; the second serves the pinned PC.
        TlsIdentity(app.preferences.flow.first().tlsAlias).socketWithResolver(Endpoint("fixture.invalid",port),pin,{verified+=it},{sockets++}) {_,_->
            List(2) {Endpoint.literalAddress("10.0.2.2")!!}
        }.use {tls->
            assertEquals("TLSv1.3",tls.session.protocol);assertTrue(verified.last())
            Protocol.write(tls.outputStream,Frame.Control(Type.HELLO,json("version" to 1,"device_id" to app.prefs.value.deviceId,"name" to "Fallback fixture","capabilities" to emptyList<String>())))
            assertEquals(Type.HELLO,(Protocol.read(tls.inputStream) as Frame.Control).type)
            Protocol.write(tls.outputStream,Frame.Control(Type.ERROR,json("code" to "closing")))
        }
        assertEquals(5,sockets);assertEquals(listOf(false,false,true),verified)
    }
    @Test(timeout=30000) fun wrongPinnedLeafFallsBackToVerifiedAddress()=fallback("wrongPort")
    @Test(timeout=30000) fun tcpListenerWithoutTlsFallsBackToVerifiedAddress()=fallback("plainPort")
}
