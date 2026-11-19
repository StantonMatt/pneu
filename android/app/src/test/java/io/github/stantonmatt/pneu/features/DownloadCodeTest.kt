package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.isDownloadCode
import org.junit.Assert.*
import org.junit.Test

class DownloadCodeTest {
    private val page="https://stantonmatt.github.io/pneu/"

    @Test fun theComputersDownloadCodeIsRecognised() {
        assertTrue(isDownloadCode("https://stantonmatt.github.io/pneu/",page))
        assertTrue(isDownloadCode("https://stantonmatt.github.io/pneu",page))
        // Uppercase QR encoding keeps the host; the path stays exact.
        assertTrue(isDownloadCode("HTTPS://STANTONMATT.GITHUB.IO/pneu/",page))
        assertTrue(isDownloadCode("http://stantonmatt.github.io/pneu/",page))
    }
    @Test fun otherAddressesAndPairingCodesAreNot() {
        assertFalse(isDownloadCode("https://stantonmatt.github.io/pneu/privacy",page))
        assertFalse(isDownloadCode("https://stantonmatt.github.io/pneu//",page))
        assertFalse(isDownloadCode("https://stantonmatt.github.io/",page))
        assertFalse(isDownloadCode("https://stantonmatt.github.io.example.com/pneu/",page))
        assertFalse(isDownloadCode("https://example.com/pneu/",page))
        assertFalse(isDownloadCode("https://stantonmatt.github.io/pneu/?ref=x",page))
        assertFalse(isDownloadCode("https://stantonmatt.github.io:8443/pneu/",page))
        assertFalse(isDownloadCode("pneu://pair?v=1",page))
        assertFalse(isDownloadCode("stantonmatt.github.io/pneu",page))
        assertFalse(isDownloadCode("",page))
        assertFalse(isDownloadCode("not a url at all",page))
    }
}
