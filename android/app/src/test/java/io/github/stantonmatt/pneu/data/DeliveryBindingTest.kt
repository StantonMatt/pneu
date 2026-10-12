package io.github.stantonmatt.pneu.data

import org.junit.Assert.*
import org.junit.Test

class DeliveryBindingTest {
    @Test fun admissionRequiresTheSamePcAndPhoneIdentityUntilCommit() {
        val p=Prefs(deviceId="phone",peer=Peer("PC","pin","pc",emptyList()))
        val binding=DeliveryBinding.from(p)!!
        assertTrue(binding.matches(p))
        assertFalse(binding.matches(p.copy(loaded=false)))
        assertNull(DeliveryBinding.from(p.copy(loaded=false)))
        assertFalse(binding.matches(p.copy(peer=p.peer!!.copy(pin="new-pin"))))
        assertFalse(binding.matches(p.copy(peer=p.peer!!.copy(deviceId="new-pc"))))
        assertFalse(binding.matches(p.copy(deviceId="repaired-same-pc")))
        assertFalse(binding.matches(p.copy(peer=null)))
        assertFalse(binding.matches(p.copy(deliveryReset=true)))
        assertFalse(binding.matches(p.copy(pairingLost=true)))
        assertNull(DeliveryBinding.from(Prefs()))
    }
}
