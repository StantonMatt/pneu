package io.github.stantonmatt.pneu.connection

import android.content.Context
import android.net.*
import android.net.nsd.*
import android.net.wifi.WifiManager
import androidx.core.content.ContextCompat
import android.os.SystemClock
import android.provider.Settings
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.features.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.File
import java.net.Socket
import javax.net.ssl.SSLSocket
import kotlin.random.Random

data class LinkState(val status: String = "PC unreachable", val route: String = "", val pairing: String = "", val sent: Long = 0, val total: Long = 0, val transferMediaId: String = "", val cellular: Boolean = false, val pairingPin: String = "", val transferKind: String = "", val transferFileName: String = "",
    val pairingFeedback:UiFeedback?=pairing.takeIf(String::isNotEmpty)?.let {UiFeedback(it)}) {
    /** Emit a new pairing result even when its copy matches the preceding one.
     * Ordinary route/progress copies preserve the existing event identity. */
    fun withPairing(message:String)=copy(pairing=message,pairingFeedback=message.takeIf(String::isNotEmpty)?.let {UiFeedback(it)})
    fun sendingTitle():String? = if(transferMediaId.isEmpty())null else when(transferKind) {
        "photo" -> "Sending photo"
        "recording" -> "Sending screen recording"
        "file" -> "Sending ${transferFileName.ifEmpty {"file"}}"
        "clipboard_image" -> "Sending clipboard image"
        else -> "Sending screenshot"
    }
    fun pairingSuccess(p: Prefs) = pairing.startsWith("Paired with ") && pairingPin.isNotEmpty() && p.peer?.pin == pairingPin && p.pending == null && !p.pairingLost
}
private class RemoteError(val code: String) : Exception(code)
class ConnectionManager(private val app: AppGraph) {
    val state = MutableStateFlow(LinkState())
    private val context = app.context
    private val cm = context.getSystemService(ConnectivityManager::class.java)
    private val nsd = context.getSystemService(NsdManager::class.java)
    private val wake = Channel<Unit>(Channel.CONFLATED)
    private val writeWake = Channel<Unit>(Channel.CONFLATED)
    private val writes = Mutex()
    private val discovery = linkedMapOf<String, Pair<String, List<Endpoint>>>()
    private val serviceCallbacks = linkedMapOf<String, NsdManager.ServiceInfoCallback>()
    private val multicast = context.getSystemService(WifiManager::class.java).createMulticastLock("pneu.discovery").apply { setReferenceCounted(false) }
    private var job: Job? = null
    @Volatile private var socket: Socket? = null
    @Volatile private var tcpSocket:Socket?=null
    private data class CapturedConnection(val ssl:SSLSocket,val tcp:Socket?)
    @Volatile private var revoking:CapturedConnection?=null
    @Volatile private var unpairing=false
    private val unpairs=Mutex()
    private fun closeCaptured(connection:CapturedConnection) {
        connection.tcp?.runCatching {close()}
        connection.ssl.runCatching {close()}
    }
    private fun closeSocket(target:Socket?=socket) {
        // Local revocation owns this socket until its durable removal and bounded
        // final write finish. Policy/lane teardown must not race that notification.
        if(target!=null && target===revoking?.ssl)return
        // Conscrypt.close can wait for a writer. Closing raw TCP first interrupts that writer.
        if(target===socket)tcpSocket?.runCatching {close()}
        target?.runCatching {close()}
    }
    @Volatile private var ready = false
    @Volatile private var txAt = 0L
    private val writing = ProgressDeadline(60_000, SystemClock::elapsedRealtime)
    private val rejections = RejectionStreak()
    internal var onUnauthorized:(()->Unit)?=null
    @Volatile private var activeReceipt: Pair<String, CompletableDeferred<Frame.Control>>? = null
    @Volatile private var fileEmission:QueuedEmission?=null
    @Volatile private var receiptReady = false
    @Volatile private var textReceipt: Pair<String,CompletableDeferred<Frame.Control>>?=null
    @Volatile private var textEmission:QueuedEmission?=null
    @Volatile private var capabilities:Set<String> = emptySet()
    private val dialEpoch=java.util.concurrent.atomic.AtomicLong()
    @Volatile private var activeEndpoint:Endpoint?=null
    @Volatile private var activePin=""
    val connected get()=ready
    fun supports(capability: String)=ready && capability in capabilities
    fun kick() {wake.trySend(Unit)}
    // Kept as the policy-facing API: "cellular" means metered, including hotspots.
    fun connectivity(): Connectivity = RoutePolicy.active(cm).connectivity
    fun cellular(): Boolean = connectivity().mobile
    private var discoveryListener:Pair<Long,NsdManager.DiscoveryListener>?=null
    private val discoveryLease:DiscoveryLifecycle=DiscoveryLifecycle(
        acquire={multicast.acquire()},
        start={token->
            val listener=discoveryListener(token)
            discoveryListener=token to listener
            nsd.discoverServices("_pneu._tcp.",NsdManager.PROTOCOL_DNS_SD,listener)
        },
        stop={token->
            discoveryListener?.takeIf {it.first==token}?.let {(_,listener)->
                discoveryListener=null;nsd.stopServiceDiscovery(listener)
            }
        },
        clear={
            val callbacks=synchronized(serviceCallbacks) {serviceCallbacks.values.toList().also {serviceCallbacks.clear()}}
            synchronized(discovery) {discovery.clear()}
            callbacks.forEach {runCatching {nsd.unregisterServiceInfoCallback(it)}}
        },
        release={if(multicast.isHeld)multicast.release()}
    )
    internal val discoveryActive get()=discoveryLease.active
    internal val multicastHeld get()=multicast.isHeld
    internal val discoveryWatchCount get()=synchronized(serviceCallbacks) {serviceCallbacks.size}
    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = retry()
        override fun onLost(network: Network) = retry()
        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            // onAvailable/onLost already restart. Capability changes wake a disconnected loop.
            if(ready && state.value.cellular != cellular()) retry()
            else if (!ready) wake.trySend(Unit)
        }
    }
    private fun discoveryListener(token:Long):NsdManager.DiscoveryListener=object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(type:String) {}
        override fun onDiscoveryStopped(type:String) {discoveryLease.fail(token)}
        override fun onStartDiscoveryFailed(type:String,code:Int) {discoveryLease.fail(token)}
        override fun onStopDiscoveryFailed(type:String,code:Int) {discoveryLease.fail(token)}
        override fun onServiceFound(info:NsdServiceInfo) {discoveryLease.accept(token) {watchService(info,token)}}
        override fun onServiceLost(info:NsdServiceInfo) {discoveryLease.accept(token) {
            synchronized(discovery) {discovery.remove(info.serviceName)}
            synchronized(serviceCallbacks) {serviceCallbacks.remove(info.serviceName)}?.let {runCatching {nsd.unregisterServiceInfoCallback(it)}}
        }}
    }
    fun start(scope: CoroutineScope) {
        if (job?.isActive == true) return
        val previousJob = job
        job = scope.launch(Dispatchers.IO) {
            previousJob?.join()
            try {
            app.initialized.await();cm.registerDefaultNetworkCallback(callback);discoveryLease.open()
            launch {
                var previous: Prefs? = null
                app.prefs.filter {it.loaded}.collect { p ->
                    val old = previous; previous = p
                    ensureDiscovery(p)
                    if (old != null && ((old.pending != p.pending && p.pending != null) || old.wifi != p.wifi || old.tailscale != p.tailscale || old.mobile != p.mobile)) retry()
                    if (old != null && old.paused != p.paused && ready) runCatching { send(Frame.Control(Type.STATUS, json("paused" to p.paused))) }
                }
            }
            var failures = 0
            while (isActive) {
                var succeeded=false
                try {
                if(unpairing) {delay(100);continue}
                app.reconcileDelivery()
                val prefs = app.preferences.read()
                ensureDiscovery(prefs)
                if (prefs.deviceId.isEmpty() || (prefs.peer == null && prefs.pending == null) || prefs.pairingLost) { delay(100); continue }
                if (!networkAllowed(prefs)) { withTimeoutOrNull(2000) { wake.receive() }; continue }
                val invitation = prefs.pending
                val pin = invitation?.fingerprint ?: prefs.peer!!.pin
                val endpoints = candidates(prefs)
                for (endpoint in endpoints) {
                    app.reconcileDelivery();app.recoverOutgoing()
                    val current=app.preferences.read()
                    if(!isActive || current.pending!=prefs.pending || current.peer!=prefs.peer || !endpointAllowed(current,endpoint))break
                    val epoch=dialEpoch.get()
                    var pinnedLeafVerified=false
                    var rejected=false
                    activeEndpoint=endpoint;activePin=pin
                    state.value = LinkState("Connecting…", pairing = if (invitation != null) "Pairing…" else "")
                    try {
                        val ssl = TlsIdentity(prefs.tlsAlias).socket(endpoint, pin, {pinnedLeafVerified=it}) { socket = it;if(it !is SSLSocket)tcpSocket=it;if(epoch!=dialEpoch.get())closeSocket(it) }
                        if(epoch!=dialEpoch.get() || !endpointAllowed(app.preferences.read(),endpoint))throw java.io.IOException("Policy changed")
                        session(ssl,endpoint,prefs,epoch) {failures=0;succeeded=true}
                    } catch (e: CancellationException) { throw e }
                    catch (e: RemoteError) {
                        if (e.code == "unauthorized") {
                            if(invitation == null) {rejected=PairingRejection.unauthorized(e.code,pinnedLeafVerified);if(rejected)onUnauthorized?.invoke()}
                            else if(app.cancelPairing(invitation))state.value=LinkState(pairing="Can't reach your PC")
                            break
                        }
                        if (invitation != null && e.code in setOf("invalid_token", "expired_token", "pairing_closed", "already_paired", "storage_failed")) {
                            if(app.cancelPairing(invitation))state.value = LinkState(pairing = if (e.code == "expired_token") { if(invitation.expires > System.currentTimeMillis()) "Your PC's clock says this code expired. Check both clocks and scan the new code." else "This code expired. Scan the new one." } else "Can't reach your PC")
                            break
                        }
                        if (e.code == "identity_mismatch") {lost(prefs.peer,prefs.deviceId);break}
                        if (e.code == "connection_replaced") break
                    } catch (e: Exception) {
                        // Only exception classes are logged; never endpoints, tokens, or user content.
                        android.util.Log.w("PneuConnection", generateSequence<Throwable>(e) { it.cause }.joinToString(" <- ") { it.javaClass.simpleName })
                    } finally {
                        ready = false; closeSocket(); socket = null;tcpSocket=null;activeEndpoint=null;activePin=""
                        activeReceipt = null; textReceipt=null;textEmission=null;capabilities=emptySet(); receiptReady=false;fileEmission=null
                        // Reconcile even if a subsequent pairing-loss persistence step fails.
                        withContext(NonCancellable) {recoverSession()}
                        try {
                            if(invitation == null && epoch==dialEpoch.get() && prefs.peer!=null && rejections.record("${prefs.peer.pin}:${prefs.peer.deviceId}",rejected)) lost(prefs.peer,prefs.deviceId)
                        } finally {
                            val status=if(app.awaitPrefs().pairingLost)"Pairing lost" else "PC unreachable"
                            state.update {
                                val disconnected=it.copy(status=status,route="",cellular=false,sent=0,total=0,transferMediaId="",transferKind="",transferFileName="",pairingPin="")
                                if(disconnected.pairing.startsWith("Paired with "))disconnected.withPairing("") else disconnected
                            }
                        }
                    }
                    if (succeeded) break
                }
                if (invitation != null && app.awaitPrefs().pending != null) state.value = LinkState(pairing = "Can't reach your PC")
                } catch(e:CancellationException) {throw e}
                catch(e:Exception) {android.util.Log.w("PneuAttempt",e.javaClass.simpleName)}
                failures=backoff(failures,succeeded)
            }
            } finally {discoveryLease.close();runCatching {cm.unregisterNetworkCallback(callback)}}
        }
    }
    private suspend fun backoff(failures:Int,succeeded:Boolean):Int {
        val seconds=listOf(2,4,8,16,32,60)[failures.coerceAtMost(5)]
        var next=if(succeeded)0 else failures+1
        withTimeoutOrNull((seconds*1000*Random.nextDouble(.8,1.2)).toLong()) {wake.receive();next=0}
        return next
    }
    /** session() has joined its cancelled senders before this reconciliation.
     * Dial-time recovery runs inside the outer attempt/backoff boundary. */
    private suspend fun recoverSession():Boolean=try {app.recoverOutgoing();true}
        catch(e:CancellationException) {throw e}
        catch(e:Exception) {android.util.Log.w("PneuRecovery",e.javaClass.simpleName);false}
    private fun watchService(info: NsdServiceInfo,token:Long) {
        synchronized(serviceCallbacks) {
            if(info.serviceName in serviceCallbacks || serviceCallbacks.size >= 32) return
            val callback = object : NsdManager.ServiceInfoCallback {
                private fun active(body:()->Unit):Unit=discoveryLease.accept(token) {
                    if(synchronized(serviceCallbacks) {serviceCallbacks[info.serviceName]===this})body()
                }
                private fun removed()=active {
                    synchronized(serviceCallbacks) {serviceCallbacks.remove(info.serviceName)}
                    synchronized(discovery) {discovery.remove(info.serviceName)}
                }
                override fun onServiceInfoCallbackRegistrationFailed(code: Int) {removed()}
                override fun onServiceInfoCallbackUnregistered() {removed()}
                override fun onServiceLost() {active {synchronized(discovery) {discovery.remove(info.serviceName)}}}
                override fun onServiceUpdated(service: NsdServiceInfo) {active {
                    val fp=service.attributes["fp"]?.toString(Charsets.UTF_8)
                    val version=service.attributes["v"]?.toString(Charsets.UTF_8)
                    val addresses=service.hostAddresses.mapNotNull { it.hostAddress }.filter { '%' !in it }.map { Endpoint(it,service.port) }
                    if(fp != null && version == "1" && service.port in 1..65535 && addresses.isNotEmpty()) {
                        synchronized(discovery) { discovery[service.serviceName] = fp to addresses }
                        if(!ready) wake.trySend(Unit)
                    }
                }}
            }
            serviceCallbacks[info.serviceName]=callback
            runCatching { nsd.registerServiceInfoCallback(info, ContextCompat.getMainExecutor(context), callback) }
                .onFailure { serviceCallbacks.remove(info.serviceName) }
        }
    }
    internal fun candidates(p: Prefs): List<Endpoint> {
        val addresses = p.pending?.addresses ?: p.peer?.addresses.orEmpty()
        val pin = p.pending?.fingerprint ?: p.peer?.pin
        val lan = if (p.wifi) buildList {
            if (p.pending == null && p.lastLan.isNotEmpty()) runCatching { Endpoint.parse(p.lastLan) }.getOrNull()?.let { add(it) }
            synchronized(discovery) { addAll(discovery.values.filter { it.first == pin }.flatMap { it.second }.filter { !it.tailscale }) }
            addAll(addresses.filter { !it.tailscale })
        } else emptyList()
        return (lan + if (p.tailscale) addresses.filter { it.tailscale } else emptyList()).distinct()
    }
    private fun localNetworkGranted()=android.os.Build.VERSION.SDK_INT<37 || context.granted(android.Manifest.permission.ACCESS_LOCAL_NETWORK)
    private fun ensureDiscovery(p:Prefs) {
        // API 37 can otherwise open a system NSD picker from the foreground service,
        // covering Pneu's own confirmation/settings recovery UI.
        discoveryLease.update(p.wifi && localNetworkGranted() && !p.deliveryReset && !p.pairingLost && (p.peer!=null || p.pending!=null))
    }
    private fun networkAllowed(p: Prefs, network:RoutePolicy.Active=RoutePolicy.active(cm)): Boolean {
        if(!localNetworkGranted()) {state.update {it.copy(status="Local network access is off")};return false}
        if (!network.available) { state.update { it.copy(status="No network") }; return false }
        if (network.mobile && !p.mobile) { state.update { it.copy(status="Waiting for Wi-Fi") }; return false }
        return true
    }
    private fun endpointAllowed(p: Prefs,endpoint: Endpoint,network:RoutePolicy.Active=RoutePolicy.active(cm))=networkAllowed(p,network) && !p.deliveryReset && DeliveryPolicy.transport(endpoint.tailscale,p.wifi,p.tailscale,network.connectivity,p.mobile)
    private suspend fun <T> phase(ssl:SSLSocket,timeout:Long,body:suspend ()->T):T=coroutineScope {
        val deadline=launch(Dispatchers.Default) {delay(timeout);closeSocket(ssl)}
        try {body()} finally {deadline.cancel()}
    }
    private suspend fun session(ssl: SSLSocket, endpoint: Endpoint, p: Prefs,epoch:Long,onReady:()->Unit) = coroutineScope {
        val name = Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME) ?: android.os.Build.MODEL
        val hello=phase(ssl,10000) {
        send(Frame.Control(Type.HELLO, json("version" to 1, "device_id" to p.deviceId, "name" to utf8Prefix(name,128).ifEmpty {"Pneu"}, "capabilities" to listOf("clipboard_text"), "paused" to p.paused)))
        readControl(ssl)
        }
        if (hello.type == Type.ERROR) throw RemoteError(hello.body["code"].asString)
        require(hello.type == Type.HELLO)
        if (hello.body["version"].asLong != 1L) {
            send(Frame.Control(Type.ERROR, json("code" to "version_mismatch", "supported_version" to 1))); throw RemoteError("version_mismatch")
        }
        val pcId = hello.body["device_id"].asString
        if (p.pending == null && pcId != p.peer?.deviceId) { lost(p.peer,p.deviceId); throw RemoteError("identity_mismatch") }
        val deferred=ArrayDeque<Frame.Control>()
        p.pending?.let { invitation ->
            phase(ssl,10000) {
            send(Frame.Control(Type.PAIR_REQUEST, json("token" to invitation.token)))
            var result=readControl(ssl)
            while(result.type!=Type.PAIR_RESULT && result.type!=Type.ERROR) {
                when(result.type) {
                    Type.PING->send(Frame.Control(Type.PONG,result.body))
                    Type.CLIPBOARD,Type.STATUS->{require(deferred.size<4);deferred.add(result)}
                    else->throw RemoteError("unexpected_message")
                }
                result=readControl(ssl)
            }
            if (result.type == Type.ERROR) throw RemoteError(result.body["code"].asString)
            require(result.type == Type.PAIR_RESULT)
            if (!result.body["accepted"].asBoolean) throw RemoteError(result.body["code"].asString)
            // Persist before processing any READY traffic, including immediate peer data.
            if(epoch!=dialEpoch.get() || !app.installBinding(invitation,pcId))throw java.io.IOException("Stale pairing")
            }
        }
        if(epoch!=dialEpoch.get() || !endpointAllowed(app.preferences.read(),endpoint))throw java.io.IOException("Policy changed")
        val boundPeer=p.pending?.let {Peer(it.name,it.fingerprint,pcId,it.addresses)} ?: p.peer
        val sessionBinding=DeliveryBinding(p.deviceId,requireNotNull(boundPeer).pin,pcId)
        check(app.preferences.connected(sessionBinding,endpoint)) {"Binding changed"}
        capabilities=hello.body["capabilities"].asJsonArray.map {it.asString}.toSet()
        app.queue.pending().filter {it.error=="unsupported_kind" && it.rejectedCapabilities!=capabilities.sorted().joinToString(",") && (if(it.kind=="text")"clipboard_text" in capabilities else "files" in capabilities && (it.kind!="clipboard_image" || "clipboard_image" in capabilities))}.forEach {
            it.state="waiting";it.error="";it.nextAttempt=0;updateRow(it)
        }
        // A progressing upload can receive nothing until ACK. READY liveness
        // belongs to PING/PONG, write progress and post-END receipt deadlines.
        ready = true;onReady();ssl.soTimeout = 0
        val currentPaused=app.preferences.read().paused
        if(currentPaused!=p.paused)send(Frame.Control(Type.STATUS,json("paused" to currentPaused)))
        deferred.forEach {if(it.type==Type.CLIPBOARD)send(app.clipboard.receive(pcId,it.body,sessionBinding))}
        val network = RoutePolicy.active(cm)
        state.value = LinkState("Connected", network.label(endpoint.tailscale), if (p.pending != null) "Paired with ${p.pending.name}" else "",cellular=network.mobile,pairingPin=if(p.pending!=null)p.pending.fingerprint else "")
        val ping = PingDeadline(30_000, SystemClock::elapsedRealtime)
        val livenessWake = Channel<Unit>(Channel.CONFLATED)
        val writeWatchdog = launch {
            while (isActive) {
                val snapshot=writing.snapshot()
                if(snapshot == null) writeWake.receive()
                else {
                    val remaining=writing.remaining(snapshot)
                    if(remaining <= 0) {
                        if(writing.expire(snapshot)) {closeSocket(ssl);throw RemoteError("timeout")}
                        continue
                    }
                    withTimeoutOrNull(remaining) {writeWake.receive()}
                }
            }
        }
        val monitor = launch {
            while (isActive) {
                val now=SystemClock.elapsedRealtime()
                val pendingPing=ping.snapshot()
                val lastTx=txAt
                val remaining=if(pendingPing != null) ping.remaining(pendingPing) else lastTx+60_000-now
                if(remaining > 0) {withTimeoutOrNull(remaining) {livenessWake.receive()};continue}
                if(pendingPing != null) {
                    if(ping.expire(pendingPing)) {closeSocket(ssl);throw RemoteError("timeout")}
                    continue
                }
                if(txAt != lastTx)continue
                val nonce=randomId(8);val pending=ping.start(nonce)
                send(Frame.Control(Type.PING,json("nonce" to nonce)))
                ping.sent(pending)
            }
        }
        fun launchSender(text: Boolean)=launch {
            val work=Channel<Unit>(Channel.CONFLATED)
            val events=launch {combine(app.prefs,app.work,state.map {it.status to it.cellular}.distinctUntilChanged()) {_,_,_->Unit}.collect {work.trySend(Unit)}}
            var waitMillis=0L
            val retries=RetryDeadlines(System::currentTimeMillis,SystemClock::elapsedRealtime)
            val deferred=mutableMapOf<String,Long>()
            try {while(isActive) {
                LaneWait.await(work,waitMillis)
                var selected:Outgoing?=null
                try {
                val prefs=app.preferences.read()
                val network=RoutePolicy.active(cm)
                if(!endpointAllowed(prefs,endpoint,network))throw TransportFailure(java.io.IOException("Policy changed"))
                // This lane has no active transfer here. A failed completion AND
                // failed fallback write can leave sending durable after the receipt.
                app.recoverDeliveryLane(text)
                val lane=app.queue.pending().filter {(it.kind=="text")==text}
                retries.read(lane)
                val now=SystemClock.elapsedRealtime()
                deferred.entries.removeAll {it.value<=now || lane.none {row->row.mediaId==it.key}}
                val selection=QueueSchedule.choose(lane.filter {!DeliveryPolicy.hold(it.kind,it.automatic,prefs.paused,network.mobile,prefs.originals,it.attempted,it.optimized)},now,retries::deadline,deferred)
                waitMillis=selection.waitMillis
                var row=selection.row
                if(row==null)continue
                selected=row
                val supported=if(text) "clipboard_text" in capabilities else "files" in capabilities && (row.kind!="clipboard_image" || "clipboard_image" in capabilities)
                if(!supported) {row.state="failed";row.error="unsupported_kind";row.nextAttempt=Long.MAX_VALUE;row.rejectedCapabilities=capabilities.sorted().joinToString(",");updateRow(row);waitMillis=0;continue}
                    val processed=if(text) transferText(row,sessionBinding,retries)
                    else {
                        if(DeliveryPolicy.optimize(row.kind,row.mime,network.mobile,prefs.originals,row.attempted,row.optimized)) {
                            try {row=app.staging.optimize(row);selected=row}
                            catch(e: LocalCapacityException) {throw e}
                            catch(e: CancellationException) {throw e}
                            catch(_:IllegalArgumentException) {row.state="failed";row.error="image_decode";row.nextAttempt=Long.MAX_VALUE;updateRow(row);continue}
                            catch(e:java.io.IOException) {throw LocalSendFailure("local_read",e)}
                        }
                        transfer(row,sessionBinding,retries)
                    }
                // Policy/revision/discard can change during preparation or a writer
                // wait. Don't repeatedly rehash a rejected row or block its siblings.
                if(!processed)deferred[row.mediaId]=SystemClock.elapsedRealtime()+1000
                waitMillis=0L
                } catch(e: TransportFailure) {closeSocket(ssl);throw e}
                catch(e: CancellationException) {throw e}
                catch(e: Exception) {
                    // No local provider/file/encoding/database exception may reset either lane.
                    val failed=selected
                    if(failed!=null) {
                        failed.state="failed";failed.error=SendFailures.code(e)
                        retries.retryAfter(failed,SendFailures.retryAfter(failed.error))
                        try {updateRow(failed);waitMillis=0}
                        catch(cancelled:CancellationException) {throw cancelled}
                        catch(_:Exception) {
                            // Cool down only the failed row; new ready work can
                            // wake this lane and pass it while storage recovers.
                            deferred[failed.mediaId]=SystemClock.elapsedRealtime()+60000;waitMillis=0
                        }
                    } else waitMillis=60000 // Read/recovery failed before selection; a new-work signal can retry it.
                }
            }} finally {events.cancel();work.close()}
        }
        val sender=launchSender(false)
        val textSender=launchSender(true)
        try {
            while (isActive) {
                val frame = Protocol.read(ssl.inputStream)
                require(frame is Frame.Control)
                when (frame.type) {
                    Type.PING -> send(Frame.Control(Type.PONG, frame.body))
                    Type.PONG -> {
                        if(ping.pong(frame.body["nonce"].asString)) livenessWake.trySend(Unit)
                    }
                    Type.ACK, Type.NACK -> {
                        val receipt = activeReceipt
                        val id=frame.body["transfer_id"].asString
                        val target=if(receiptReady && receipt?.first==id)receipt else textReceipt?.takeIf {it.first==id}
                        require(target != null && target.second.complete(frame))
                    }
                    Type.UNPAIR -> {
                        app.forgetBinding(boundPeer,sessionBinding.phoneId); throw RemoteError("closing")
                    }
                    Type.ERROR -> throw RemoteError(frame.body["code"].asString)
                    Type.STATUS -> Unit
                    Type.CLIPBOARD -> send(app.clipboard.receive(pcId,frame.body,sessionBinding))
                    else -> { send(Frame.Control(Type.ERROR, json("code" to "unexpected_message"))); throw RemoteError("unexpected_message") }
                }
            }
        } finally { closeSocket(ssl); monitor.cancel(); sender.cancel(); textSender.cancel(); writeWatchdog.cancel() }
    }
    private fun readControl(ssl: SSLSocket): Frame.Control = Protocol.read(ssl.inputStream).also { require(it is Frame.Control) } as Frame.Control
    internal var openPayload:(File)->java.io.FileInputStream={it.inputStream()}
    private suspend fun transfer(row: Outgoing,binding:DeliveryBinding,retries:RetryDeadlines):Boolean {
        val file = File(row.path)
        if (!file.exists()) { row.state="failed";row.error="local_file_missing";row.nextAttempt=Long.MAX_VALUE;updateRow(row);return true }
        if (file.length() != row.size) { localMismatch(row,"size_mismatch",retries);return true }
        val input=SendFailures.local("local_read") {openPayload(file)}
        var began=false;var ended=false
        try {
            val digest=java.security.MessageDigest.getInstance("SHA-256")
            SendFailures.local("local_read") {
                val buffer=ByteArray(CHUNK_SIZE)
                while(true) {currentCoroutineContext().ensureActive();val n=input.read(buffer);if(n<0)break;digest.update(buffer,0,n)}
                input.channel.position(0)
            }
            if(digest.digest().hex()!=row.hash) {localMismatch(row,"hash_mismatch",retries);return true}
            val receipt=CompletableDeferred<Frame.Control>();activeReceipt=row.transferId to receipt
            val emission=QueuedEmission(row,binding);fileEmission=emission
            if(!send(Frame.Control(Type.FILE_BEGIN,Staging.metadata(row)),emission))return false
            began=true
            val buffer=ByteArray(CHUNK_SIZE);var offset=0L
            while(offset<row.size) {
                currentCoroutineContext().ensureActive()
                val n=SendFailures.local("local_read") {input.read(buffer,0,minOf(buffer.size.toLong(),row.size-offset).toInt()).also {if(it<=0)throw java.io.EOFException()}}
                send(Frame.Chunk(row.transferId,offset,buffer.copyOf(n)));offset+=n
                state.update {it.copy(sent=offset)}
                yield()
            }
            receiptReady=true
            send(Frame.Control(Type.FILE_END,json("transfer_id" to row.transferId)));ended=true
            val response=awaitReceipt(receipt)
            if(response.type==Type.ACK) {
                row.state="sent";row.error="";app.staging.completeFile(row)
            } else {
                val code=response.body["code"].asString;row.error=code;row.state="failed";if(code=="unsupported_kind")row.rejectedCapabilities=capabilities.sorted().joinToString(",")
                val decision=RetryPolicy.nack(code,row.mismatches,Random.nextDouble(.8,1.2))
                row.mismatches=decision.mismatches;retries.retryAfter(row,decision.waitMillis);updateRow(row)
            }
            return true
        } catch(e:LocalSendFailure) {
            if(began && !ended) {
                // A local read failed between complete frames. END with fewer bytes is
                // normative NACK size_mismatch and returns the peer's file lane to IDLE.
                receiptReady=true
                send(Frame.Control(Type.FILE_END,json("transfer_id" to row.transferId)))
                awaitReceipt(requireNotNull(activeReceipt).second)
            }
            throw e
        } catch(e:TransportFailure) {
            if(app.queue.find(row.mediaId)?.state!="discarded") {row.state="waiting";runCatching {updateRow(row)}}
            throw e
        } finally {
            runCatching {input.close()}
            activeReceipt=null;receiptReady=false;fileEmission=null
            state.update {it.copy(sent=0,total=0,transferMediaId="",transferKind="",transferFileName="")}
        }
    }
    private suspend fun awaitReceipt(receipt:CompletableDeferred<Frame.Control>):Frame.Control =
        withTimeoutOrNull(120000) {receipt.await()} ?: throw TransportFailure(java.net.SocketTimeoutException("Receipt deadline expired"))
    private suspend fun transferText(row: Outgoing,binding:DeliveryBinding,retries:RetryDeadlines):Boolean {
        val receipt=CompletableDeferred<Frame.Control>();textReceipt=row.transferId to receipt
        val emission=QueuedEmission(row,binding);textEmission=emission
        try {
            if(!send(Frame.Control(Type.CLIPBOARD,json("transfer_id" to row.transferId,"text" to row.text,"created_at" to row.createdAt,"origin" to row.origin)),emission))return false
            val response=awaitReceipt(receipt)
            if(response.type==Type.ACK) {row.state="sent";row.error="";updateRow(row)}
            else {
                val code=response.body["code"].asString;if(code=="unsupported_kind")row.rejectedCapabilities=capabilities.sorted().joinToString(",");val decision=RetryPolicy.nack(code,row.mismatches,Random.nextDouble(.8,1.2))
                row.state="failed";row.error=code;retries.retryAfter(row,decision.waitMillis);row.mismatches=decision.mismatches;updateRow(row)
            }
            return true
        } catch(e: TransportFailure) {row.state="waiting";runCatching {updateRow(row)};throw e}
        finally {textReceipt=null;textEmission=null}
    }
    private suspend fun updateRow(row:Outgoing)=app.updateDelivery(row)
    private suspend fun localMismatch(row: Outgoing, code: String,retries:RetryDeadlines) {
        val decision=RetryPolicy.nack(code,row.mismatches,1.0)
        row.state="failed";row.error=code;row.mismatches=decision.mismatches;retries.retryAfter(row,decision.waitMillis);updateRow(row)
    }
    internal var beforeFirstFrame:suspend (Outgoing)->Unit={}
    internal suspend fun withWriteLockForTest(body:suspend ()->Unit)=writes.withLock {body()}
    private suspend fun send(frame: Frame,emission:QueuedEmission?=null):Boolean {
        // Validate local fields/encoding before touching a wire frame.
        SendFailures.local("invalid_payload") {Protocol.write(java.io.OutputStream.nullOutputStream(),frame)}
        if(emission!=null)beforeFirstFrame(emission.row)
        return writes.withLock {
        val s = socket ?: throw TransportFailure(java.io.IOException("Disconnected"))
        if(unpairing)throw TransportFailure(java.io.IOException("Unpairing"))
        if(frame is Frame.Chunk || (frame is Frame.Control && frame.type in setOf(Type.FILE_BEGIN,Type.CLIPBOARD))) {
            val current=SendFailures.local("local_storage") {app.preferences.read()}
            if(activeEndpoint?.let {endpointAllowed(current,it)}!=true || (current.pending?.fingerprint ?: current.peer?.pin)!=activePin) {closeSocket(s);throw TransportFailure(java.io.IOException("Policy changed"))}
        }
        if(emission!=null) {
            // Revalidate both lanes after preparation AND after waiting for the
            // frame writer. Discard, replacement, Pause and cost-policy changes
            // cannot authorize a first frame from a stale selection.
            val accepted=SendFailures.local("local_storage") {app.beginDelivery(emission.row,emission.binding,
                eligible={p,current,newer->
                    val network=RoutePolicy.active(cm)
                    EmissionPolicy.eligible(emission.row,current,p,emission.binding,
                        activeEndpoint?.let {endpointAllowed(p,it,network)}==true,network.mobile,capabilities,newer)},begin=emission::begin)}
            if(!accepted)return@withLock false
            if(frame is Frame.Control && frame.type==Type.FILE_BEGIN) {
                val row=emission.row
                state.update {it.copy(sent=0,total=row.size,transferMediaId=row.mediaId,transferKind=row.kind,transferFileName=row.fileName)}
            }
        }
        writing.progress(); writeWake.trySend(Unit)
        try { withContext(Dispatchers.IO) { Protocol.write(ProgressOutput(s.getOutputStream()) {writing.progress();writeWake.trySend(Unit)},frame) }; txAt = SystemClock.elapsedRealtime() }
        catch(e:CancellationException) {closeSocket(s);throw e}
        catch(e:Exception) {
            val unauthorized=try {
                if(!ready && s is SSLSocket && !s.isClosed)phase(s,200) {withContext(Dispatchers.IO) {
                    PairingRejection.afterWriteFailure(e) {
                        s.soTimeout=200
                        val frame=readControl(s)
                        frame.type==Type.ERROR && frame.body["code"].asString=="unauthorized"
                    }
                }} else false
            } finally {closeSocket(s)}
            if(unauthorized)throw RemoteError("unauthorized")
            throw TransportFailure(e)
        }
        finally { writing.complete(); writeWake.trySend(Unit) }
        true
        }
    }
    private suspend fun lost(expected:Peer?,phoneId:String) {if(app.loseBinding(expected,phoneId))state.value=LinkState("Pairing lost")}
    fun bindingChanged(lost: Boolean=false) { ready=false;capabilities=emptySet();rejections.clear();state.value=LinkState(if(lost)"Pairing lost" else "PC unreachable") }
    fun retry() { dialEpoch.incrementAndGet();closeSocket(); wake.trySend(Unit) }
    fun cancelTransfer(mediaId: String) {
        fileEmission?.takeIf {it.row.mediaId==mediaId}?.let {if(it.cancel())retry()}
        // Already-emitted text must consume its ACK/NACK without resetting the
        // independent file lane. Queued text is cancelled before CLIPBOARD.
        textEmission?.takeIf {it.row.mediaId==mediaId}?.cancel()
    }
    internal var beforeUnpairRemoval:suspend ()->Unit={}
    suspend fun unpair(expected:ConfirmedBinding=ConfirmedBinding.from(app.prefs.value)):Boolean = withContext(Dispatchers.IO + NonCancellable) {unpairs.withLock {
        var captured:CapturedConnection?=null
        var matched=false
        try {
            // Freeze normal writes/teardown while durable removal changes the
            // policy. Only this captured authenticated session may notify it.
            beforeUnpairRemoval()
            if(!LifecycleOrder.revoke({app.forgetBinding(rotateUnpaired=false,confirmed=expected,beforeRemoval={
                // Called only after the confirmed identity matched under the
                // binding lock. A stale confirmation must not close a new session.
                matched=true
                captured=(socket.takeIf {ready} as? SSLSocket)?.let {CapturedConnection(it,tcpSocket)}
                revoking=captured;unpairing=true;ready=false
            })}) {
                val connection=captured
                if(connection!=null)coroutineScope {
                    val closer=launch(Dispatchers.Default) {delay(500);closeCaptured(connection)}
                    try {withTimeout(500) {writes.withLock {withContext(Dispatchers.IO) {
                        Protocol.write(connection.ssl.outputStream,Frame.Control(Type.UNPAIR,json()))
                        Protocol.write(connection.ssl.outputStream,Frame.Control(Type.ERROR,json("code" to "closing")))
                    }}}} finally {closer.cancel()}
                }
            })return@withLock false
            captured?.let(::closeCaptured)
            stop()
            withTimeoutOrNull(2000) {job?.join()}
            state.value = LinkState()
            true
        } finally {
            captured?.let(::closeCaptured)
            if(matched) {revoking=null;unpairing=false;kick()}
        }
    }}
    @androidx.annotation.VisibleForTesting
    internal suspend fun awaitStopped() {job?.join()}
    fun stop() {
        ready=false; closeSocket(); job?.cancel()
        runCatching { cm.unregisterNetworkCallback(callback) }
        discoveryLease.close()
    }
}
