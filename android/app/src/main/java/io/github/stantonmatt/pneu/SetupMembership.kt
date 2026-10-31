package io.github.stantonmatt.pneu

import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

internal data class SetupHandshake(val generation:Long,val local:SetupHello,val remote:SetupHello?,val self:SetupIdentity,val remoteName:String,val remoteAddresses:List<Endpoint>)

internal fun AppGraph.setupIdentity(p:Prefs,name:String=p.selfName.ifEmpty {android.os.Build.MODEL})=
    SetupIdentity(p.deviceId,sha256(TlsIdentity(p.tlsAlias).certificate.encoded),utf8Prefix(name,128).ifEmpty {"Pneu"})

internal suspend fun AppGraph.evaluateSetupHello(binding:DeliveryBinding,generation:Long,local:SetupHello,remote:SetupHello?,
    self:SetupIdentity,discoveryId:String?,current:()->Boolean):SetupHelloResult=withBindingLifecycle {
    check(current()) {"Connection changed"}
    val result=preferences.setupHello(binding,generation,local,remote,self,discoveryId,current)
    if(result==SetupHelloResult.RECONNECT)retrySetupLinks()
    result
}
/** Public UI actions carry the same frozen authority used by other device actions. */
suspend fun AppGraph.linkExistingDevice(confirmed:ConfirmedBinding,displayedPrompts:List<SetupPrompt>?=null)=withContext(Dispatchers.IO) {withBindingLifecycle {
    preferences.requestSetupLink(confirmed,displayedPrompts);retrySetupLinks()
}}
suspend fun AppGraph.answerSetupPrompt(prompt:SetupPrompt,allow:Boolean)=withContext(Dispatchers.IO) {withBindingLifecycle {
    preferences.answerSetupPrompt(prompt,allow);if(allow)retrySetupLinks() else connection.refreshSetupPeer(prompt.pin)
}}
private const val LINK_DISCOVERY_MS=3000L
internal suspend fun AppGraph.retrySetupLinks() {
    val p=preferences.read()
    p.devices.filter {peer->peer.setupScope==SetupScope.ENABLED || p.membership.prompts.any {it.allowRequested && it.peerId==peer.deviceId}}.forEach {connection.refreshSetupPeer(it.pin)}
}
/** One task per durable action. Restart gives it a new full discovery window;
 * observations do not restart the timer, and no lifecycle lock spans the delay. */
internal fun AppGraph.resumeSetupLinkDiscovery()=scope.launch {
    initialized.await()
    prefs.filter {it.loaded}.map {it.membership.pendingLink?.id}.distinctUntilChanged().collectLatest {id->
        if(id==null)return@collectLatest
        while(isActive)try {
            withBindingLifecycle {retrySetupLinks()}
            delay(LINK_DISCOVERY_MS)
            withBindingLifecycle {
                var founded=false
                val self=setupIdentity(preferences.read())
                preferences.policy.locked {preferences.edit {p->
                    founded=SetupTransactions.finishLinkDiscovery(p,id,self)
                }}
                if(founded)retrySetupLinks()
            }
            break
        }catch(e:CancellationException) {throw e}
        catch(e:Exception) {android.util.Log.w("PneuLinkRecovery",e.javaClass.simpleName);delay(2000)}
    }
}
suspend fun AppGraph.removeSetupMember(confirmed:ConfirmedSetupMember):Boolean=withContext(Dispatchers.IO+NonCancellable) {
    val p=preferences.read()
    if(!confirmed.matches(p))return@withContext false
    val member=confirmed.member;val key=member.key;val setupGeneration=confirmed.setupGeneration
    val peer=p.devices.find {it.deviceId==member.deviceId && it.pin==member.fingerprint}
    if(peer!=null) {
        if(p.membership.generation!=setupGeneration)return@withContext false
        // Existing unpair captures the live socket before its durable removal.
        connection.unpair(requireNotNull(confirmed.binding),confirmed)
    } else withBindingLifecycle {
        val before=preferences.read()
        if(!confirmed.matches(before))return@withBindingLifecycle false
        preferences.edit {SetupTransactions.remove(it,key,setupGeneration)}
        cancelRemovedAttempt(setOf(key));true
    }
}
internal fun AppGraph.cancelRemovedAttempt(keys:Set<String>) {
    val attempt=connection.attempt.value ?: return
    if(keys.any(SetupAttemptTarget(attempt.deviceId,attempt.invitation.fingerprint)::matchesRemoval))endPairing(attempt)
}
internal suspend fun AppGraph.withdrawSetup(binding:DeliveryBinding,generation:Long,current:()->Boolean)=withContext(NonCancellable) {withBindingLifecycle {
    check(current()) {"Connection changed"}
    var removed=emptyList<Peer>()
    preferences.edit {removed=SetupTransactions.withdraw(it,binding,generation)}
    removed.forEach {changed(it);connection.retryPeer(it.pin)}
    connection.attempt.value?.let {endPairing(it)}
    for(peer in removed)completePeerReset(peer.deviceId)
}}
internal suspend fun AppGraph.receiveSetupMembers(binding:DeliveryBinding,generation:Long,setupId:String,body:com.google.gson.JsonObject,
    self:SetupIdentity,current:()->Boolean):Boolean=withContext(NonCancellable) {withBindingLifecycle {
    check(current()) {"Connection changed"}
    val p=preferences.read()
    val a=connection.attempt.value?.let {SetupAttemptTarget(it.deviceId,it.invitation.fingerprint)}
    // Check mismatch/deny before consulting even a retained shared receipt.
    if(p.membership.setupId!=setupId || body["setup_id"].asString!=setupId || p.membership.generation!=generation)throw SetupUnavailable()
    if(p.membership.denies(binding.pcId,binding.pin))throw RemovedSetupSender()
    val (duplicate,plan)=preferences.receiveMembers(binding,generation,setupId,body,self,a,queue,current=current)
    plan?.let {
        it.removed.forEach {peer->changed(peer);connection.retryPeer(peer.pin)}
        cancelRemovedAttempt(it.removedKeys)
        if(it.state.withdrawn)connection.attempt.value?.let {a->endPairing(a)}
    }
    // Worker/content publication is gated by resetPeers, and startup/dial always
    // finishes these intents. Do not ACK until the dependent Room work is done.
    for(id in preferences.read().resetPeers)completePeerReset(id)
    preferences.finishMembersIntents(queue)
    duplicate
}}
internal suspend fun AppGraph.reconcileSetupCandidates()=withBindingLifecycle {reconcileSetupCandidatesLocked()}
internal suspend fun AppGraph.reconcileSetupCandidatesLocked() {
    val p=preferences.read();val state=p.membership
    if(state.setupId==null || state.withdrawn)return
    val attempt=connection.attempt.value?.let {SetupAttemptTarget(it.deviceId,it.invitation.fingerprint)}
    val plan=SetupReconcile.retained(state,p.devices,p.deviceId,attempt)
    if(plan.installed.isEmpty())return
    preferences.policy.locked {preferences.edit {data->
        val current=SetupStorage.read(data)
        if(current.generation!=state.generation)return@edit
        SetupTransactions.commit(data,SetupReconcile.retained(current,PeerRegistry.read(data),p.deviceId,attempt))
    }}
    for(id in preferences.read().resetPeers)completePeerReset(id)
}
