package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.protocol.Endpoint
import org.junit.Assert.*
import org.junit.Test
import java.net.InetAddress
import java.net.SocketException
import java.net.SocketTimeoutException

class EndpointResolutionTest {
    @Test fun stalledDnsExpiresAndCancelsNativeLookup() {
        var now=0L;var cancelled=false
        val budget=DialBudget(5) {now}
        assertThrows(SocketTimeoutException::class.java) {
            EndpointResolution.await(budget,{false}) {now=5_000_000;{cancelled=true}}
        }
        assertTrue(cancelled)
    }
    @Test fun socketCancellationInterruptsDnsWithoutWaitingForDeadline() {
        var closed=false;var cancelled=false
        assertThrows(SocketException::class.java) {
            EndpointResolution.await(DialBudget(5000),{closed}) {closed=true;{cancelled=true}}
        }
        assertTrue(cancelled)
    }
    @Test fun resolutionConsumesTheTcpBudgetAndLateAnswerIsRejected() {
        var now=0L;val budget=DialBudget(5000) {now}
        val literal=InetAddress.getByAddress(byteArrayOf(127,0,0,1))
        assertEquals(listOf(literal),EndpointResolution.await(budget,{false}) {callback->now=4_000_000_000;callback(Result.success(listOf(literal)));{}})
        assertEquals(1000,budget.remainingMillis())
        now=5_000_000_000;assertThrows(SocketTimeoutException::class.java) {budget.remainingMillis()}
    }
    @Test fun failedAndEmptyLookupsCancelAndNeverReachTcp() {
        var cancels=0
        for(answer in listOf(Result.failure<List<InetAddress>>(java.net.UnknownHostException()),Result.success(emptyList()))) {
            assertThrows(java.net.UnknownHostException::class.java) {
                EndpointResolution.await(DialBudget(5000),{false}) {complete->complete(answer);{cancels++}}
            }
        }
        assertEquals(2,cancels)
    }
    @Test fun completedAnswerCannotExtendTheEndpointBudget() {
        var now=0L;var cancelled=false;val budget=DialBudget(5000) {now}
        assertThrows(SocketTimeoutException::class.java) {
            EndpointResolution.await(budget,{false}) {complete->now=5_000_000_000;complete(Result.success(listOf(InetAddress.getLoopbackAddress())));{cancelled=true}}
        }
        assertTrue(cancelled)
    }
    @Test fun hostnameLabelsNeverBecomeTailscaleLiterals() {
        for(host in listOf("100.pc.67.144.28","100.64.name.1.2","100.64.1.2.example","100.064.1.2","100.64.1","100.128.1.2","fd7a:115c:a1e00::1"))assertFalse(host,Endpoint(host,1234).tailscale)
        for(host in listOf("100.64.1.2","100.127.255.255","::ffff:100.67.144.28","0:0:0:0:0:ffff:6443:901c","fd7a:115c:a1e0::1","FD7A:115C:A1E0:0:0:0:0:1"))assertTrue(host,Endpoint(host,1234).tailscale)
        assertNull(Endpoint.literalAddress("100.pc.67.144.28"))
    }
}
