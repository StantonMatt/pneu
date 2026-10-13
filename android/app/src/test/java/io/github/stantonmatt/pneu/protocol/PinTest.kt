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
    // Exercise fixture validity at their intended date, independently of when CI runs.
    private fun cert(data: ByteArray):X509Certificate {
        val leaf=CertificateFactory.getInstance("X.509").generateCertificate(ByteArrayInputStream(data)) as X509Certificate
        return object:X509Certificate() {
            override fun checkValidity()=leaf.checkValidity(java.util.Date.from(java.time.Instant.parse("2026-10-12T00:00:00Z")))
            override fun checkValidity(date:java.util.Date)=leaf.checkValidity(date)
            override fun getEncoded()=leaf.encoded
            override fun getPublicKey()=leaf.publicKey
            override fun verify(key:java.security.PublicKey)=leaf.verify(key)
            override fun verify(key:java.security.PublicKey,provider:String)=leaf.verify(key,provider)
            override fun getVersion()=leaf.version
            override fun getSerialNumber()=leaf.serialNumber
            @Suppress("DEPRECATION") override fun getIssuerDN()=leaf.issuerDN
            @Suppress("DEPRECATION") override fun getSubjectDN()=leaf.subjectDN
            override fun getIssuerX500Principal()=leaf.issuerX500Principal
            override fun getSubjectX500Principal()=leaf.subjectX500Principal
            override fun getNotBefore()=leaf.notBefore
            override fun getNotAfter()=leaf.notAfter
            override fun getTBSCertificate()=leaf.tbsCertificate
            override fun getSignature()=leaf.signature
            override fun getSigAlgName()=leaf.sigAlgName
            override fun getSigAlgOID()=leaf.sigAlgOID
            override fun getSigAlgParams()=leaf.sigAlgParams
            override fun getIssuerUniqueID()=leaf.issuerUniqueID
            override fun getSubjectUniqueID()=leaf.subjectUniqueID
            override fun getKeyUsage()=leaf.keyUsage
            override fun getBasicConstraints()=leaf.basicConstraints
            override fun hasUnsupportedCriticalExtension()=leaf.hasUnsupportedCriticalExtension()
            override fun getCriticalExtensionOIDs()=leaf.criticalExtensionOIDs
            override fun getNonCriticalExtensionOIDs()=leaf.nonCriticalExtensionOIDs
            override fun getExtensionValue(oid:String)=leaf.getExtensionValue(oid)
            override fun toString()=leaf.toString()
        }
    }
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
