package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.features.DeliveryPolicy
import org.junit.Assert.*
import org.junit.Test

class TwentySecondPolicyTest {
    @Test fun offlineAdmissionUsesQueueSettingEvenWhenMobileIsDisabledOrLinkStateLags() {
        for(network in Connectivity.entries)for(queue in listOf(false,true))for(mobile in listOf(false,true))for(connected in listOf(false,true)) {
            val expected= !queue && (network==Connectivity.NONE || !connected) && !(network==Connectivity.METERED && !mobile)
            assertEquals("$network queue=$queue mobile=$mobile connected=$connected",expected,
                DeliveryPolicy.skipAutomatic(false,true,queue,connected,network,mobile))
            assertTrue(DeliveryPolicy.skipAutomatic(true,true,queue,connected,network,mobile))
            assertTrue(DeliveryPolicy.skipAutomatic(false,false,queue,connected,network,mobile))
        }
    }
    @Test fun routeCostAndAvailabilityAreIndependentOfEndpointTransport() {
        assertEquals(Connectivity.NONE,Connectivity.from(null))
        assertEquals(Connectivity.METERED,Connectivity.from(false))
        assertEquals(Connectivity.UNMETERED,Connectivity.from(true))
        for(network in Connectivity.entries)for(tailscaleRoute in listOf(false,true))for(wifi in listOf(false,true))for(tailscale in listOf(false,true))for(mobile in listOf(false,true)) {
            val expected=network!=Connectivity.NONE && (network==Connectivity.UNMETERED || mobile) && (if(tailscaleRoute)tailscale else wifi)
            assertEquals(expected,DeliveryPolicy.transport(tailscaleRoute,wifi,tailscale,network,mobile))
            assertEquals(network==Connectivity.METERED,DeliveryPolicy.optimize("photo","image/jpeg",network.mobile,false,false))
            assertEquals(network==Connectivity.METERED,DeliveryPolicy.hold("recording",true,false,network.mobile,false,false,false))
        }
    }
}
