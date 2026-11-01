package io.github.stantonmatt.pneu

import android.Manifest
import androidx.datastore.preferences.core.edit
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import java.net.InetAddress
import java.net.Socket
import java.security.*
import java.security.cert.X509Certificate
import javax.net.ssl.*

/** A real loopback TLS computer isolates observer tests from offline recipient
 * selection. Hold the production sender before its
 * first frame so these tests can inspect durable waiting work independently of ACKs. */
internal class LocalPeerFixture(private val app:AppGraph,
    private val capabilities:Set<String> = setOf("files","clipboard_text","clipboard_image"),
    private val helloVersion:Int=1,
    private val helloError:String?=null,
    private val beforeHello:()->Unit={},
    private val setupHello:SetupHello?=null,
    private val onHello:(Frame.Control)->Unit={},
    private val handle:(Frame.Control,(Frame.Control)->Unit)->Boolean={_,_->false}):AutoCloseable {
    private val alias=TlsIdentity.aliasFor(randomId())
    private val identity=TlsIdentity(alias,create=true)
    private val store=KeyStore.getInstance("AndroidKeyStore").apply {load(null)}
    private val key=store.getKey(alias,null) as PrivateKey
    private val manager=object:X509ExtendedKeyManager() {
        override fun getPrivateKey(a:String?)=if(a==alias)key else null
        override fun getCertificateChain(a:String?)=if(a==alias)arrayOf(identity.certificate) else null
        override fun getServerAliases(type:String?,issuers:Array<out Principal>?)=if(type=="EC")arrayOf(alias) else null
        override fun chooseServerAlias(type:String?,issuers:Array<out Principal>?,socket:Socket?)=if(type=="EC")alias else null
        override fun getClientAliases(type:String?,issuers:Array<out Principal>?)=null
        override fun chooseClientAlias(type:Array<out String>?,issuers:Array<out Principal>?,socket:Socket?)=null
    }
    private val phone=TlsIdentity(runBlocking {app.preferences.read().tlsAlias}).certificate
    private val trust=object:X509TrustManager {
        override fun getAcceptedIssuers()=arrayOf(phone)
        override fun checkClientTrusted(chain:Array<out X509Certificate>?,authType:String?) {
            if(chain?.size!=1 || !MessageDigest.isEqual(chain.single().encoded,phone.encoded))
                throw java.security.cert.CertificateException("Unexpected fixture client")
            phone.checkValidity();phone.verify(phone.publicKey)
        }
        override fun checkServerTrusted(chain:Array<out X509Certificate>?,authType:String?) {
            throw java.security.cert.CertificateException("Fixture is server-only")
        }
    }
    private val context=SSLContext.getInstance("TLSv1.3").apply {init(arrayOf(manager),arrayOf(trust),SecureRandom())}
    private val server=context.serverSocketFactory.createServerSocket(0,4,InetAddress.getByName("127.0.0.1")) as SSLServerSocket
    private val clients=java.util.concurrent.CopyOnWriteArrayList<Socket>()
    private val handlers=java.util.concurrent.CopyOnWriteArrayList<Thread>()
    @Volatile private var closed=false
    private var peer:Peer?=null
    private var pairingToken:String?=null
    val pairRequests=java.util.concurrent.atomic.AtomicInteger()
    val hellos=java.util.concurrent.atomic.AtomicInteger()
    val activeConnections get()=clients.size
    @Volatile private var controlSender:((Frame.Control)->Unit)?=null
    fun sendControl(frame:Frame.Control)=checkNotNull(controlSender)(frame)
    fun disconnect() {clients.forEach {it.close()}}
    fun invitation(deviceId:String,name:String):String {
        peer=Peer(name,sha256(identity.certificate.encoded),deviceId,listOf(Endpoint("127.0.0.1",server.localPort)))
        pairingToken=randomId(32)
        return "pneu://pair?v=1&name=${java.net.URLEncoder.encode(name,"UTF-8")}&fp=${peer!!.pin}&addr=127.0.0.1%3A${server.localPort}&token=$pairingToken&exp=${System.currentTimeMillis()+300000}"
    }
    private val thread=Thread({
        while(!closed)try {
            val client=server.accept() as SSLSocket;clients+=client
            val handler=Thread({serve(client)},"Pneu-local-peer-session").apply {isDaemon=true}
            handlers+=handler;handler.start()
        }catch(_:java.io.IOException) {if(!closed)throw IllegalStateException("Local peer listener failed")}
    },"Pneu-local-peer").apply {isDaemon=true;start()}
    private fun serve(socket:SSLSocket) {
        try {socket.use {ssl->
            ssl.enabledProtocols=arrayOf("TLSv1.3");ssl.useClientMode=false;ssl.needClientAuth=true;ssl.soTimeout=30000;ssl.startHandshake()
            val hello=Protocol.read(ssl.inputStream) as Frame.Control
            check(hello.type==Type.HELLO)
            hellos.incrementAndGet();onHello(hello);beforeHello()
            val selected=requireNotNull(peer)
            val output=ssl.outputStream
            val send:(Frame.Control)->Unit={frame->synchronized(output) {Protocol.write(output,frame)}}
            controlSender=send
            helloError?.let {send(Frame.Control(Type.ERROR,json("code" to it,"supported_version" to 1)));return}
            send(Frame.Control(Type.HELLO,json("version" to helloVersion,"device_id" to selected.deviceId,"name" to selected.name,"capabilities" to capabilities.toList(),"role" to "computer").apply {
                setupHello?.let {add("setup",it.json())}
            }))
            while(!closed) {
                val frame=Protocol.read(ssl.inputStream)
                if(frame is Frame.Control && !handle(frame,send))when(frame.type) {
                    Type.PING->send(Frame.Control(Type.PONG,frame.body))
                    Type.PAIR_REQUEST->{
                        check(frame.body["token"].asString==pairingToken);pairRequests.incrementAndGet()
                        send(Frame.Control(Type.PAIR_RESULT,json("accepted" to true)))
                    }
                    Type.PEER_SETTINGS->{
                        send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)))
                        send(Frame.Control(Type.PEER_SETTINGS,frame.body.deepCopy().apply {addProperty("transfer_id",randomId())}))
                    }
                    Type.ERROR,Type.UNPAIR->return
                    else->Unit
                }
            }
        }}catch(_:java.io.IOException) {}finally {clients-=socket}
    }
    suspend fun connect(peerId:String?=null,holdFirstFrame:Boolean=true) {
        val i=InstrumentationRegistry.getInstrumentation()
        i.uiAutomation.grantRuntimePermission(app.context.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val p=app.preferences.read()
        val original=requireNotNull(if(peerId==null)p.peer else p.devices.find {it.deviceId==peerId})
        peer=original.copy(pin=sha256(identity.certificate.encoded),addresses=listOf(Endpoint("127.0.0.1",server.localPort)),lastLan="")
        app.preferences.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).map {if(it.deviceId==original.deviceId)peer!! else it})}
        withTimeout(5000) {app.prefs.first {it.devices.any {saved->saved.deviceId==original.deviceId && saved.pin==peer!!.pin}}}
        if(holdFirstFrame)app.connection.beforeFirstFrame={awaitCancellation()}
        app.connection.start(app.scope)
        withTimeout(10000) {while(!app.connection.connected(original.deviceId))delay(20)}
    }
    override fun close() {
        closed=true;server.close();clients.forEach {runCatching {it.close()}}
        thread.join(2000);handlers.forEach {it.join(2000)}
        check(!thread.isAlive && handlers.none {it.isAlive}) {"Local TLS fixture did not stop"}
        TlsIdentity.retire(alias)
    }
}
