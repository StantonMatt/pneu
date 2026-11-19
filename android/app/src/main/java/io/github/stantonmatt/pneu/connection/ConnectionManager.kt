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
    val capabilities:Set<String> = emptySet(),val incoming:IncomingTransfer?=null,val pairingFeedback:UiFeedback?=pairing.takeIf(String::isNotEmpty)?.let {UiFeedback(it)},val pairingGeneration:Long=0,val mismatchPin:String="") {
    val connectionStatus get()=ConnectionStatus.fromKey(status)
    /** Emit a new pairing result even when its copy matches the preceding one.
     * Ordinary route/progress copies preserve the existing event identity. */
    fun withPairing(message:String)=copy(pairing=message,pairingFeedback=message.takeIf(String::isNotEmpty)?.let {UiFeedback(it)})
    companion object {
        fun pairingFailure(status:ConnectionStatus)=LinkState(status.key,pairing=status.key,pairingFeedback=UiFeedback(status.key,status=status))
    }
    fun sendingTitle():String? = if(transferMediaId.isEmpty())null else when(transferKind) {
        "photo" -> "Sending photo"
        "recording" -> "Sending screen recording"
        "file" -> "Sending ${transferFileName.ifEmpty {"file"}}"
        "clipboard_image" -> "Sending clipboard image"
        else -> "Sending screenshot"
    }
    fun pairingSuccess(p: Prefs) = pairing.startsWith("Paired with ") && pairingPin.isNotEmpty() && p.devices.any {it.pin==pairingPin && it.generation==pairingGeneration && !it.pairingLost} && !p.pairingLost
}
private class RemoteError(val code: String) : Exception(code)
class ConnectionManager(private val app:AppGraph,private val peerPin:String?=null) {
    private fun scoped(p:Prefs)=BindingTransitions.scoped(p,peerPin)
    private fun pairingAttempt()=app.connection.attemptFor(peerPin)
    private suspend fun readPrefs()=scoped(app.preferences.read())
    private var clipboardCleanup:Pair<DeliveryBinding,Boolean>?=null
    private suspend fun retryClipboardCleanup():Boolean {
        val (binding,sessionOnly)=clipboardCleanup ?: return true
        return try {app.abandonClipboard(binding,sessionOnly);clipboardCleanup=null;true}
        catch(e:CancellationException) {throw e}
        catch(e:Exception) {android.util.Log.w("PneuCleanup",e.javaClass.simpleName);false}
    }
    private suspend fun recoverPeer() {
        if(!retryClipboardCleanup())throw java.io.IOException("Clipboard cleanup pending")
        val p=readPrefs();DeliveryBinding.from(p,p.peer?.deviceId)?.let {app.recoverDeliveryLane(false,it);app.recoverDeliveryLane(true,it)}
    }
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
    private data class DialFailure(val status:ConnectionStatus,val observedPin:String="")
    @Volatile private var lastIssue:DialFailure?=null
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
    @Volatile private var membersFence:Triple<DeliveryBinding,Long,String>?=null
    @Volatile private var membersReceipt:Pair<String,CompletableDeferred<Frame.Control>>?=null
    @Volatile private var settingsReady=false
    private val settingsEvents=Channel<Unit>(Channel.CONFLATED)
    @Volatile private var settingsReceipt:Pair<String,CompletableDeferred<Frame.Control>>?=null
    @Volatile private var ready = false
    // A UUID also fences restored PendingIntents and pending media after process death.
    @Volatile private var admissionSession:String?=null
    @Volatile private var txAt = 0L
    private val writing = ProgressDeadline(60_000, SystemClock::elapsedRealtime)
    private val rejections = RejectionStreak()
    internal var onUnauthorized:(()->Unit)?=null
    @Volatile private var activeReceipt: Pair<String, CompletableDeferred<Frame.Control>>? = null
    private val fileFrameWriter=FileFrameWriter()
    @Volatile private var cancelFlush:Job?=null
    @Volatile private var filePhase:FileSendPhase?=null
    private val cancelledFiles=java.util.concurrent.ConcurrentHashMap<String,FileSendPhase>()
    @Volatile private var fileEmission:QueuedEmission?=null
    @Volatile private var receiptReady = false
    @Volatile private var textReceipt: Pair<String,CompletableDeferred<Frame.Control>>?=null
    @Volatile private var textEmission:QueuedEmission?=null
    @Volatile private var capabilities:Set<String> = emptySet()
    private val dialEpoch=java.util.concurrent.atomic.AtomicLong()
    private val setupRefreshRedial=java.util.concurrent.atomic.AtomicBoolean()
    @Volatile private var activeEndpoint:Endpoint?=null
    @Volatile private var activePin=""
    @Volatile private var authenticatedBinding:DeliveryBinding?=null
    internal fun authenticatedAs(binding:DeliveryBinding)=authenticatedBinding==binding
    internal fun freezeSession(binding:DeliveryBinding):DeliveryBinding? {
        val id=admissionSession ?: return null
        return binding.copy(sessionId=id).takeIf {ready && authenticatedBinding==binding.copy(sessionId=null) && admissionSession==id}
    }
    internal fun ownsSession(binding:DeliveryBinding)=binding.sessionId!=null && freezeSession(binding.copy(sessionId=null))==binding
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
                var previousAttempt:String?=null
                combine(app.prefs.filter {it.loaded}.map(::scoped),app.connection.attempt) {p,a->p to a?.takeIf {peerPin==null || it.invitation.fingerprint==peerPin}}.collect { (p,a) ->
                    val changedAttempt=previousAttempt!=a?.id && a!=null
                    previousAttempt=a?.id
                    val old = previous; previous = p
                    ensureDiscovery(p)
                    if (old != null && (changedAttempt || BindingTransitions.restart(old,p,authenticatedBinding))) {
                        if(!changedAttempt && old.peer?.generation==p.peer?.generation &&
                            (old.membership.generation!=p.membership.generation || old.peer?.setupScope!=p.peer?.setupScope))
                            app.connection.expectSetupRefresh(peerPin)
                        retry()
                    }
                    if (old != null && old.paused != p.paused && ready) runCatching { send(Frame.Control(Type.STATUS, json("paused" to p.paused))) }
                }
            }
            var failures = 0
            while (isActive) {
                var succeeded=false
                try {
                if(unpairing) {delay(100);continue}
                app.reconcileDelivery()
                val view=app.connectionView(peerPin)
                val prefs=view.prefs;val attempt=view.attempt;val outcome=view.outcome
                ensureDiscovery(prefs)
                if (!BindingTransitions.mayDial(prefs,attempt)) { delay(100); continue }
                if (!networkAllowed(prefs)) { withTimeoutOrNull(2000) { wake.receive() }; continue }
                val invitation = attempt?.invitation
                val pin = invitation?.fingerprint ?: prefs.peer!!.pin
                val endpoints = candidates(prefs)
                var attemptIssue:ConnectionStatus?=null
                for (endpoint in endpoints) {
                    app.reconcileDelivery();recoverPeer()
                    val current=readPrefs()
                    if(!isActive || pairingAttempt()?.id!=attempt?.id || current.peer!=prefs.peer || !endpointAllowed(current,endpoint))break
                    val epoch=dialEpoch.get()
                    var pinnedLeafVerified=false
                    var rejected=false
                    var issue:ConnectionStatus?=null
                    activeEndpoint=endpoint;activePin=pin
                    authenticatedBinding=if(invitation==null)DeliveryBinding.from(prefs,prefs.peer?.deviceId) else null
                    state.value = LinkState("Connecting…", pairing = if (invitation != null) "Pairing…" else "")
                    try {
                        val ssl = TlsIdentity(prefs.tlsAlias).socket(endpoint, pin, {pinnedLeafVerified=it}) { socket = it;if(it !is SSLSocket)tcpSocket=it;if(epoch!=dialEpoch.get())closeSocket(it) }
                        if(epoch!=dialEpoch.get() || !endpointAllowed(readPrefs(),endpoint))throw java.io.IOException("Policy changed")
                        session(ssl,endpoint,prefs,epoch,attempt,outcome) {lastIssue=null;failures=0;succeeded=true}
                    } catch (e: CancellationException) { throw e }
                    catch (e: RemoteError) {
                        // An authenticated READY setup peer explicitly closing is a
                        // planned refresh. EOF, timeout and TLS failures never qualify.
                        if(e.code=="closing" && ready && "setup" in capabilities) {
                            app.connection.expectSetupRefresh(peerPin)
                            setupRefreshRedial.set(true)
                        }
                        issue=ConnectionStatus.remoteError(e.code)
                        if(issue!=null)lastIssue=DialFailure(issue)
                        if(issue!=null) {attempt?.let {app.rejectPairing(it,issue)};break}
                        if (e.code == "unauthorized") {
                            if(invitation==null)authenticatedBinding?.let {app.preferences.introducedRefusal(it,pinnedLeafVerified)}
                            if(invitation == null) {rejected=PairingRejection.unauthorized(e.code,pinnedLeafVerified,readPrefs().peer?.setupScoped==true);if(rejected)onUnauthorized?.invoke()}
                            else if(attempt!=null)app.rejectPairing(attempt,ConnectionStatus.UNREACHABLE)
                            break
                        }
                        if (invitation != null && e.code in setOf("invalid_token", "expired_token", "pairing_closed", "already_paired", "storage_failed")) {
                            if(e.code=="expired_token")app.rejectPairing(requireNotNull(attempt),if(invitation.expires > System.currentTimeMillis()) "Your PC's clock says this code expired. Check both clocks and scan the new code." else PairingIssue.EXPIRED_MESSAGE)
                            else app.rejectPairing(requireNotNull(attempt),ConnectionStatus.UNREACHABLE)
                            break
                        }
                        if (e.code == "protocol_identity") break
                        if (e.code == "connection_replaced") break
                    } catch (e: Exception) {
                        if(generateSequence<Throwable>(e) {it.cause}.any {it is FingerprintMismatch}) {
                            val mismatch=generateSequence<Throwable>(e) {it.cause}.filterIsInstance<FingerprintMismatch>().first()
                            issue=ConnectionStatus.FINGERPRINT_MISMATCH;lastIssue=DialFailure(issue,mismatch.observedPin);attemptIssue=issue
                        }
                        // Only exception classes are logged; never endpoints, tokens, or user content.
                        android.util.Log.w("PneuConnection", generateSequence<Throwable>(e) { it.cause }.joinToString(" <- ") { it.javaClass.simpleName })
                    } finally {
                        admissionSession=null;ready = false; closeSocket(); socket = null;tcpSocket=null;activeEndpoint=null;activePin=""
                        val endingBinding=authenticatedBinding
                        clipboardCleanup=endingBinding?.let {it to ("mesh" !in capabilities)}
                        authenticatedBinding=null
                        settingsReady=false;settingsReceipt=null;membersReceipt=null;membersFence=null
                        activeReceipt = null; textReceipt=null;textEmission=null;capabilities=emptySet(); receiptReady=false;fileEmission=null
                        // Reconcile even if a subsequent pairing-loss persistence step fails.
                        // Runtime authority and disconnected UI are mandatory:
                        // publish before any fallible durable maintenance.
                        publishDisconnected(if(scoped(app.prefs.value).pairingLost)"Pairing lost" else (issue ?: lastIssue?.status)?.key ?: "PC unreachable")
                        withContext(NonCancellable) {retryClipboardCleanup();recoverSession(endingBinding)}
                        try {
                            if(invitation == null && epoch==dialEpoch.get() && prefs.peer!=null && rejections.record("${prefs.peer.pin}:${prefs.peer.deviceId}:${prefs.peer.generation}",rejected)) lost(prefs.peer,prefs.deviceId,outcome)
                        } finally {
                            publishDisconnected(if(scoped(app.prefs.value).pairingLost)"Pairing lost" else (issue ?: lastIssue?.status)?.key ?: "PC unreachable")
                        }
                    }
                    if (succeeded) break
                }
                if (attempt != null && pairingAttempt()?.id==attempt.id) {
                    // A stale address may serve another certificate. Try all
                    // candidates, then retain the typed failure if none work.
                    if(attemptIssue!=null)app.rejectPairing(attempt,attemptIssue)
                    else state.value = LinkState.pairingFailure(ConnectionStatus.UNREACHABLE)
                }
                } catch(e:CancellationException) {throw e}
                catch(e:Exception) {android.util.Log.w("PneuAttempt",e.javaClass.simpleName)}
                failures=if(setupRefreshRedial.getAndSet(false)) {wake.tryReceive();0} else backoff(failures,succeeded)
            }
            } finally {try {discoveryLease.close()}finally {runCatching {cm.unregisterNetworkCallback(callback)}}}
        }
    }
    private suspend fun backoff(failures:Int,succeeded:Boolean):Int {
        val seconds=listOf(2,4,8,16,32,60)[failures.coerceAtMost(5)]
        var next=if(succeeded)0 else failures+1
        withTimeoutOrNull((seconds*1000*Random.nextDouble(.8,1.2)).toLong()) {wake.receive();next=0}
        return next
    }
    private fun publishDisconnected(status:String) {
        val mismatchPin=lastIssue?.takeIf {it.status.key==status}?.observedPin.orEmpty()
        state.update {
            val disconnected=it.copy(status=status,route="",cellular=false,sent=0,total=0,transferMediaId="",transferKind="",transferFileName="",pairingPin="",incoming=null,mismatchPin=mismatchPin)
            if(disconnected.pairing.startsWith("Paired with "))disconnected.withPairing("") else disconnected
        }
    }
    /** session() has joined its cancelled senders before this reconciliation.
     * Dial-time recovery runs inside the outer attempt/backoff boundary. */
    @androidx.annotation.VisibleForTesting
    internal suspend fun recoverSession(binding:DeliveryBinding?):Boolean {
        var recovered=true
        // A storage failure in one lane must not skip recovery of the other.
        if(binding!=null)for(text in listOf(false,true))try {app.recoverDeliveryLane(text,binding)}
        catch(e:CancellationException) {throw e}
        catch(e:Exception) {android.util.Log.w("PneuRecovery",e.javaClass.simpleName);recovered=false}
        return recovered
    }
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
        val invitation=pairingAttempt()?.invitation
        val addresses = invitation?.addresses ?: p.peer?.addresses.orEmpty()
        val pin = invitation?.fingerprint ?: p.peer?.pin
        val lan = if (p.wifi) buildList {
            if (invitation == null && p.lastLan.isNotEmpty()) runCatching { Endpoint.parse(p.lastLan) }.getOrNull()?.let { add(it) }
            synchronized(discovery) { addAll(discovery.values.filter { it.first == pin }.flatMap { it.second }.filter { !it.tailscale }) }
            addAll(addresses.filter { !it.tailscale })
        } else emptyList()
        return (lan + if (p.tailscale) addresses.filter { it.tailscale } else emptyList()).distinct().take(24)
    }
    private fun localNetworkGranted()=android.os.Build.VERSION.SDK_INT<37 || context.granted(android.Manifest.permission.ACCESS_LOCAL_NETWORK)
    private fun ensureDiscovery(p:Prefs) {
        // API 37 can otherwise open a system NSD picker from the foreground service,
        // covering Pneu's own confirmation/settings recovery UI.
        discoveryLease.update(p.wifi && localNetworkGranted() && !p.deliveryReset && (pairingAttempt()!=null || !p.pairingLost && p.peer!=null))
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
        try {body()} finally {deadline.cancel();state.update {it.copy(incoming=null)}}
    }
    private suspend fun session(ssl: SSLSocket, endpoint: Endpoint, p: Prefs,epoch:Long,
                                attempt:PairingAttempt?,outcome:ConnectionOutcome?,onReady:()->Unit):Unit = coroutineScope {
        var sessionOutcome=outcome
        val name=p.selfName.ifEmpty {Settings.Global.getString(context.contentResolver,Settings.Global.DEVICE_NAME) ?: android.os.Build.MODEL}
        val localScope=attempt?.prior?.setupScope ?: attempt?.let {if(it.setupEnabled)SetupScope.ENABLED else SetupScope.DISABLED} ?: p.peer?.setupScope ?: SetupScope.ASK
        val localSetup=p.membership.hello(localScope)
        val hello=phase(ssl,10000) {
        send(Frame.Control(Type.HELLO, json("version" to 1, "device_id" to p.deviceId, "name" to utf8Prefix(name,128).ifEmpty {"Pneu"}, "capabilities" to phoneComputerCapabilities(p.allowRinging),"role" to "phone", "paused" to p.paused).apply {add("setup",localSetup.json())}))
        readControl(ssl)
        }
        if (hello.type == Type.ERROR) throw RemoteError(hello.body["code"].asString)
        if(hello.type!=Type.HELLO) {
            send(Frame.Control(Type.ERROR,json("code" to "unexpected_message")));throw RemoteError("unexpected_message")
        }
        val advertised=hello.body["capabilities"].asJsonArray.map {it.asString}.toSet()
        if (ConnectionStatus.hello(hello.body["version"].asLong,advertised)!=null) {
            // HELLO already establishes the actionable failure. A failed
            // diagnostic write must not turn it into a generic network error.
            try {send(Frame.Control(Type.ERROR, json("code" to "version_mismatch", "supported_version" to 1)))}
            catch(e:CancellationException) {throw e}
            catch(_:Exception) { }
            throw RemoteError("version_mismatch")
        }
        val mesh=hello.body["capabilities"].asJsonArray.any {it.asString=="mesh"}
        if(mesh && hello.body["role"]?.asString!="computer") {send(Frame.Control(Type.ERROR,json("code" to "malformed_message")));throw RemoteError("malformed_message")}
        val pcId = hello.body["device_id"].asString
        // A self ID or a known certificate claiming another ID is a protocol
        // violation, never evidence that the retained binding was lost.
        if(pcId==p.deviceId || pcId.isEmpty() || attempt==null && pcId!=p.peer?.deviceId) {
            attempt?.let {app.rejectPairing(it,"Conflicting peer identity")}
            send(Frame.Control(Type.ERROR,json("code" to "unauthorized")))
            throw RemoteError("protocol_identity")
        }
        val remoteSetup=if("setup" in advertised)SetupWire.hello(hello.body["setup"]) else null
        val deferred=PairingControlBuffer(remoteSetup!=null)
        // Recovery can race a READY peer's file lane. Drain with the ordinary
        // framing checks and NACK only at END; never acquire storage or effects
        // before PAIR_RESULT has durably installed the binding. An in-flight
        // drain keeps its slot even when PAIR_RESULT interleaves before END.
        val recoveryFiles=FileReceiveMachine(ReceiveBackend {ReceiveAdmission(reply=ReceiveReply("storage_failed"))})
        suspend fun recoveryFile(frame:Frame) {
            try {
                when(frame) {
                    is Frame.Chunk->recoveryFiles.chunk(frame)
                    is Frame.Control->when(frame.type) {
                        Type.FILE_BEGIN->recoveryFiles.begin(frame.body)
                        Type.FILE_END->send(recoveryFiles.end(frame.body["transfer_id"].asString))
                        else->throw ReceiveViolation()
                    }
                }
            }catch(e:ReceiveViolation) {send(Frame.Control(Type.ERROR,json("code" to e.code)));throw RemoteError(e.code)}
        }
        attempt?.let { scanned ->
            val invitation=scanned.invitation
            val resolved=app.pairingHello(scanned,pcId) ?: throw RemoteError("closing")
            phase(ssl,10000) {
            send(Frame.Control(Type.PAIR_REQUEST, json("token" to invitation.token)))
            var result:Frame.Control
            while(true) {
                val frame=Protocol.read(ssl.inputStream)
                if(frame is Frame.Chunk) {recoveryFile(frame);continue}
                require(frame is Frame.Control)
                if(frame.type==Type.PAIR_RESULT || frame.type==Type.ERROR) {result=frame;break}
                when(frame.type) {
                    Type.PING->send(Frame.Control(Type.PONG,frame.body))
                    Type.MEMBERS->{
                        try {deferred.hold(frame)}catch(_:IllegalArgumentException) {
                            send(Frame.Control(Type.ERROR,json("code" to "unexpected_message")));throw RemoteError("unexpected_message")
                        }
                    }
                    Type.CLIPBOARD,Type.STATUS,Type.PEER_SETTINGS->deferred.hold(frame)
                    Type.FILE_BEGIN,Type.FILE_END->recoveryFile(frame)
                    else->throw RemoteError("unexpected_message")
                }
            }
            if (result.type == Type.ERROR) throw RemoteError(result.body["code"].asString)
            if (!result.body["accepted"].asBoolean) throw RemoteError(result.body["code"].asString)
            // Persist before processing any READY traffic, including immediate peer data.
            if(epoch!=dialEpoch.get())throw java.io.IOException("Stale pairing")
            authenticatedBinding=app.installSessionBinding(resolved,pcId,p.deviceId,setupHandshake=SetupHandshake(p.membership.generation,localSetup,remoteSetup,app.setupIdentity(p,name),hello.body["name"].asString,
                if(mesh)hello.body["addresses"]?.asJsonArray?.map {Endpoint.parse(it.asString)}.orEmpty() else emptyList()),onOutcome={sessionOutcome=it}) {authenticatedBinding=it}
                ?: throw java.io.IOException("Stale pairing")
            }
        }
        if(epoch!=dialEpoch.get() || !endpointAllowed(readPrefs(),endpoint))throw java.io.IOException("Policy changed")
        // This binding comes only from the authenticated dial or its PAIR_RESULT
        // transaction. Registry reads can revoke it, never replace its authority.
        val sessionBinding=requireNotNull(authenticatedBinding)
        require(sessionBinding.pcId==pcId && sessionBinding.pin==(attempt?.invitation?.fingerprint ?: p.peer?.pin))
        val boundPeer=Peer(attempt?.invitation?.name ?: p.peer!!.name,sessionBinding.pin,pcId,emptyList(),generation=sessionBinding.generation)
        capabilities=hello.body["capabilities"].asJsonArray.map {it.asString}.toSet()
        app.preferences.hello(sessionBinding,hello.body["name"].asString,if(mesh)hello.body["addresses"]?.asJsonArray?.map {Endpoint.parse(it.asString)}.orEmpty() else emptyList(),mesh,remoteSetup!=null,remoteSetup?.scope)
        if(readPrefs().membership.generation!=p.membership.generation)throw RemoteError("closing")
        val setupResult=app.evaluateSetupHello(sessionBinding,p.membership.generation,localSetup,remoteSetup,app.setupIdentity(p,name),p.membership.pendingLink?.id) {
            epoch==dialEpoch.get() && authenticatedBinding==sessionBinding
        }
        if(setupResult==SetupHelloResult.RECONNECT || setupResult==SetupHelloResult.SEPARATE) {
            setupRefreshRedial.set(true)
            throw RemoteError("closing")
        }
        val setupEnabled=setupResult==SetupHelloResult.SYNCHRONIZE
        val setupGeneration=p.membership.generation
        val setupId=localSetup.setupId
        membersFence=if(setupEnabled && setupId!=null)Triple(sessionBinding,setupGeneration,setupId) else null
        app.abandonClipboard(sessionBinding,sessionOnly=!mesh)
        settingsReady=!mesh
        app.queue.pendingPeer(pcId).filter {it.bindingGeneration==sessionBinding.generation && it.error=="unsupported_kind" && !it.rejectedCapabilities.startsWith("settings:") && it.rejectedCapabilities!=capabilities.sorted().joinToString(",") && (if(it.kind=="text")"clipboard_text" in capabilities else "files" in capabilities && (it.kind!="clipboard_image" || "clipboard_image" in capabilities))}.forEach {
            it.state="waiting";it.error="";it.nextAttempt=0;updateRow(it)
        }
        // A progressing upload can receive nothing until ACK. READY liveness
        // belongs to PING/PONG, write progress and post-END receipt deadlines.
        val admitted=app.withBinding(sessionBinding) {
            if(epoch!=dialEpoch.get())return@withBinding false
            // Persist READY evidence only after setup activation has accepted
            // this session; HELLO-triggered reconnects do not count as linked.
            check(app.preferences.connected(sessionBinding,endpoint)) {"Binding changed"}
            admissionSession=randomId()
            app.connection.admitted(sessionBinding,requireNotNull(admissionSession))
            ready=true;true
        }
        if(admitted!=true)throw RemoteError("closing")
        val notificationPostPosition=app.phoneNotifications.postPosition()
        onReady();ssl.soTimeout = 0
        val currentPaused=readPrefs().paused
        if(currentPaused!=p.paused)send(Frame.Control(Type.STATUS,json("paused" to currentPaused)))

        val network = RoutePolicy.active(cm)
        state.value = LinkState("Connected", network.label(endpoint.tailscale), if (attempt != null) "Paired with ${attempt.invitation.name}" else "",cellular=network.mobile,pairingPin=if(attempt!=null)attempt.invitation.fingerprint else "",pairingGeneration=sessionBinding.generation,capabilities=capabilities)
        val phoneAuthority=requireNotNull(freezeSession(sessionBinding))
        val ringNegotiated=mesh && CAP_RING in capabilities && p.allowRinging
        val batterySender=if(mesh && CAP_BATTERY in capabilities)launch {
            publishBattery(combine(phoneBattery(context),app.prefs.filter {it.loaded}) {sample,prefs->
                if(prefs.showBatteryLevel && !prefs.paused)sample else BatteryState()
            }.distinctUntilChanged(),SystemClock::elapsedRealtime) {sample->
                sendPhoneState(phoneAuthority,Frame.Control(Type.BATTERY_STATE,if(sample.level==null)json() else json("level" to sample.level,"charging" to requireNotNull(sample.charging))))
            }
        } else null
        val ringSender=if(ringNegotiated)launch {
            app.ringing.state.map {it.ringing}.distinctUntilChanged().collect {ringing->
                sendPhoneState(phoneAuthority,Frame.Control(Type.RING_STATE,json("ringing" to ringing)))
            }
        } else null
        val ringPolicy=launch {
            app.prefs.filter {it.loaded}.map {it.allowRinging}.distinctUntilChanged().collect {allowed->
                if(allowed!=p.allowRinging) {
                    // Stop and flush false using the old negotiation before reconnecting.
                    if(!allowed) {
                        withContext(Dispatchers.Main.immediate) {app.ringing.stop(RingStopSource.DISABLED)}
                        if(ringNegotiated)withTimeoutOrNull(2000) {sendPhoneState(phoneAuthority,Frame.Control(Type.RING_STATE,json("ringing" to false)))}
                    }
                    // Prefer a frame boundary; a stalled writer must not keep
                    // obsolete ring willingness advertised indefinitely.
                    if(withTimeoutOrNull(2000) {writes.withLock {retry()};true}!=true)retry()
                }
            }
        }
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
        val notificationSender=if(mesh && CAP_NOTIFICATIONS in capabilities && hello.body["role"]?.asString=="computer")launch {
            val session=NotificationMirrorSession(notificationPostPosition)
            val owner=admissionSession
            try {while(isActive && ready && admissionSession==owner) {
                val prefs=readPrefs();val peer=prefs.peer
                val allowed=peer!=null && prefs.notificationConsent.allows(prefs,peer,context.notificationAccess() && app.phoneNotifications.connected,true)
                val event=session.next(app.phoneNotifications.snapshot(),prefs.notificationConsent,allowed)
                if(event!=null && app.phoneNotifications.rate.take(requireNotNull(owner),event.action!=NotificationAction.UPSERT)) {
                    val emitted=sendNotification(sessionBinding,owner,session,event)
                    if(emitted)session.committed(event)
                }
                delay(100)
            }}finally {owner?.let {app.phoneNotifications.rate.forget(it)}}
        } else null
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
        var receivedSettings=!mesh
        suspend fun receiveSettings(frame:Frame.Control) {
            if(!mesh)throw MeshViolation("unexpected_message")
            val id=frame.body["transfer_id"].asString
            try {
                val duplicate=app.withBinding(sessionBinding) {
                    val duplicate=app.preferences.receiveSettings(sessionBinding,frame.body,app.queue)
                    if(readPrefs().peer?.clipboardSend!=true)app.queueMutex.withLock {app.queue.supersedeBoundText(pcId,sessionBinding.generation)}
                    duplicate
                } ?: throw java.io.IOException("Binding changed")
                receivedSettings=true;settingsEvents.trySend(Unit)
                send(Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to duplicate)))
            }catch(e:MeshViolation) {throw e}
            catch(_:Preferences.SettingsConflict) {send(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "id_conflict","retryable" to false)))}
            catch(e:CancellationException) {throw e}
            catch(_:Exception) {send(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "storage_failed","retryable" to true)))}
        }
        suspend fun receiveMembers(frame:Frame.Control) {
            if(remoteSetup==null)throw MeshViolation("unexpected_message")
            val id=frame.body["transfer_id"].asString
            if(!setupEnabled || setupId==null) {
                send(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "unsupported_kind","retryable" to false)));return
            }
            try {
                val duplicate=app.receiveSetupMembers(sessionBinding,setupGeneration,setupId,frame.body,app.setupIdentity(p,name)) {
                    ready && epoch==dialEpoch.get() && authenticatedBinding==sessionBinding
                }
                app.connection.setupProblem.value=null
                send(Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to duplicate)))
            }catch(e:SetupCapacity) {
                app.connection.setupProblem.value="Setup capacity reached"
                send(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "storage_failed","retryable" to true)))
            }catch(e:RemovedSetupSender) {throw RemoteError("closing")}
            catch(e:SetupUnavailable) {
                if(frame.body["setup_id"].asString!=setupId)app.connection.setupProblem.value="Direct link kept; separate setups not linked"
                send(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "unsupported_kind","retryable" to false)))
            }
            catch(e:SetupReceiptConflict) {send(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "id_conflict","retryable" to false)))}
            catch(e:MeshViolation) {throw e}
            catch(e:CancellationException) {throw e}
            catch(_:Exception) {send(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "storage_failed","retryable" to true)))}
        }
        suspend fun content(frame:Frame.Control) {
            try {when(frame.type) {
                Type.PEER_SETTINGS->receiveSettings(frame)
                Type.MEMBERS->receiveMembers(frame)
                Type.CLIPBOARD->send(app.clipboard.receive(pcId,frame.body,sessionBinding,mesh))
                else->Unit
            }}catch(e:MeshViolation) {send(Frame.Control(Type.ERROR,json("code" to e.code)));throw RemoteError(e.code)}
        }
        val settingsSender=if(mesh)launch {SessionTasks.settings({closeSocket(ssl)}) {
            val events=launch {app.prefs.map {it.devices.find {it.deviceId==pcId}}.distinctUntilChanged().collect {settingsEvents.trySend(Unit)}}
            var full=true
            try {while(isActive) {
                try {
                    val peer=SendFailures.local("local_storage") {requireNotNull(readPrefs().peer)}
                    if(full || peer.pendingSettings.isNotEmpty() || PairSettings.syncing(peer)) {
                        settingsReady=false
                        val snapshot=SendFailures.local("local_storage") {app.withBinding(sessionBinding) {app.preferences.prepareSettings(sessionBinding)}}
                            ?: throw RemoteError("closing")
                        val receipt=CompletableDeferred<Frame.Control>();settingsReceipt=snapshot["transfer_id"].asString to receipt
                        send(Frame.Control(Type.PEER_SETTINGS,snapshot))
                        val response=awaitReceipt(receipt);settingsReceipt=null
                        if(response.type==Type.ACK) {SendFailures.local("local_storage") {app.preferences.acknowledgeSettings(sessionBinding,snapshot)};full=false}
                        else {if(response.body["code"].asString!="storage_failed")throw RemoteError("unexpected_message");delay(60000)}
                        continue
                    }
                    if(receivedSettings)SendFailures.local("local_storage") {releaseSettingsBlocked(sessionBinding,peer)}
                    settingsReady=receivedSettings
                    settingsEvents.receive()
                }catch(e:CancellationException) {throw e}
                catch(e:TransportFailure) {throw e}
                catch(e:RemoteError) {throw e}
                catch(e:LocalSendFailure) {
                    // Local reads/preparation/ACK commits retain the exact pending
                    // snapshot. Keep the control reader and keepalive alive.
                    settingsReady=false;settingsReceipt=null;delay(60000)
                }
            }}finally {
                // Cancellation cannot interrupt blocking TLS read.
                events.cancel();settingsReceipt=null
            }
        }} else null
        val membersSender=if(setupEnabled && setupId!=null)launch {
            val events=Channel<Unit>(Channel.CONFLATED)
            val watch=launch {app.prefs.map {it.membership}.distinctUntilChanged().collect {events.trySend(Unit)}}
            var first=true
            try {while(isActive) {
                try {
                    val current=readPrefs().membership
                    if(current.generation!=setupGeneration || current.setupId!=setupId)break
                    val delivery=current.sends[pcId]
                    val canonical=MembersSnapshot("0".repeat(32),setupId,current.roster).canonical()
                    val confirmed=delivery?.acknowledged?.takeIf {it.isNotEmpty()}?.let {SetupWire.decode(com.google.gson.JsonParser.parseString(it).asJsonObject).canonical()}
                    if(first || delivery?.pending?.isNotEmpty()==true || confirmed!=canonical) {
                        val snapshot=app.withBinding(sessionBinding) {
                            check(epoch==dialEpoch.get() && ready)
                            app.preferences.prepareMembers(sessionBinding,setupGeneration,setupId) {epoch==dialEpoch.get() && ready && authenticatedBinding==sessionBinding}
                        } ?: throw SetupUnavailable()
                        val receipt=CompletableDeferred<Frame.Control>();membersReceipt=snapshot["transfer_id"].asString to receipt
                        send(Frame.Control(Type.MEMBERS,snapshot))
                        val response=awaitReceipt(receipt);membersReceipt=null
                        if(response.type==Type.ACK) {
                            app.withBinding(sessionBinding) {app.preferences.acknowledgeMembers(sessionBinding,setupGeneration,setupId,snapshot) {epoch==dialEpoch.get() && ready && authenticatedBinding==sessionBinding}}
                            first=false
                        } else if(response.body["code"].asString=="storage_failed")delay(60000)
                        else {
                            app.connection.setupProblem.value="Device linking paused: ${response.body["code"].asString}"
                            val blocked=readPrefs().membership.roster
                            while(readPrefs().membership.roster==blocked)events.receive()
                            continue
                        }
                    } else events.receive()
                }catch(e:CancellationException) {throw e}
                catch(e:TransportFailure) {closeSocket(ssl);throw e}
                catch(e:RemoteError) {closeSocket(ssl);throw e}
                catch(e:SetupUnavailable) {break}
                catch(e:RemovedSetupSender) {break}
                catch(_:Exception) {membersReceipt=null;delay(60000)}
            }}finally {membersReceipt=null;watch.cancel();events.close()}
        } else null
        deferred.afterInstall(sessionBinding.matches(readPrefs())).forEach {content(it)}
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
                if(!text)cancelFlush?.join()
                val prefs=readPrefs()
                if(!settingsReady) {waitMillis=1000;continue}
                if(text && prefs.peer?.clipboardSend!=true)app.queueMutex.withLock {app.queue.supersedeBoundText(sessionBinding.pcId,sessionBinding.generation)}
                val network=RoutePolicy.active(cm)
                if(!endpointAllowed(prefs,endpoint,network))throw TransportFailure(java.io.IOException("Policy changed"))
                // This lane has no active transfer here. A failed completion AND
                // failed fallback write can leave sending durable after the receipt.
                app.recoverDeliveryLane(text,sessionBinding)
                val lane=app.queue.pendingPeer(sessionBinding.pcId).filter {it.bindingGeneration==sessionBinding.generation && (it.kind=="text")==text && (!it.automatic || prefs.peer?.automaticMedia==true) && (it.kind!="clipboard_image" || prefs.peer?.clipboardSend==true)}
                retries.read(lane)
                val now=SystemClock.elapsedRealtime()
                deferred.entries.removeAll {it.value<=now || lane.none {row->row.mediaId==it.key}}
                val selection=QueueSchedule.choose(lane.filter {!DeliveryPolicy.hold(it.kind,it.automatic,prefs.paused,network.mobile,prefs.originals,(it.attempted || it.payloadAttempted),it.optimized)},now,retries::deadline,deferred)
                waitMillis=selection.waitMillis
                var row=selection.row
                if(row==null)continue
                selected=row
                val supported=if(text) "clipboard_text" in capabilities else "files" in capabilities && (row.kind!="clipboard_image" || "clipboard_image" in capabilities) && (!row.isFolder() || capabilities.containsAll(setOf(CAP_FILE_RESUME,CAP_FOLDER)))
                if(!supported) {row.state="failed";row.error=if(row.isFolder())"folder_update_required" else "unsupported_kind";row.nextAttempt=Long.MAX_VALUE;row.rejectedCapabilities=capabilities.sorted().joinToString(",");updateRow(row);waitMillis=0;continue}
                    val processed=if(text) transferText(row,sessionBinding,retries)
                    else {
                        if(DeliveryPolicy.optimize(row.kind,row.mime,network.mobile,prefs.originals,(row.attempted || row.payloadAttempted),row.optimized)) {
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
        // Flush durable mid-frame/offline cancels before any new file BEGIN.
        suspend fun flushCancels() {
            if(CAP_FILE_RESUME !in capabilities)return
            for(intent in app.queue.pendingCancels(sessionBinding.pcId)) {
                if(intent.generation==sessionBinding.generation && intent.pin==sessionBinding.pin)
                    send(Frame.Control(Type.CANCEL,json("transfer_id" to intent.transferId)))
                app.queue.deletePendingCancel(intent.peerId,intent.transferId)
            }
        }
        flushCancels()
        val sender=launchSender(false)
        val textSender=launchSender(true)
        val receiveSession=admissionSession
        val receiver=app.receivedFiles.connection(CapturedAuthority(sessionBinding,requireNotNull(receiveSession)),mesh,CAP_FILE_RESUME in capabilities) {ready && admissionSession==receiveSession && authenticatedBinding==sessionBinding}
        val receiveProgress=ProgressDeadline(60_000,SystemClock::elapsedRealtime)
        val receiveWake=Channel<Unit>(Channel.CONFLATED)
        val receiveWatchdog=launch(Dispatchers.Default) {
            while(isActive) {
                val snapshot=receiveProgress.snapshot()
                if(snapshot==null)receiveWake.receive()
                else {
                    val remaining=receiveProgress.remaining(snapshot)
                    if(remaining<=0) {
                        if(receiveProgress.expire(snapshot)) {closeSocket(ssl);throw RemoteError("timeout")}
                    } else withTimeoutOrNull(remaining) {receiveWake.receive()}
                }
            }
        }
        val fileLane=FileReceiveLane(receiver::requestFailure)
        val incomingPhase=FileReceiveState()
        // Cleanup owns an ID only while queued/running (at most 65 IDs).
        // Keep it fenced until both the file worker and purge have retired it.
        val cancelPurges=Channel<String>(64)
        val purgingCancels=java.util.concurrent.ConcurrentHashMap.newKeySet<String>()
        val cancelPurger=app.scope.launch(Dispatchers.IO) {
            for(id in cancelPurges)try {PostCommit.maintainSuspending {receiver.purgeCancelled(id)}}
                finally {purgingCancels.remove(id)}
        }
        val fileWorker=launch(Dispatchers.IO) {
            try {
                while(isActive) {
                    val frame=fileLane.receive()
                    val id=when(frame) {is Frame.Chunk->frame.id;is Frame.Control->frame.body["transfer_id"].asString}
                    if(receiver.isCancelled(id) && !(frame is Frame.Control && frame.type==Type.CANCEL))continue
                    try {
                        when(frame) {
                            is Frame.Chunk->{beforeReceiveChunk();receiver.chunk(frame);state.update {it.copy(incoming=it.incoming?.copy(received=receiver.receivedBytes))}}
                            is Frame.Control->when(frame.type) {
                                Type.FILE_BEGIN->{
                                    val reply=withTimeoutOrNull(90_000) {receiver.begin(frame.body)}
                                    if(!receiver.isCancelled(id)) {
                                        if(reply?.type==Type.NACK) {if(incomingPhase.clear(id))receiveProgress.complete()}
                                        else {
                                            receiveProgress.progress(120_000)
                                            state.update {it.copy(incoming=receiver.metadata?.let {meta->IncomingTransfer(pcId,meta.id,meta.name,receiver.receivedBytes,meta.size)})}
                                        }
                                        receiveWake.trySend(Unit)
                                        reply?.let {response->
                                            if(response.type==Type.FILE_READY)incomingPhase.ready(id,response.body["offset"].asLong) {send(response)}
                                            else send(response)
                                        }
                                        if(frame.body["resume"]?.asBoolean==true && reply==null)throw RemoteError("timeout")
                                    }
                                }
                                Type.FILE_END->{
                                    val reply=withTimeoutOrNull(120_000) {receiver.end(id)} ?: throw RemoteError("timeout")
                                    if(incomingPhase.clear(id))receiveProgress.complete();receiveWake.trySend(Unit)
                                    state.update {it.copy(incoming=null)}
                                    if(!receiver.isCancelled(id))send(reply)
                                }
                                Type.CANCEL->{receiver.cancel(id);state.update {it.copy(incoming=it.incoming?.takeUnless {transfer->transfer.transferId==id})}}
                                else->throw ReceiveViolation()
                            }
                        }
                    }catch(e:ReceiveViolation) {
                        if(!receiver.isCancelled(id)) {send(Frame.Control(Type.ERROR,json("code" to e.code)));throw RemoteError(e.code)}
                    }
                }
            }finally {closeSocket(ssl);withContext(NonCancellable) {receiver.disconnect()}}
        }
        suspend fun receiveFile(frame:Frame) {
            if(recoveryFiles.active) {
                recoveryFile(frame)
                if(recoveryFiles.active)receiveProgress.progress() else receiveProgress.complete()
                receiveWake.trySend(Unit);return
            }
            try {
                when(frame) {
                    is Frame.Chunk->{
                        incomingPhase.chunk(frame)
                        receiveProgress.progress();receiveWake.trySend(Unit)
                    }
                    is Frame.Control->when(frame.type) {
                        Type.FILE_BEGIN->{
                            val resume=frame.body["resume"]?.asBoolean==true
                            if((resume && CAP_FILE_RESUME !in capabilities) || frame.body.has("folder"))throw ReceiveViolation()
                            val id=frame.body["transfer_id"].asString
                            if(receiver.isPending(id) || id in purgingCancels)throw ReceiveViolation()
                            incomingPhase.begin(id,frame.body["size"].asLong,resume)
                            receiver.register(id)
                            receiveProgress.progress(90_000);receiveWake.trySend(Unit)
                        }
                        Type.FILE_END->{
                            incomingPhase.end(frame.body["transfer_id"].asString)
                            receiveProgress.progress(120_000);receiveWake.trySend(Unit)
                        }
                        Type.CANCEL->{
                            if(CAP_FILE_RESUME !in capabilities)throw ReceiveViolation()
                            val id=frame.body["transfer_id"].asString
                            val pending=receiver.requestCancel(id)
                            if(incomingPhase.clear(id))receiveProgress.complete()
                            receiveWake.trySend(Unit)
                            // Do not queue deletion behind prefix hashing or provider copy.
                            val partial=if(pending)true else app.queue.receivedFile(pcId,id)?.let {it.generation==sessionBinding.generation && it.state=="writing"}==true
                            if(partial && purgingCancels.add(id) && cancelPurges.trySend(id).isFailure) {
                                purgingCancels.remove(id);throw ReceiveViolation()
                            }
                            // Inactive durable partials need deletion, but no queued
                            // operation or tombstone. Repeated CANCEL is a no-op.
                            if(!pending)return
                        }
                        else->throw ReceiveViolation()
                    }
                }
                fileLane.offer(frame)
            }catch(e:ReceiveViolation) {send(Frame.Control(Type.ERROR,json("code" to e.code)));throw RemoteError(e.code)}
        }
        if(recoveryFiles.active) {receiveProgress.progress();receiveWake.trySend(Unit)}
        try {
            while (isActive) {
                val frame = Protocol.read(ssl.inputStream)
                if(frame is Frame.Chunk) {receiveFile(frame);continue}
                require(frame is Frame.Control)
                when (frame.type) {
                    Type.PING -> send(Frame.Control(Type.PONG, frame.body))
                    Type.PONG -> {
                        if(ping.pong(frame.body["nonce"].asString)) livenessWake.trySend(Unit)
                    }
                    Type.FILE_READY -> {
                        if(CAP_FILE_RESUME !in capabilities)throw RemoteError("unexpected_message")
                        val id=frame.body["transfer_id"].asString
                        val phase=filePhase?.takeIf {it.id==id} ?: cancelledFiles[id] ?: throw RemoteError("unexpected_message")
                        try {phase.response(frame)}catch(e:FilePhaseViolation) {send(Frame.Control(Type.ERROR,json("code" to e.code)));throw RemoteError(e.code)}
                    }
                    Type.ACK, Type.NACK -> {
                        val id=frame.body["transfer_id"].asString
                        val phase=filePhase?.takeIf {it.id==id} ?: cancelledFiles[id]
                        if(phase!=null) {
                            try {phase.response(frame)}catch(e:FilePhaseViolation) {send(Frame.Control(Type.ERROR,json("code" to e.code)));throw RemoteError(e.code)}
                            continue
                        }
                        val receipt = activeReceipt
                        val target=if(receiptReady && receipt?.first==id)receipt else textReceipt?.takeIf {it.first==id} ?: settingsReceipt?.takeIf {it.first==id} ?: membersReceipt?.takeIf {it.first==id}
                        if(target!=null && target===membersReceipt) {
                            val fence=membersFence ?: throw RemoteError("closing")
                            val current=readPrefs()
                            if(current.membership.generation!=fence.second || current.membership.setupId!=fence.third || !fence.first.matches(current))throw RemoteError("closing")
                        }
                        require(target != null && target.second.complete(frame))
                    }
                    Type.UNPAIR -> {
                        // The peer has ended this authenticated session. Clear
                        // visible authority before any DataStore observer can see
                        // removal; persistence failure cannot make this link READY.
                        bindingChanged();app.connection.sessionChanged(sessionBinding)
                        if(localScope==SetupScope.ENABLED && remoteSetup?.scope==SetupScope.ENABLED)
                            app.withdrawSetup(sessionBinding,setupGeneration) {epoch==dialEpoch.get() && authenticatedBinding==sessionBinding}
                        else app.forgetBinding(boundPeer,sessionBinding.phoneId,internalOutcome=sessionOutcome,remoteUnpair=true)
                        throw RemoteError("closing")
                    }
                    Type.RING -> {
                        if(!ringNegotiated) {
                            send(Frame.Control(Type.ERROR,json("code" to "unexpected_message")))
                            throw RemoteError("unexpected_message")
                        }
                        val before=app.ringing.state.value.ringing
                        val result=app.withBinding(sessionBinding) {
                            if(!ownsSession(phoneAuthority))throw RemoteError("closing")
                            app.ringing.command(frame.body["action"].asString=="start",hello.body["name"].asString) {ownsSession(phoneAuthority)}
                        } ?: throw RemoteError("closing")
                        if(before==result)sendPhoneState(phoneAuthority,Frame.Control(Type.RING_STATE,json("ringing" to result)))
                    }
                    Type.ERROR -> throw RemoteError(frame.body["code"].asString)
                    Type.STATUS -> Unit
                    Type.CLIPBOARD,Type.PEER_SETTINGS,Type.MEMBERS -> content(frame)
                    Type.FILE_BEGIN,Type.FILE_END,Type.CANCEL -> receiveFile(frame)
                    else -> { send(Frame.Control(Type.ERROR, json("code" to "unexpected_message"))); throw RemoteError("unexpected_message") }
                }
            }
        } finally { batterySender?.cancel();ringSender?.cancel();ringPolicy.cancel();closeSocket(ssl); notificationSender?.cancel(); monitor.cancel(); sender.cancel(); textSender.cancel(); writeWatchdog.cancel();settingsSender?.cancel();membersSender?.cancel();receiveWatchdog.cancel();receiveWake.close();fileLane.close();fileWorker.cancel()
            withContext(NonCancellable) {
                state.update {it.copy(incoming=null)}
                runCatching {fileWorker.join()};cancelledFiles.clear()
                cancelPurges.close();runCatching {cancelPurger.join()}
                runCatching {app.preferences.connected(sessionBinding,endpoint)}
            }
        }
    }
    private fun readControl(ssl: SSLSocket): Frame.Control = Protocol.read(ssl.inputStream).also { require(it is Frame.Control) } as Frame.Control
    @Volatile private var activeFolderRead:Pair<String,java.util.concurrent.atomic.AtomicBoolean>?=null
    internal var openPayload:(File)->java.io.FileInputStream={it.inputStream()}
    private suspend fun transfer(row: Outgoing,binding:DeliveryBinding,retries:RetryDeadlines):Boolean {
        val file = File(row.path)
        if (!file.exists()) { row.state="failed";row.error="local_file_missing";row.nextAttempt=Long.MAX_VALUE;updateRow(row);return true }
        val folder=row.isFolder()
        val readCancelled=java.util.concurrent.atomic.AtomicBoolean(false)
        if (!folder && file.length() != row.size) { localMismatch(row,"size_mismatch",retries);return true }
        val folderInput=if(folder)SendFailures.local("folder_read") {app.folderSends.open(row) {readCancelled.get() || filePhase?.cancelled==true}} else null
        val input=if(folder)null else SendFailures.local("local_read") {openPayload(file)}
        suspend fun seek(offset:Long) {if(folderInput!=null)folderInput.seek(offset) else requireNotNull(input).channel.position(offset)}
        suspend fun read(buffer:ByteArray,limit:Int):Int=if(folderInput!=null) {
            val b=if(limit==buffer.size)buffer else ByteArray(limit)
            folderInput.read(b).also {if(it>0 && b!==buffer)b.copyInto(buffer,0,0,it)}
        }else requireNotNull(input).read(buffer,0,limit)
        val readError=if(folder)"folder_read" else "local_read"
        var began=false;var ended=false
        if(folder)activeFolderRead=row.mediaId to readCancelled
        try {
            // Register before checking durable state: a discard either reaches
            // this token or is already visible here, including before BEGIN.
            if(folder) {
                val current=app.queue.delivery(row.mediaId,row.peerId)
                if(current==null || current.state !in setOf("waiting","failed","sending") || current.revision!=row.revision)return true
            }
            val digest=java.security.MessageDigest.getInstance("SHA-256")
            SendFailures.local(readError) {
                if(folderInput!=null) {if(row.attempted)folderInput.verify()} else {
                    val buffer=ByteArray(CHUNK_SIZE)
                    while(true) {currentCoroutineContext().ensureActive();val n=requireNotNull(input).read(buffer);if(n<0)break;digest.update(buffer,0,n)}
                    requireNotNull(input).channel.position(0)
                }
            }
            if(!folder && digest.digest().hex()!=row.hash) {localMismatch(row,"hash_mismatch",retries);return true}
            val resume=CAP_FILE_RESUME in capabilities && "files" in capabilities && !row.sensitive
            val phase=FileSendPhase(row.transferId,row.size,resume);filePhase=phase
            row.resumable=resume
            val receipt=phase.receipt;activeReceipt=row.transferId to receipt
            val emission=QueuedEmission(row,binding);fileEmission=emission
            if(!send(Frame.Control(Type.FILE_BEGIN,Staging.metadata(row).apply {if(resume)addProperty("resume",true);if(!folder && "mesh" in capabilities && row.hlc.isNotEmpty()) {addProperty("hlc",row.hlc.toBigInteger());addProperty("source_device",row.sourceDevice)}}),emission))return false
            began=true
            val readiness=if(resume)withTimeoutOrNull(120_000) {phase.readiness.await()} ?: throw TransportFailure(java.net.SocketTimeoutException("READY deadline expired")) else null
            if(phase.cancelled)return true
            val early=readiness?.takeIf {it.type==Type.NACK}
            var offset=readiness?.takeIf {it.type==Type.FILE_READY}?.body?.get("offset")?.asLong ?: 0L
            if(early==null)SendFailures.local(readError) {
                seek(offset)
            }
            row.sentBytes=offset
            state.update {it.copy(sent=offset)}
            val buffer=ByteArray(CHUNK_SIZE)
            var durableSent=offset
            while(early==null && offset<row.size) {
                currentCoroutineContext().ensureActive()
                if(phase.cancelled)return true
                val n=SendFailures.local(readError) {read(buffer,minOf(buffer.size.toLong(),row.size-offset).toInt()).also {if(it<=0)throw java.io.EOFException()}}
                send(Frame.Chunk(row.transferId,offset,buffer.copyOf(n)));offset+=n
                row.sentBytes=offset
                if(offset-durableSent>=ReceiveSpool.CHECKPOINT_BYTES) {PostCommit.maintainSuspending {updateRow(row)};durableSent=offset}
                state.update {it.copy(sent=offset)}
                yield()
            }
            if(phase.cancelled)return true
            val response=if(early!=null)early else {
                SendFailures.local(readError) {folderInput?.finish()}
                phase.ending();receiptReady=true
                send(Frame.Control(Type.FILE_END,json("transfer_id" to row.transferId)));ended=true
                beforeFileReceiptWait()
                awaitReceipt(receipt)
            }
            if(phase.cancelled)return true
            if(response.type==Type.ACK) {
                if(row.kind=="clipboard_image")PostCommit.maintainSuspending {app.savedClips.recordImage(row.mime,sensitive=row.sensitive) {file.inputStream()}}
                row.state="sent";row.error="";if(app.staging.completeFile(row) && folder)PostCommit.maintainSuspending {app.withAuthority(CapturedAuthority(binding,admissionSession)) {FolderSentNotifications.show(app.context,row,binding.pcId,readPrefs().devices)}}
            } else {
                val code=response.body["code"].asString;row.error=if(folder && early!=null && code=="disk_full")"folder_disk_full_early" else code;row.state="failed";if(code=="unsupported_kind")row.rejectedCapabilities=emission.rejectedCapabilities
                val decision=RetryPolicy.nack(code,row.mismatches,Random.nextDouble(.8,1.2))
                row.mismatches=decision.mismatches;retries.retryAfter(row,decision.waitMillis);updateRow(row)
                // A newer settings sync can finish before this NACK. Revalidate
                // after recording it as well as when the sync itself completes.
                if(code=="unsupported_kind")settingsEvents.trySend(Unit)
            }
            return true
        } catch(e:LocalSendFailure) {
            if(filePhase?.cancelled==true || readCancelled.get())return true
            if(folder && e.code in setOf("folder_changed","folder_access_lost") && !ended && (began || row.attempted)) {
                filePhase?.cancel()
                // Keep the existing durable cancel intent if this lane drops
                // before the control frame can be flushed.
                app.withAuthority(CapturedAuthority(binding)) {
                    app.queue.insertPendingCancel(PendingCancel().apply {
                        peerId=binding.pcId;transferId=row.transferId;pin=binding.pin;generation=binding.generation
                    })
                }
                try {
                    if(send(Frame.Control(Type.CANCEL,json("transfer_id" to row.transferId))))app.queue.deletePendingCancel(binding.pcId,row.transferId)
                }catch(_:TransportFailure) {retry()}
            }else if(began && !ended) {
                // A local read failed between complete frames. END with fewer bytes is
                // normative NACK size_mismatch and returns the peer's file lane to IDLE.
                filePhase?.ending();receiptReady=true
                send(Frame.Control(Type.FILE_END,json("transfer_id" to row.transferId)))
                awaitReceipt(requireNotNull(activeReceipt).second)
            }
            throw e
        } catch(e:TransportFailure) {
            if(app.queue.delivery(row.mediaId,row.peerId)?.state!="discarded") {row.state=if(!row.resumable || app.preferences.read().queue)"waiting" else "failed";row.error="connection_lost";row.nextAttempt=if(row.state=="failed")Long.MAX_VALUE else 0;runCatching {updateRow(row)}}
            throw e
        } finally {
            runCatching {input?.close();folderInput?.close()}
            if(activeFolderRead?.second===readCancelled)activeFolderRead=null
            if(began && row.state=="sending" && filePhase?.cancelled!=true)withContext(NonCancellable) {
                if(app.queue.delivery(row.mediaId,row.peerId)?.state!="discarded") {row.state=if(!row.resumable || app.preferences.read().queue)"waiting" else "failed";row.error="connection_lost";row.nextAttempt=if(row.state=="failed")Long.MAX_VALUE else 0;runCatching {updateRow(row)}}
            }
            filePhase?.takeIf {it.cancelled}?.let {cancelledFiles[it.id]=it}
            filePhase=null;activeReceipt=null;receiptReady=false;fileEmission=null
            state.update {it.copy(sent=0,total=0,transferMediaId="",transferKind="",transferFileName="")}
        }
    }
    private suspend fun awaitReceipt(receipt:CompletableDeferred<Frame.Control>):Frame.Control =
        withTimeoutOrNull(120000) {receipt.await()} ?: throw TransportFailure(java.net.SocketTimeoutException("Receipt deadline expired"))
    private suspend fun transferText(row: Outgoing,binding:DeliveryBinding,retries:RetryDeadlines):Boolean {
        val receipt=CompletableDeferred<Frame.Control>();textReceipt=row.transferId to receipt
        val emission=QueuedEmission(row,binding);textEmission=emission
        try {
            val body=Staging.clipboardMetadata(row)
            if("mesh" in capabilities) {body.addProperty("hlc",row.hlc.toBigInteger());body.addProperty("source_device",row.sourceDevice)}
            if(!send(Frame.Control(Type.CLIPBOARD,body),emission))return false
            val response=awaitReceipt(receipt)
            if(response.type==Type.ACK) {PostCommit.maintainSuspending {app.savedClips.recordText(row.text,sensitive=row.sensitive)};row.state="sent";row.error="";updateRow(row)}
            else {
                val code=response.body["code"].asString;if(code=="unsupported_kind")row.rejectedCapabilities=emission.rejectedCapabilities
                val decision=RetryPolicy.nack(code,row.mismatches,Random.nextDouble(.8,1.2))
                row.state="failed";row.error=code;retries.retryAfter(row,decision.waitMillis);row.mismatches=decision.mismatches;updateRow(row)
                if(code=="unsupported_kind")settingsEvents.trySend(Unit)
            }
            return true
        } catch(e: TransportFailure) {row.state="waiting";runCatching {updateRow(row)};throw e}
        finally {textReceipt=null;textEmission=null}
    }
    private suspend fun releaseSettingsBlocked(binding:DeliveryBinding,peer:Peer) {
        app.queue.pendingPeer(binding.pcId).filter {it.bindingGeneration==binding.generation && SettingsDeliveryPolicy.releasable(it,peer,capabilities)}.forEach {
            it.state="waiting";it.error="";it.nextAttempt=0;it.rejectedCapabilities="";updateRow(it)
        }
    }
    private suspend fun updateRow(row:Outgoing)=app.updateDelivery(row)
    private suspend fun localMismatch(row: Outgoing, code: String,retries:RetryDeadlines) {
        val decision=RetryPolicy.nack(code,row.mismatches,1.0)
        row.state="failed";row.error=code;row.mismatches=decision.mismatches;retries.retryAfter(row,decision.waitMillis);updateRow(row)
    }
    internal suspend fun sendNotification(binding:DeliveryBinding,owner:String?,session:NotificationMirrorSession,event:NotificationEvent):Boolean {
        var fence:Pair<Long,Long>?=null
        suspend fun admit():Boolean=app.withBinding(binding) {app.preferences.policy.locked {
            val current=readPrefs();val currentPeer=current.peer
            val generation=current.policyEpoch to app.preferences.notificationGeneration
            val authorized=currentPeer!=null && current.notificationConsent.allows(current,currentPeer,context.notificationAccess() && app.phoneNotifications.connected,true)
            if(fence!=null && fence!=generation || !ready || admissionSession!=owner ||
                !session.current(event,app.phoneNotifications.snapshot(),current.notificationConsent,authorized))false
            else {fence=generation;true}
        }} ?: false
        val frame=Frame.Control(Type.NOTIFICATION_EVENT,com.google.gson.Gson().toJsonTree(event).asJsonObject)
        return send(frame,admission={if(admit())frame else null})
    }

    internal suspend fun sendPhoneState(binding:DeliveryBinding,frame:Frame.Control):Boolean {
        // Each admission rebuilds the frame from current settings, so a Pause or
        // toggle raced before the first byte sends the current state, not the old one.
        suspend fun admit():Frame?=app.withBinding(binding.copy(sessionId=null)) {
            app.preferences.computerPolicy.withLock {
                if(!ownsSession(binding) || "mesh" !in capabilities)return@withBinding null
                val current=readPrefs()
                if(activeEndpoint?.let {endpointAllowed(current,it)}!=true)return@withBinding null
                when(frame.type) {
                    Type.BATTERY_STATE -> {
                        if(CAP_BATTERY !in capabilities)return@withBinding null
                        if(!current.showBatteryLevel || current.paused)Frame.Control(Type.BATTERY_STATE,json()) else frame
                    }
                    Type.RING_STATE -> {
                        if(CAP_RING !in capabilities)return@withBinding null
                        Frame.Control(Type.RING_STATE,json("ringing" to (current.allowRinging && !current.paused && app.ringing.state.value.ringing)))
                    }
                    else->null
                }
            }
        }
        return send(frame,admission={admit()})
    }

    internal var beforeFirstFrame:suspend (Outgoing)->Unit={}
    internal var beforeFileReceiptWait:suspend ()->Unit={}
    internal var beforeReceiveChunk:suspend ()->Unit={}
    internal var afterFileFrameFlushed:(String)->Unit={}
    internal suspend fun withWriteLockForTest(body:suspend ()->Unit)=writes.withLock {body()}
    private suspend fun send(frame: Frame,emission:QueuedEmission?=null,expectedSession:String?=null,onWritten:(()->Unit)?=null,admission:(suspend ()->Frame?)?=null):Boolean {
        // Validate local fields/encoding before touching a wire frame.
        SendFailures.local("invalid_payload") {Protocol.write(java.io.OutputStream.nullOutputStream(),frame)}
        if(emission!=null)beforeFirstFrame(emission.row)
        return writes.withLock write@{
        if(expectedSession!=null && (!ready || admissionSession!=expectedSession))return@write false
        val s = socket ?: throw TransportFailure(java.io.IOException("Disconnected"))
        if(frame is Frame.Control && frame.type==Type.MEMBERS) {
            val fence=membersFence ?: throw SetupUnavailable()
            val current=readPrefs()
            if(!ready || !fence.first.matches(current) || current.membership.generation!=fence.second ||
                current.membership.setupId!=fence.third || current.membership.withdrawn || current.peer?.setupScope!=SetupScope.ENABLED)throw SetupUnavailable()
        }
        if(frame is Frame.Chunk && cancelledFiles.containsKey(frame.id) || frame is Frame.Control && frame.type==Type.FILE_END && cancelledFiles.containsKey(frame.body["transfer_id"].asString))return@write false
        if(unpairing)throw TransportFailure(java.io.IOException("Unpairing"))
        if(frame is Frame.Chunk || (frame is Frame.Control && frame.type in setOf(Type.FILE_BEGIN,Type.CLIPBOARD))) {
            val current=SendFailures.local("local_storage") {readPrefs()}
            if(activeEndpoint?.let {endpointAllowed(current,it)}!=true || (pairingAttempt()?.invitation?.fingerprint ?: current.peer?.pin)!=activePin) {closeSocket(s);throw TransportFailure(java.io.IOException("Policy changed"))}
        }
        if(emission!=null) {
            if(!settingsReady)return@write false
            if(emission.row.sessionId.isNotEmpty() && emission.row.sessionId!=admissionSession)return@write false
            // Revalidate both lanes after preparation AND after waiting for the
            // frame writer. Discard, replacement, Pause and cost-policy changes
            // cannot authorize a first frame from a stale selection.
            val accepted=SendFailures.local("local_storage") {app.beginDelivery(emission.row,emission.binding,
                eligible={p,current,newer->
                    val network=RoutePolicy.active(cm)
                    EmissionPolicy.eligible(emission.row,current,p,emission.binding,
                        activeEndpoint?.let {endpointAllowed(p,it,network)}==true,network.mobile,capabilities,newer)},
                begin={p->emission.begin(requireNotNull(p.devices.find {it.deviceId==emission.binding.pcId}),capabilities)})}
            if(!accepted)return@write false
            if(frame is Frame.Control && frame.type==Type.FILE_BEGIN) {
                val row=emission.row
                state.update {it.copy(sent=0,total=row.size,transferMediaId=row.mediaId,transferKind=row.kind,transferFileName=row.fileName)}
            }
        }
        // Admission holds only local locks. Socket I/O owns just this peer's writer.
        val outgoing=if(admission==null)frame else admission() ?: return@write false
        transmit(s,outgoing,onWritten,admission)
        }
    }
    private suspend fun transmit(s:Socket,frame:Frame,onWritten:(()->Unit)?=null,admission:(suspend ()->Frame?)?=null):Boolean {
        val writingTransferId=when(frame) {is Frame.Chunk->frame.id;is Frame.Control->if(frame.type in setOf(Type.FILE_BEGIN,Type.FILE_END))frame.body["transfer_id"].asString else null}
        writing.progress(); writeWake.trySend(Unit)
        try { val emitted=withContext(Dispatchers.IO) {
            val output=ProgressOutput(s.getOutputStream()) {writing.progress();writeWake.trySend(Unit)}
            // Obtaining the socket or dispatcher can suspend past opt-out, Pause,
            // unpair or reconnect. Recheck immediately before the first byte.
            val outgoing=if(admission==null)frame else admission() ?: return@withContext false
            fileFrameWriter.write(writingTransferId,{
                Protocol.write(output,outgoing)
            },{
                onWritten?.invoke()
                writingTransferId?.let {afterFileFrameFlushed(it)}
            })
            true
        }; if(!emitted)return false;txAt = SystemClock.elapsedRealtime() }
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
        return true
    }

    private suspend fun lost(expected:Peer?,phoneId:String,outcome:ConnectionOutcome?) {if(app.loseBinding(expected,phoneId,outcome))state.value=LinkState("Pairing lost")}
    fun bindingChanged(lost: Boolean=false) { lastIssue=null;admissionSession=null;ready=false;capabilities=emptySet();rejections.clear();state.value=LinkState(if(lost)"Pairing lost" else "PC unreachable") }
    fun retry() { admissionSession=null;dialEpoch.incrementAndGet();closeSocket(); wake.trySend(Unit) }
    internal fun refreshSetup() {setupRefreshRedial.set(true);retry()}
    fun cancelTransfer(mediaId: String) {
        val binding=authenticatedBinding
        val session=admissionSession
        val flush=if(binding!=null && session!=null && ready && CAP_FILE_RESUME in capabilities) {
            app.scope.launch(start=CoroutineStart.LAZY) {
                try {
                    for(intent in app.queue.pendingCancels(binding.pcId)) {
                        if(intent.generation!=binding.generation || intent.pin!=binding.pin)continue
                        if(send(Frame.Control(Type.CANCEL,json("transfer_id" to intent.transferId)),expectedSession=session)) {
                            app.queue.deletePendingCancel(intent.peerId,intent.transferId)
                            cancelledFiles[intent.transferId]?.let {phase->
                                phase.readiness.complete(Frame.Control(Type.CANCEL,json("transfer_id" to phase.id)))
                                phase.receipt.complete(Frame.Control(Type.CANCEL,json("transfer_id" to phase.id)))
                            }
                        }
                    }
                }catch(_:Exception) {if(admissionSession==session)retry()}
            }.also {cancelFlush=it}
        } else null
        // Publish the flush barrier before waking/stopping the active operation.
        val emission=fileEmission?.takeIf {it.row.mediaId==mediaId}
        if(emission!=null && emission.cancel()) {
            val phase=filePhase
            if(phase!=null && CAP_FILE_RESUME in capabilities) {
                phase.cancel();cancelledFiles[phase.id]=phase
                // A torn frame cannot carry a following control message. The
                // durable intent is flushed by the next capable session.
                if(fileFrameWriter.cancel(phase.id))retry()
            } else retry()
        }
        flush?.start()
        activeFolderRead?.takeIf {it.first==mediaId}?.second?.set(true)
        // Already-emitted text consumes its own ACK/NACK without resetting files.
        textEmission?.takeIf {it.row.mediaId==mediaId}?.cancel()
    }
    internal var beforeUnpairRemoval:suspend ()->Unit={}
    suspend fun unpair(expected:ConfirmedBinding=ConfirmedBinding.from(app.prefs.value),setupMember:ConfirmedSetupMember?=null):Boolean = withContext(Dispatchers.IO + NonCancellable) {unpairs.withLock {
        var captured:CapturedConnection?=null
        var matched=false
        try {
            // Freeze normal writes/teardown while durable removal changes the
            // policy. Only this captured authenticated session may notify it.
            beforeUnpairRemoval()
            if(!LifecycleOrder.revoke({app.forgetBinding(rotateUnpaired=false,confirmed=expected,setupMember=setupMember,beforeRemoval={
                // Called only after the confirmed identity matched under the
                // binding lock. A stale confirmation must not close a new session.
                matched=true
                captured=(socket.takeIf {ready} as? SSLSocket)?.let {CapturedConnection(it,tcpSocket)}
                revoking=captured;unpairing=true;admissionSession=null;ready=false
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
    internal suspend fun awaitStopped() {job?.join()}
    fun stop() {
        admissionSession=null;ready=false; closeSocket(); job?.cancel()
        runCatching { cm.unregisterNetworkCallback(callback) }
        discoveryLease.close()
    }
}
