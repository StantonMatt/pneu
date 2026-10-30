package io.github.stantonmatt.pneu.features

import org.junit.Assert.*
import org.junit.Test

class ReachabilityHintsTest {
    private fun hint(wifi:Boolean=false,vpn:Boolean=false,installed:Boolean=false,allowed:Boolean=true,peerTailscale:Boolean=true)=
        ReachabilityHints.unreachable(PhoneNetwork(wifi,vpn),installed,allowed,peerTailscale)

    @Test fun withoutTailscaleThePhonesOwnNetworkExplainsIt() {
        assertEquals(UnreachableHint.SAME_WIFI,hint(wifi=false))
        assertEquals(UnreachableHint.OFF_OR_OTHER_WIFI,hint(wifi=true))
        // Another VPN being on doesn't make Tailscale set up.
        assertEquals(UnreachableHint.SAME_WIFI,hint(wifi=false,vpn=true))
    }
    @Test fun tailscaleIsOnlyBlamedWhenItIsInstalledAndAllowed() {
        assertEquals(UnreachableHint.TAILSCALE_OFF,hint(installed=true))
        assertEquals(UnreachableHint.TAILSCALE_OFF,hint(wifi=true,installed=true))
        // Settings → Tailscale when away is off: not set up, so no Open Tailscale.
        assertEquals(UnreachableHint.SAME_WIFI,hint(installed=true,allowed=false))
        assertEquals(UnreachableHint.OFF_OR_OTHER_WIFI,hint(wifi=true,installed=true,allowed=false))
        // The computer has no Tailscale address: turning it on here can't help.
        assertEquals(UnreachableHint.SAME_WIFI,hint(installed=true,peerTailscale=false))
    }
    @Test fun withTailscaleOnTheComputerIsProbablyAsleep() {
        assertEquals(UnreachableHint.LAST_CONNECTED,hint(installed=true,vpn=true))
        assertEquals(UnreachableHint.LAST_CONNECTED,hint(wifi=true,installed=true,vpn=true))
    }
}
