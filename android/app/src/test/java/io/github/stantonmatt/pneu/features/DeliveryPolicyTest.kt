package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.connection.Connectivity.*
import org.junit.Assert.*
import org.junit.Test

class DeliveryPolicyTest {
    @Test fun automaticSkipAndManualPause() {
        assertTrue(DeliveryPolicy.skipAutomatic(true,true,true,true,UNMETERED,true))
        assertTrue(DeliveryPolicy.skipAutomatic(false,false,true,true,UNMETERED,true))
        assertTrue(DeliveryPolicy.skipAutomatic(false,true,false,false,NONE,false))
        assertFalse(DeliveryPolicy.skipAutomatic(false,true,false,false,METERED,false))
        assertTrue(DeliveryPolicy.hold("photo",true,true,false,false,false,false))
        assertFalse(DeliveryPolicy.hold("file",false,true,false,false,false,false))
        assertFalse(DeliveryPolicy.hold("text",false,true,false,false,false,false))
    }
    @Test fun mobilePolicyAndImmutableReplay() {
        assertTrue(DeliveryPolicy.optimize("photo","image/png",true,false,false))
        assertFalse(DeliveryPolicy.optimize("photo","image/jpeg",true,false,false,true))
        assertFalse(DeliveryPolicy.optimize("file","image/png",true,false,false))
        assertFalse(DeliveryPolicy.optimize("photo","image/png",true,false,true))
        assertTrue(DeliveryPolicy.hold("photo",true,false,true,false,true,false))
        assertFalse(DeliveryPolicy.hold("photo",true,false,true,false,true,true))
        assertTrue(DeliveryPolicy.hold("recording",true,false,true,false,false,false))
        assertFalse(DeliveryPolicy.hold("recording",true,false,false,false,false,false))
        assertFalse(DeliveryPolicy.hold("recording",true,false,true,true,false,false))
    }
    @Test fun ownedEchoAllowsDeliberateRecopy() {
        assertTrue(DeliveryPolicy.ownedEcho(true,"abc","abc"))
        assertFalse(DeliveryPolicy.ownedEcho(false,"abc","abc"))
        assertFalse(DeliveryPolicy.ownedEcho(true,"new","abc"))
    }
    @Test fun lastWriterWinsWithoutBarriers() {
        assertTrue(DeliveryPolicy.newer(1,-1))
        assertTrue(DeliveryPolicy.newer(11,10))
        assertFalse(DeliveryPolicy.newer(10,10))
        assertFalse(DeliveryPolicy.newer(9,10))
    }
}
