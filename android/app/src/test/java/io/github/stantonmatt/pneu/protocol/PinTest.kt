package io.github.stantonmatt.pneu.protocol

import io.github.stantonmatt.pneu.connection.PinnedTrustManager
import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayInputStream
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate

class PinTest {
    // Public-only Android Keystore test leaf, generated 2026-10-11, valid to 2036.
    private val bytes=javaClass.getResourceAsStream("/p256-public-test-leaf.der")!!.readBytes()
    private fun cert(data: ByteArray)=CertificateFactory.getInstance("X.509").generateCertificate(ByteArrayInputStream(data)) as X509Certificate
    @Test fun validSelfSignedP256LeafIsAcceptedByItsExactDerPin() {
        PinnedTrustManager(sha256(bytes)).checkServerTrusted(arrayOf(cert(bytes)),"EC")
    }
    @Test fun changedPinMissingCertificateAndAdditionalLeavesAreRejected() {
        val c=cert(bytes);val verifier=PinnedTrustManager(sha256(bytes))
        assertTrue(runCatching {PinnedTrustManager("0".repeat(64)).checkServerTrusted(arrayOf(c),"EC")}.isFailure)
        assertTrue(runCatching {verifier.checkServerTrusted(emptyArray(),"EC")}.isFailure)
        assertTrue(runCatching {verifier.checkServerTrusted(arrayOf(c,c),"EC")}.isFailure)
    }
    @Test fun exactPinsStillRejectExpiredFutureAndNonP256Leaves() {
        for (name in listOf("expired","future","p384")) {
            val fixture=javaClass.getResourceAsStream("/$name-public-test-leaf.der")!!.readBytes()
            assertTrue(name,runCatching {PinnedTrustManager(sha256(fixture)).checkServerTrusted(arrayOf(cert(fixture)),"EC")}.isFailure)
        }
    }
    @Test fun correctPinCannotExcuseAnInvalidSelfSignature() {
        val changed=bytes.clone();changed[changed.lastIndex]=(changed.last().toInt() xor 1).toByte()
        assertTrue(runCatching {PinnedTrustManager(sha256(changed)).checkServerTrusted(arrayOf(cert(changed)),"EC")}.isFailure)
    }
}
