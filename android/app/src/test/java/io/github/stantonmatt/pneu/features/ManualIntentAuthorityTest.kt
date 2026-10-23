package io.github.stantonmatt.pneu.features

import android.app.Application
import android.content.Intent
import io.github.stantonmatt.pneu.data.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ManualIntentAuthorityTest {
    private val p=Prefs(deviceId="phone",peers=listOf(Peer("A","pin","pc",emptyList(),generation=12)))
    @Test fun internalActionsRoundTripGenerationAndHaveDistinctPendingIntentIdentityAfterRepair() {
        val old=ManualEntries.bind(Intent("clipboard"),p)
        val repaired=p.copy(peers=p.peers.map {it.copy(generation=13)})
        val new=ManualEntries.bind(Intent("clipboard"),repaired)
        assertEquals(DeliveryBinding.from(p),ManualEntries.admission(old,repaired))
        assertFalse(ManualEntries.admission(old,repaired)!!.matches(repaired))
        assertNotEquals(old.identifier,new.identifier);assertFalse(old.filterEquals(new))
        assertTrue(ManualEntries.admission(new,repaired)!!.matches(repaired))
    }
    @Test fun oldGenerationlessOrUnpairedActionsCannotAcquireCurrentAuthority() {
        val legacy=Intent().putExtra("pneu.admission.phone","phone").putExtra("pneu.admission.pin","pin").putExtra("pneu.admission.pc","pc")
        assertNull(ManualEntries.admission(legacy,p))
        val unpaired=ManualEntries.bind(Intent(),Prefs())
        assertNull(ManualEntries.admission(unpaired,p))
    }
    @Test fun disabledLegacyClipboardKeepsAckAndIgnoreWhileMeshReportsTheDisabledDirection() {
        val legacy=ClipboardDelivery.disabledDirection("a".repeat(32),false)
        assertEquals(io.github.stantonmatt.pneu.protocol.Type.ACK,legacy.type)
        val mesh=ClipboardDelivery.disabledDirection("a".repeat(32),true)
        assertEquals(io.github.stantonmatt.pneu.protocol.Type.NACK,mesh.type)
        assertEquals("unsupported_kind",mesh.body["code"].asString);assertFalse(mesh.body["retryable"].asBoolean)
    }

}
