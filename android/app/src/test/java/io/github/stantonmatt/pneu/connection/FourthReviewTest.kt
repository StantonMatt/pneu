package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.features.PermissionPolicy
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import java.net.*
import java.io.IOException

class FourthReviewTest {
    @Test fun localErrorsHaveRowDeadlinesAndDoNotBecomeTransportFailures()=runBlocking<Unit> {
        for(code in listOf("source_read","local_read","local_storage")) {
            try {SendFailures.local(code) {throw IOException("fixture")};fail()}
            catch(e:LocalSendFailure) {assertEquals(code,SendFailures.code(e));assertEquals(60000,SendFailures.retryAfter(code));assertFalse(e.cause is TransportFailure)}
        }
        assertEquals(Long.MAX_VALUE,SendFailures.retryAfter("invalid_payload"))
        assertThrows(CancellationException::class.java) {runBlocking {SendFailures.local("source_read") {throw CancellationException()}}}
    }
    private class FakeSocket(val dial:(Int)->Unit):Socket() {
        var closed=false
        override fun connect(address:SocketAddress,timeout:Int)=dial(timeout)
        override fun close() {closed=true}
        override fun isClosed()=closed
        override fun setTcpNoDelay(value:Boolean) {}
    }
    @Test fun everyResolvedAddressSharesOneBudgetAndFreshSocket() {
        var now=0L;val calls=mutableListOf<Int>();val failed=FakeSocket {calls+=it;now=2_000_000_000;throw ConnectException()}
        val good=FakeSocket {calls+=it};val sockets=ArrayDeque(listOf(failed,good))
        val address=InetAddress.getLoopbackAddress()
        assertSame(good,EndpointDialer.connect(listOf(address,address),1234,DialBudget(5000) {now},{}, {sockets.removeFirst()}))
        assertTrue(failed.closed);assertFalse(good.closed);assertEquals(listOf(5000,3000),calls)
    }
    @Test fun exhaustionAndPolicyCancellationStopRemainingAddresses() {
        var now=0L;var created=0
        assertThrows(SocketTimeoutException::class.java) {EndpointDialer.connect(List(3) {InetAddress.getLoopbackAddress()},1234,DialBudget(5000) {now},{},{created++;FakeSocket {now=5_000_000_000;throw ConnectException()}})}
        assertEquals(1,created)
        val closed=FakeSocket {fail("Connected cancelled socket")}
        assertThrows(SocketException::class.java) {EndpointDialer.connect(listOf(InetAddress.getLoopbackAddress()),1234,DialBudget(5000),{it.close()},{closed})}
    }
    @Test fun settingsRecoveryDistinguishesFirstRequestFromPermanentDenial() {
        assertFalse(PermissionPolicy.settings(false,false));assertFalse(PermissionPolicy.settings(true,true));assertTrue(PermissionPolicy.settings(true,false))
        assertEquals(5000,PermissionPolicy.acknowledgedAt(1000,5000));assertEquals(7000,PermissionPolicy.acknowledgedAt(7000,5000))
    }
}
