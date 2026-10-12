package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.features.DeliveryPolicy
import org.junit.Assert.*
import org.junit.Test
import java.net.*
import javax.net.ssl.SSLHandshakeException

class RoundTwoPolicyTest {
    @Test fun meterednessControlsAllMobilePoliciesRegardlessOfTransport() {
        for((name,notMetered,expectedMobile) in listOf(
            Triple("Wi-Fi",true,false),
            Triple("cellular",false,true),
            Triple("VPN over Wi-Fi",true,false),
            Triple("VPN over cellular",false,true),
            Triple("metered Wi-Fi hotspot",false,true),
            Triple("missing capabilities",null,false)
        )) {
            val connectivity=Connectivity.from(notMetered)
            val mobile=connectivity.mobile
            assertEquals(name,expectedMobile,mobile)
            assertEquals(name,connectivity==Connectivity.UNMETERED,DeliveryPolicy.transport(true,true,true,connectivity,false))
            assertEquals(name,mobile,DeliveryPolicy.optimize("photo","image/png",mobile,false,false))
            assertEquals(name,mobile,DeliveryPolicy.hold("recording",true,false,mobile,false,false,false))
            assertEquals(name,if(mobile)"Mobile data" else "Wi-Fi",RoutePolicy.label(false,mobile))
            assertEquals(name,"Tailscale",RoutePolicy.label(true,mobile))
        }
    }
    private class Tcp:Socket() {
        var closed=false
        override fun connect(address:SocketAddress,timeout:Int) {}
        override fun close() {closed=true}
        override fun isClosed()=closed
        override fun setTcpNoDelay(value:Boolean) {}
    }
    @Test fun authenticationFailureClosesOnlyThatAddressAndTriesTheNext() {
        val sockets=mutableListOf<Tcp>();var authenticated=0
        val result=EndpointDialer.authenticate(List(3) {InetAddress.getLoopbackAddress()},1234,DialBudget(15000),{},create={Tcp().also {sockets+=it}}) {
            if(authenticated++==0)throw SSLHandshakeException("wrong pin")
            "pinned"
        }
        assertEquals("pinned",result);assertEquals(2,sockets.size)
        assertTrue(sockets[0].closed);assertFalse(sockets[1].closed)
    }
    @Test fun authenticationCannotResetEndpointBudgetAndCancellationStillStopsFallback() {
        var now=0L;var creates=0
        assertThrows(SocketTimeoutException::class.java) {
            EndpointDialer.authenticate(List(3) {InetAddress.getLoopbackAddress()},1234,DialBudget(15) {now},{},create={creates++;Tcp()}) {
                now=15_000_000;throw SSLHandshakeException("rejected")
            }
        }
        assertEquals(1,creates)
        assertThrows(SocketException::class.java) {
            EndpointDialer.authenticate(List(3) {InetAddress.getLoopbackAddress()},1234,DialBudget(15000),{it.close()},create={Tcp()}) {fail("Cancelled TLS")}
        }
    }
}
