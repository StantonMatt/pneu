package io.github.stantonmatt.pneu.connection

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.net.ssl.SSLSockets
import io.github.stantonmatt.pneu.protocol.Endpoint
import io.github.stantonmatt.pneu.protocol.sha256
import java.math.BigInteger
import java.net.Socket
import java.security.*
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import java.security.interfaces.ECPublicKey
import java.security.spec.ECGenParameterSpec
import java.security.spec.ECParameterSpec
import java.util.Date
import javax.net.ssl.*
import javax.security.auth.x500.X500Principal

/** Keys never leave Android Keystore. The provider signs TLS CertificateVerify. */
class TlsIdentity(private val alias: String, create:Boolean=false) {
    companion object {
        const val LEGACY_ALIAS="pneu.identity.v1"
        fun aliasFor(deviceId:String)="pneu.identity.v2.$deviceId"
        private val keyLock=Any()
        fun retire(alias:String) {if(alias.isNotEmpty())synchronized(keyLock) {
            KeyStore.getInstance("AndroidKeyStore").apply {load(null);deleteEntry(alias)}
        }}
        private val deadlines=java.util.concurrent.ScheduledThreadPoolExecutor(1) {r->Thread(r,"Pneu-TLS-deadline").apply {isDaemon=true}}}
    private val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    init { synchronized(keyLock) {
        require(alias==LEGACY_ALIAS || alias.matches(Regex("pneu\\.identity\\.v2\\.[0-9a-f]{32}")))
        if (!store.containsAlias(alias)) {
            check(create) {"Missing identity certificate"}
            KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, "AndroidKeyStore").apply {
                initialize(KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_SIGN or KeyProperties.PURPOSE_VERIFY)
                    .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
                    .setDigests(KeyProperties.DIGEST_SHA256, KeyProperties.DIGEST_NONE)
                    // Conscrypt prehashes CertificateVerify and calls NONEwithECDSA on Keystore.
                    .setCertificateSubject(X500Principal("CN=Pneu Android"))
                    .setCertificateSerialNumber(BigInteger(128, SecureRandom()).add(BigInteger.ONE))
                    .setCertificateNotBefore(Date(System.currentTimeMillis() - 86_400_000))
                    .setCertificateNotAfter(Date(System.currentTimeMillis() + 10L * 365 * 86_400_000))
                    .build())
                generateKeyPair()
            }
        }
    }}
    val certificate: X509Certificate = store.getCertificate(alias) as X509Certificate
    private val privateKey: PrivateKey = store.getKey(alias,null) as PrivateKey
    private val keyManager = object : X509ExtendedKeyManager() {
        override fun chooseClientAlias(keyType: Array<out String>?, issuers: Array<out java.security.Principal>?, socket: Socket?): String? =
            if (keyType?.any { it == "EC" } == true) alias else null
        override fun chooseEngineClientAlias(keyType: Array<out String>?, issuers: Array<out java.security.Principal>?, engine: SSLEngine?): String? =
            if (keyType?.any { it == "EC" } == true) alias else null
        override fun getClientAliases(keyType: String?, issuers: Array<out java.security.Principal>?): Array<String>? = if (keyType == "EC") arrayOf(alias) else null
        override fun getCertificateChain(alias: String?): Array<X509Certificate>? = if (alias == this@TlsIdentity.alias) arrayOf(certificate) else null
        override fun getPrivateKey(alias: String?): PrivateKey? = if (alias == this@TlsIdentity.alias) privateKey else null
        override fun getServerAliases(keyType: String?, issuers: Array<out java.security.Principal>?) = null
        override fun chooseServerAlias(keyType: String?, issuers: Array<out java.security.Principal>?, socket: Socket?) = null
    }
    fun socket(endpoint: Endpoint, pin: String, pinnedLeafVerified: (Boolean) -> Unit = {}, register: (Socket) -> Unit): SSLSocket =
        socketWithResolver(endpoint,pin,pinnedLeafVerified,register) {budget,cancelled->
            Endpoint.literalAddress(endpoint.host)?.let {listOf(it)} ?: EndpointResolution.await(budget,cancelled) {complete->
                val signal=android.os.CancellationSignal()
                android.net.DnsResolver.getInstance().query(null,endpoint.host,android.net.DnsResolver.FLAG_EMPTY,deadlines,signal,
                    object:android.net.DnsResolver.Callback<List<java.net.InetAddress>> {
                        override fun onAnswer(answer:List<java.net.InetAddress>,rcode:Int) {complete(Result.success(answer))}
                        override fun onError(error:android.net.DnsResolver.DnsException) {complete(Result.failure(error))}
                    })
                val cancel:()->Unit={signal.cancel()};cancel
            }
        }
    /** Injectable resolution for tests; pin and platform TLS verification remain mandatory. */
    internal fun socketWithResolver(endpoint:Endpoint,pin:String,pinnedLeafVerified:(Boolean)->Unit={},register:(Socket)->Unit,
                                    resolve:(DialBudget,()->Boolean)->List<java.net.InetAddress>):SSLSocket {
        val context = SSLContext.getInstance("TLSv1.3").apply {
            init(arrayOf(keyManager), arrayOf(PinnedTrustManager(pin) {pinnedLeafVerified(true)}), SecureRandom())
        }
        val endpointBudget=DialBudget(15000);val connectBudget=DialBudget(5000)
        val resolving=Socket();register(resolving)
        val addresses=try {resolve(connectBudget) {resolving.isClosed}} finally {resolving.close()}
        return EndpointDialer.authenticate(addresses,endpoint.port,endpointBudget,
            {pinnedLeafVerified(false);register(it)},maxConnect=5000) {tcp->
            val ssl = context.socketFactory.createSocket(tcp, endpoint.host, endpoint.port, true) as SSLSocket
            register(ssl); ssl.enabledProtocols = arrayOf("TLSv1.3"); ssl.useClientMode = true
            if (SSLSockets.isSupportedSocket(ssl)) SSLSockets.setUseSessionTickets(ssl, false)
            ssl.soTimeout=minOf(10000,endpointBudget.remainingMillis())
            val deadline=deadlines.schedule({runCatching {tcp.close()};runCatching {ssl.close()}},ssl.soTimeout.toLong(),java.util.concurrent.TimeUnit.MILLISECONDS)
            try {ssl.startHandshake()} catch(e:Exception) {runCatching {ssl.close()};throw e} finally {deadline.cancel(false)}
            ssl.soTimeout=10000
            ssl
        }
    }
}

class PinnedTrustManager(private val pin: String, private val verified: () -> Unit = {}) : X509TrustManager {
    override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
    override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) = throw CertificateException("Client verification not used")
    override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {
        if (chain?.size != 1) throw CertificateException("Expected one self-signed leaf")
        val cert = chain.single()
        if (!MessageDigest.isEqual(sha256(cert.encoded).toByteArray(), pin.toByteArray())) throw CertificateException("PC certificate changed")
        cert.checkValidity()
        val publicKey = cert.publicKey as? ECPublicKey ?: throw CertificateException("Expected P-256")
        val expected = AlgorithmParameters.getInstance("EC").apply { init(ECGenParameterSpec("secp256r1")) }.getParameterSpec(ECParameterSpec::class.java)
        if (publicKey.params.curve != expected.curve || publicKey.params.generator != expected.generator || publicKey.params.order != expected.order || publicKey.params.cofactor != expected.cofactor) throw CertificateException("Expected P-256")
        if (cert.subjectX500Principal != cert.issuerX500Principal) throw CertificateException("Expected self-signed certificate")
        cert.verify(publicKey)
        verified()
        // TrustManager verifies the leaf only. Conscrypt still verifies TLS CertificateVerify.
    }
}
