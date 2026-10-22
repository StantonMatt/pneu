package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import java.io.File

/** Phone stays a client. Every pin owns an independent supervisor, discovery
 * filter, backoff, TLS session, file lane and text lane. */
class Connections(private val app:AppGraph) {
    internal data class Installation(val attemptId:String,val binding:DeliveryBinding)
    internal val installation=MutableStateFlow<Installation?>(null)
    @Volatile internal var lastStartedAttemptId:String?=null
    internal fun installed(attempt:PairingAttempt,binding:DeliveryBinding) {installation.value=Installation(attempt.id,binding)}
    private val activeAttempt=MutableStateFlow<PairingAttempt?>(null)
    val attempt=activeAttempt.asStateFlow()
    internal fun setAttempt(value:PairingAttempt?) {activeAttempt.value=value;value?.let {pairingPin=it.invitation.fingerprint;lastStartedAttemptId=it.id};publish()}
    internal fun attemptFor(pin:String?)=attempt.value?.takeIf {pin==null || it.invitation.fingerprint==pin}
    val state=MutableStateFlow(LinkState()) // Aggregate status; action eligibility is computed from every recipient.
    val devices=MutableStateFlow<Map<String,LinkState>>(emptyMap())
    @Volatile private var workers:Map<String,ConnectionManager> = emptyMap()
    @Volatile private var pairingResult:LinkState?=null
    @Volatile private var pairingPin:String?=null
    private var job:Job?=null
    private val watchers=mutableMapOf<String,Job>()
    private var stopped:List<ConnectionManager> = emptyList()
    val connected get()=workers.values.any {it.connected}
    fun connected(peerId:String)=app.prefs.value.devices.find {it.deviceId==peerId}?.let {workers[it.pin]?.connected}==true
    fun supports(capability:String)=workers.values.any {it.supports(capability)}
    fun supports(peerId:String,capability:String)=app.prefs.value.devices.find {it.deviceId==peerId}?.let {workers[it.pin]?.supports(capability)}==true
    internal fun freezeSession(binding:DeliveryBinding)=workers[binding.pin]?.freezeSession(binding)
    internal fun ownsSession(binding:DeliveryBinding)=workers[binding.pin]?.ownsSession(binding)==true
    fun clipboardAvailable(p:Prefs=app.prefs.value)=
        app.recipients(p,"text").isNotEmpty() || app.recipients(p,"clipboard_image").isNotEmpty()
    fun connectivity()=RoutePolicy.active(app.context.getSystemService(android.net.ConnectivityManager::class.java)).connectivity
    fun cellular()=connectivity().mobile
    private fun publish() {
        val p=app.prefs.value
        devices.value=p.devices.associate {it.deviceId to (workers[it.pin]?.state?.value ?: LinkState(if(it.pairingLost)"Pairing lost" else "PC unreachable"))}
        state.value=LinkProjection.aggregate(p,workers.mapValues {it.value.state.value},pairingResult,pairingPin,attempt.value)
    }
    fun start(scope:CoroutineScope) {
        if(job?.isActive==true) {publish();return}
        val previous=job
        job=scope.launch {
            previous?.join();workers=emptyMap();watchers.clear();app.initialized.await()
            try {coroutineScope {
                combine(app.prefs.filter {it.loaded},attempt) {p,a->p to a}.map {
                    // The two flows can publish in either order after installation.
                    // Read them together under the binding lock before retiring workers.
                    // A transient settings read must retry with the workers intact.
                    app.connectionView()
                }.recoverReads().collect {view->
                    val p=view.prefs;val a=view.attempt
                    val pins=(p.devices.filter {!it.pairingLost}.map {it.pin}+listOfNotNull(a?.invitation?.fingerprint)).toSet()
                    val removed=workers.filterKeys {it !in pins}.values.toList()
                    watchers.keys.filter {it !in pins}.forEach {watchers.remove(it)?.cancel()}
                    workers=workers.filterKeys {it in pins};removed.forEach {it.stop()};removed.forEach {it.awaitStopped()}
                    for(pin in pins-workers.keys) {
                        val worker=ConnectionManager(app,pin)
                        worker.onUnauthorized={onUnauthorized?.invoke()};worker.openPayload={openPayload(it)}
                        worker.beforeFirstFrame={beforeFirstFrame(it)};worker.beforeUnpairRemoval={beforeUnpairRemoval()}
                        workers=workers+(pin to worker)
                        worker.start(this)
                        watchers[pin]=launch {worker.state.collect {publish()}}
                    }
                    publish()
                }
            }} finally {workers.values.forEach {it.stop()};workers.values.forEach {it.awaitStopped()}}
        }
    }
    fun kick()=workers.values.forEach {it.kick()}
    fun retry()=workers.values.forEach {it.retry()}
    fun retryPeer(pin:String?) {workers[pin]?.retry()}
    internal fun pairingFailed(message:String) {pairingResult=LinkState(pairing=message);publish()}
    internal fun clearPairingResult(pin:String?=null) {
        pairingResult=null;lastStartedAttemptId=null
        pin?.let {workers[it]?.state?.update {state->state.withPairing("")}}
        publish()
    }
    internal fun sessionChanged(binding:DeliveryBinding) {
        if(workers[binding.pin]?.authenticatedAs(binding)==true)publish()
    }
    fun bindingChanged(lost:Boolean=false,peerId:String?=null) {
        val pin=app.prefs.value.devices.find {it.deviceId==peerId}?.pin
        workers.filterKeys {it==pin}.values.forEach {it.bindingChanged(lost);it.retry()}
        // Durable revocation callers must not return while aggregate UI/actions
        // still advertise the revoked session. Flow watchers may run later.
        publish()
    }
    fun cancelTransfer(mediaId:String,peerId:String?=null)=workers.filterKeys {peerId==null || app.prefs.value.devices.find {peer->peer.deviceId==peerId}?.pin==it}.values.forEach {it.cancelTransfer(mediaId)}
    suspend fun unpair(expected:ConfirmedBinding=ConfirmedBinding.from(app.prefs.value)):Boolean =
        workers[expected.pin]?.unpair(expected) ?: app.forgetBinding(confirmed=expected)
    fun stop() {stopped=workers.values.toList();stopped.forEach {it.stop()};job?.cancel();workers=emptyMap();publish()}
    internal suspend fun awaitStopped() {job?.join();stopped.forEach {it.awaitStopped()}}
    internal val discoveryActive get()=workers.values.any {it.discoveryActive}
    internal val multicastHeld get()=workers.values.any {it.multicastHeld}
    internal val discoveryWatchCount get()=workers.values.sumOf {it.discoveryWatchCount}
    internal var onUnauthorized:(()->Unit)?=null
    internal var openPayload:(File)->java.io.FileInputStream={it.inputStream()}
    internal var beforeFirstFrame:suspend (Outgoing)->Unit={}
    internal var beforeUnpairRemoval:suspend ()->Unit={}
    internal suspend fun withWriteLockForTest(body:suspend ()->Unit)=requireNotNull(workers.values.firstOrNull()).withWriteLockForTest(body)
    internal fun candidates(p:Prefs)=workers[attempt.value?.invitation?.fingerprint ?: p.peer?.pin]?.candidates(p) ?: ConnectionManager(app).candidates(p)
}
