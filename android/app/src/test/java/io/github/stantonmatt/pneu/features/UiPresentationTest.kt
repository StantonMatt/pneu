package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.protocol.Endpoint
import org.junit.Assert.*
import org.junit.Test

class UiPresentationTest {
    @Test fun failedItemsUseOnlyTheApprovedCopy() {
        val copy=mapOf(
            "disk_full" to "Failed · PC disk is full",
            "not_writable" to "Failed · PC can't save files",
            "unsupported_kind" to "Failed · PC can't receive this",
            "image_decode" to "Failed · Couldn't read image",
            "local_file_missing" to "Failed · File is gone"
        )
        copy.forEach {(error,text)->assertEquals(text,UiPresentation.failedItem(error))}
        for(error in listOf("local_capacity","storage_failed","id_conflict","","private /file/path"))
            assertEquals("Failed",UiPresentation.failedItem(error))
    }

    @Test fun pairedPcShowsAtMostOneIpv4AddressPerRoute() {
        val hosts=listOf("pc.local","fd7a:115c:a1e0::1","2001:db8::1","::ffff:192.168.1.30",
            "100.67.144.28","192.168.1.20","100.67.144.29","192.168.1.21","192.168.1.20")
        assertEquals(listOf("192.168.1.20","100.67.144.28"),UiPresentation.pairedAddresses(hosts.map {Endpoint(it,47821)}))
        assertEquals(listOf("100.64.0.1"),UiPresentation.pairedAddresses(listOf(Endpoint("100.64.0.1",47821))))
        assertEquals(listOf("10.0.2.2"),UiPresentation.pairedAddresses(listOf(Endpoint("10.0.2.2",47821))))
    }

    @Test fun namesIpv6AndInvalidIpv4HaveNoAddressDetail() {
        assertTrue(UiPresentation.pairedAddresses(emptyList()).isEmpty())
        assertTrue(UiPresentation.pairedAddresses(listOf("pc.example.com","::1","fd7a:115c:a1e0::1","999.1.1.1","192.168.01.1").map {Endpoint(it,47821)}).isEmpty())
    }

    @Test fun rejectedSettingsLaunchFallsThroughAndStopsOnSuccess() {
        val attempted=mutableListOf<Int>()
        assertTrue(SettingsNavigation.launchFirstAvailable(listOf(1,2,3)) {
            attempted+=it
            if(it==1)throw SecurityException("Not exported")
        })
        assertEquals(listOf(1,2),attempted)
    }

    @Test fun settingsFallbackIsAttemptedAndAllFailuresAreContained() {
        for(succeeds in listOf(true,false)) {
            val attempted=mutableListOf<Int>()
            assertEquals(succeeds,SettingsNavigation.launchFirstAvailable(listOf(1,2,3)) {
                attempted+=it
                if(it<3 || !succeeds)throw IllegalStateException("Unavailable")
            })
            assertEquals(listOf(1,2,3),attempted)
        }
    }
}
