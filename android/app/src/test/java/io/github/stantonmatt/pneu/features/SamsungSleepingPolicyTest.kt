package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.Health
import org.junit.Assert.*
import org.junit.Test

class SamsungSleepingPolicyTest {
    private fun health(exempt:Boolean,acknowledged:Boolean=false,bucketAllowed:Boolean=true,killed:Boolean=false)=Health(
        photos=true,notifications=true,battery=exempt,
        sleeping=PermissionPolicy.sleeping(acknowledged,bucketAllowed,killed,exempt),killed=killed
    )

    @Test fun exemptionSatisfiesSetupWithoutVisitingSleepingPicker() {
        // Neither a saved acknowledgement nor a favourable bucket is needed on One UI.
        for(bucketAllowed in listOf(false,true)) {
            val checked=health(exempt=true,bucketAllowed=bucketAllowed)
            assertTrue(checked.sleeping)
            assertTrue(checked.issues.isEmpty())
        }
    }

    @Test fun revocationRestoresSetupAndHomeIssuesWithoutChangingPreferences() {
        assertTrue(health(exempt=true).sleeping)
        val revoked=health(exempt=false)
        assertFalse(revoked.sleeping)
        assertEquals(listOf("Battery optimization can stop Pneu","Pneu isn't in Never sleeping apps"),revoked.issues)
        assertTrue(health(exempt=true).issues.isEmpty())
    }

    @Test fun revocationRechecksPreviouslyAcknowledgedSleepingState() {
        assertTrue(health(exempt=false,acknowledged=true).sleeping)
        assertFalse(health(exempt=false,acknowledged=true,bucketAllowed=false).sleeping)
        assertFalse(health(exempt=false,acknowledged=true,killed=true).sleeping)
    }

    @Test fun exemptionDoesNotSuppressBackgroundKillEvidence() {
        for(acknowledged in listOf(false,true)) {
            val killed=health(exempt=true,acknowledged=acknowledged,bucketAllowed=false,killed=true)
            assertTrue(killed.sleeping)
            assertTrue(killed.killed)
            assertEquals(listOf("Pneu was stopped in the background"),killed.issues)
        }
    }
}
