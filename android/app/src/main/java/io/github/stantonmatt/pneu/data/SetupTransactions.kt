package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.JsonObject
import io.github.stantonmatt.pneu.protocol.*

internal enum class SetupHelloResult { DIRECT, WAIT, RECONNECT, SYNCHRONIZE, SEPARATE }
internal object SetupTransactions {
    private val revision=longPreferencesKey("pairingRevision")
    private val deviceId=stringPreferencesKey("deviceId")
    private fun member(p:MutablePreferences,id:String,pin:String,role:String,name:String,now:Long)=
        MeshClock.next(p,p[deviceId].orEmpty(),false,now).let {SetupMember(id,pin,role,name,"member",it.hlc.toString(),it.source)}
    fun paired(p:MutablePreferences,state:SetupState,peer:Peer,prior:Peer?,choice:Boolean,fresh:Boolean,selfPin:String?=null,now:Long=System.currentTimeMillis()) {
        require(state.roster.none {it.active && it.fingerprint==peer.pin && it.deviceId!=peer.deviceId}) {"Conflicting peer identity"}
        var next=state
        val key="${peer.deviceId}/${peer.pin}"
        if(state.denies(peer.deviceId,peer.pin)) {
            check(fresh) {"A fresh confirmed pairing is required after removal"}
            next=next.copy(barriers=next.barriers+(key to next.barriers.getValue(key).copy(cleared=true)))
        }
        if(next.withdrawn) {
            check(fresh) {"A fresh confirmed pairing is required after withdrawal"}
            val ownKey="${p[deviceId]}/$selfPin"
            val own=next.barriers[ownKey]
            next=next.copy(withdrawn=false,barriers=if(own!=null)next.barriers+(ownKey to own.copy(cleared=true)) else next.barriers)
        }
        if(prior!=null && prior.pin!=peer.pin && next.member(prior.deviceId)?.fingerprint==prior.pin) {
            val old=next.member(prior.deviceId)!!
            val stamp=MeshClock.next(p,p[deviceId].orEmpty(),false,now)
            val removal=old.copy(status="removed",hlc=stamp.hlc.toString(),writer=stamp.source,reach=null)
            next=next.copy(roster=next.roster.filter {it.key!=old.key}+removal,
                barriers=next.barriers+(old.key to RemovalBarrier(removal)))
        }
        // A fresh pair may clear the local deny, but only the authenticated
        // reciprocal HELLO below authorizes exporting the new binding.
        SetupReconcile.capacity(next,PeerRegistry.read(p).filter {it.deviceId!=peer.deviceId}+peer)
        SetupStorage.write(p,next)
    }
    /** Token completion has its own founding authority. A migration assignment
     * committed since the token HELLO is rechecked against the same remote ID. */
    fun tokenHello(p:MutablePreferences,binding:DeliveryBinding,remote:SetupHello?,self:SetupIdentity,
                   now:Long=System.currentTimeMillis(),newId:()->String={randomId()}):SetupHelloResult {
        val state=SetupStorage.read(p)
        val peer=PeerRegistry.read(p).single {it.deviceId==binding.pcId && it.pin==binding.pin && it.generation==binding.generation}
        return hello(p,binding,state.generation,state.hello(peer.setupScope),remote,self,now,newId,permitReset=true,tokenPairing=true)
    }
    fun hello(p:MutablePreferences,binding:DeliveryBinding,generation:Long,local:SetupHello,remote:SetupHello?,
              self:SetupIdentity,now:Long=System.currentTimeMillis(),newId:()->String={randomId()},permitReset:Boolean=false,tokenPairing:Boolean=false,discoveryId:String?=null):SetupHelloResult {
        var state=SetupStorage.read(p);val peers=PeerRegistry.read(p)
        val peer=peers.find {it.deviceId==binding.pcId && it.pin==binding.pin && it.generation==binding.generation} ?: throw SetupUnavailable()
        if(state.generation!=generation || !permitReset && binding.pcId in PeerRegistry.resets(p))throw SetupUnavailable()
        if(state.withdrawn)return SetupHelloResult.DIRECT
        if(state.denies(peer.deviceId,peer.pin))throw RemovedSetupSender()
        if(remote==null)return SetupHelloResult.DIRECT
        // Captured local HELLO is authority only for its original assignment and
        // decision. An action during the handshake forces a fresh HELLO.
        if(local!=state.hello(peer.setupScope) || !tokenPairing && state.pendingLink?.let {it.id!=discoveryId}==true)return SetupHelloResult.RECONNECT
        fun saveScope(scope:SetupScope,next:SetupState) {
            PeerRegistry.write(p,peers.map {if(it==peer)it.copy(setupDecision=scope) else it})
            SetupStorage.write(p,next)
        }
        if(peer.setupScope==SetupScope.DISABLED || remote.scope==SetupScope.DISABLED)return SetupHelloResult.DIRECT
        // Only a HELLO from a dial started after this Link action is discovery
        // evidence. An ask peer can prevent founding without authorizing adoption.
        state.pendingLink?.takeIf {it.id==discoveryId && it.setupGeneration==generation &&
            SetupLinkBinding(peer.deviceId,peer.pin,peer.generation) in it.bindings}?.let {pending->
            remote.setupId?.let {id->
                state=state.copy(pendingLink=pending.copy(advertised=pending.advertised.filterNot {it.peerId==peer.deviceId}+
                    SetupLinkAdvertisement(peer.deviceId,peer.name,id,peer.pin,peer.generation)))
                SetupStorage.write(p,state)
            }
        }
        if(state.setupId!=null && remote.setupId!=null && state.setupId!=remote.setupId) {
            saveScope(SetupScope.DISABLED,state.copy(prompts=state.prompts.filterNot {it.peerId==peer.deviceId},
                sends=state.sends-peer.deviceId,separateLinks=state.separateLinks+peer.deviceId))
            return SetupHelloResult.SEPARATE
        }
        if(peer.setupScope==SetupScope.ASK && remote.scope==SetupScope.ENABLED) {
            val prompt=state.prompts.find {it.peerId==peer.deviceId && it.pin==peer.pin && it.bindingGeneration==peer.generation && it.setupGeneration==state.generation}
            if(state.prompts.any {it.allowRequested && it.setupGeneration==state.generation}) {
                // Consent is applied only after this refreshed compatible HELLO.
                enableConsent(p,state,peers)
                return SetupHelloResult.RECONNECT
            }
            if(prompt==null)SetupStorage.write(p,state.copy(prompts=state.prompts.filterNot {it.peerId==peer.deviceId}+
                SetupPrompt(peer.deviceId,peer.pin,peer.generation,state.generation,peer.name)))
            return SetupHelloResult.WAIT
        }
        if(peer.setupScope!=SetupScope.ENABLED || remote.scope!=SetupScope.ENABLED)return SetupHelloResult.WAIT
        if(state.setupId==null && remote.setupId==null && (!tokenPairing || self.id>peer.deviceId))return SetupHelloResult.WAIT
        var next=state
        var assigned=false
        if(state.setupId==null) {
            val id=remote.setupId ?: generateSequence(newId).first {it !in state.retiredIds}
            // HELLO installs self before the first snapshot. Remember that admission until it arrives.
            next=state.copy(pendingLink=null,initialSnapshotPending=remote.setupId!=null,setupId=id,generation=Math.addExact(state.generation,1),prompts=emptyList(),sends=emptyMap(),receipts=emptyList(),
                roster=listOf(member(p,self.id,self.pin,self.role,self.name,now)))
            assigned=true
        }
        val identityConflict=next.roster.any {it.active &&
            (it.deviceId==peer.deviceId && it.fingerprint!=peer.pin || it.fingerprint==peer.pin && it.deviceId!=peer.deviceId)}
        if(identityConflict) {
            saveScope(SetupScope.DISABLED,state.copy(prompts=state.prompts.filterNot {it.peerId==peer.deviceId},
                conflicts=(state.conflicts+SetupConflict(peer.deviceId,peer.pin,"Identity already bound")).distinct().takeLast(64)))
            return SetupHelloResult.DIRECT
        }
        if(next.member(peer.deviceId)==null) {
            next=next.copy(roster=next.roster.filterNot {it.key=="${peer.deviceId}/${peer.pin}"}+
                member(p,peer.deviceId,peer.pin,peer.role,peer.name,now))
        }
        next=next.copy(separateLinks=next.separateLinks-peer.deviceId)
        SetupReconcile.capacity(next,peers);SetupStorage.write(p,next)
        return if(assigned)SetupHelloResult.RECONNECT else if(local.setupId==next.setupId && remote.setupId==next.setupId)SetupHelloResult.SYNCHRONIZE else SetupHelloResult.WAIT
    }
    private fun enableConsent(p:MutablePreferences,state:SetupState,peers:List<Peer>,selected:String?=null) {
        PeerRegistry.write(p,peers.map {if(it.setupScope==SetupScope.ASK || it.deviceId==selected)
            it.copy(setupDecision=SetupScope.ENABLED) else it})
        SetupStorage.write(p,state.copy(prompts=emptyList(),separateLinks=state.separateLinks-
            peers.filter {it.setupScope==SetupScope.ASK || it.deviceId==selected}.map {it.deviceId}.toSet()))
    }
    fun link(p:MutablePreferences,confirmed:ConfirmedBinding,displayedPrompts:List<SetupPrompt>?=null) {
        val state=SetupStorage.read(p)
        check(displayedPrompts==null || state.prompts==displayedPrompts) {"Link request changed"}
        check(!state.withdrawn) {"Pair again before linking devices"}
        val peers=PeerRegistry.read(p)
        check(confirmed.matches(Prefs(deviceId=p[deviceId].orEmpty(),peers=peers))) {"Pairing changed"}
        val prompt=state.prompts.firstOrNull {offer->offer.setupGeneration==state.generation && peers.any {
            it.deviceId==offer.peerId && it.pin==offer.pin && it.generation==offer.bindingGeneration && it.setupScope==SetupScope.ASK}}
        if(prompt!=null) {
            answer(p,prompt,true)
            // A selected disabled binding is an explicit opt-in, even when Link acts as Allow.
            PeerRegistry.write(p,peers.map {if(it.deviceId==confirmed.pcId && it.setupScope==SetupScope.DISABLED)
                it.copy(setupDecision=SetupScope.ENABLED) else it})
            return
        }
        enableConsent(p,state,peers,confirmed.pcId)
        if(state.setupId==null) {
            val covered=PeerRegistry.read(p).filter {it.setupScope==SetupScope.ENABLED}
                .map {SetupLinkBinding(it.deviceId,it.pin,it.generation)}
            val advertisements=state.pendingLink?.advertised.orEmpty().filter {
                SetupLinkBinding(it.peerId,it.pin,it.bindingGeneration) in covered}
            SetupStorage.write(p,SetupStorage.read(p).copy(pendingLink=PendingSetupLink(randomId(),state.generation,covered,advertisements)))
        }
    }
    fun answer(p:MutablePreferences,prompt:SetupPrompt,allow:Boolean) {
        val state=SetupStorage.read(p);val peers=PeerRegistry.read(p)
        check(state.generation==prompt.setupGeneration && prompt in state.prompts && peers.any {
            it.deviceId==prompt.peerId && it.pin==prompt.pin && it.generation==prompt.bindingGeneration && it.setupScope==SetupScope.ASK}) {"Link request changed"}
        if(allow)SetupStorage.write(p,state.copy(prompts=state.prompts.map {if(it==prompt)it.copy(allowRequested=true) else it}))
        else {
            PeerRegistry.write(p,peers.map {if(it.deviceId==prompt.peerId)it.copy(setupDecision=SetupScope.DISABLED) else it})
            SetupStorage.write(p,state.copy(prompts=state.prompts-prompt,sends=state.sends-prompt.peerId))
        }
    }
    /** Called only after a complete discovery window; the durable nonce fences replaced actions. */
    fun finishLinkDiscovery(p:MutablePreferences,id:String,self:SetupIdentity,now:Long=System.currentTimeMillis(),newId:()->String={randomId()}):Boolean {
        val state=SetupStorage.read(p);val pending=state.pendingLink ?: return false
        if(pending.id!=id || pending.setupGeneration!=state.generation || state.setupId!=null || state.withdrawn)return false
        if(pending.advertised.isNotEmpty())return false
        val next=state.copy(setupId=generateSequence(newId).first {it !in state.retiredIds},generation=Math.addExact(state.generation,1),
            pendingLink=null,prompts=emptyList(),sends=emptyMap(),receipts=emptyList(),
            roster=listOf(member(p,self.id,self.pin,self.role,self.name,now)))
        SetupReconcile.capacity(next,PeerRegistry.read(p));SetupStorage.write(p,next)
        return true
    }
    fun commit(p:MutablePreferences,plan:SetupPlan,sender:Peer?=null,stage:(String)->Unit={}):SetupState {
        var generation=p[revision] ?: 0
        val installed=plan.installed.associate {peer->peer.deviceId to peer.copy(generation=Math.addExact(generation,1).also {generation=it})}
        val peers=plan.peers.map {installed[it.deviceId] ?: it}
        val notices=installed.values.map {peer->SetupNotice("${plan.state.generation}/${peer.deviceId}/${peer.generation}",peer.deviceId,peer.name,
            peer.addedViaId ?: requireNotNull(sender).deviceId,peer.addedViaName ?: requireNotNull(sender).name,plan.alsoLinked)}
        val state=plan.state.copy(notices=(plan.state.notices+notices).takeLast(64))
        plan.observed.forEach {MeshClock.observe(p,it,plan.validatedAt)}
        stage("membersClockStaged")
        PeerRegistry.write(p,peers);p[revision]=generation
        stage("membersPeersStaged")
        (plan.removed+installed.values).forEach {PeerRegistry.reset(p,it.deviceId)}
        stage("membersResetsStaged")
        SetupStorage.write(p,state)
        return state
    }
    fun remove(p:MutablePreferences,key:String,generation:Long,now:Long=System.currentTimeMillis()):List<Peer> {
        val state=SetupStorage.read(p)
        check(!state.withdrawn && state.setupId!=null && state.generation==generation) {"Setup changed"}
        val target=state.roster.single {it.key==key && it.active}
        check(target.deviceId!=p[deviceId]) {"Cannot remove this device locally"}
        val stamp=MeshClock.next(p,p[deviceId].orEmpty(),false,now)
        val removed=target.copy(status="removed",hlc=stamp.hlc.toString(),writer=stamp.source,reach=null)
        var next=state.copy(roster=state.roster.map {if(it.key==key)removed else it},barriers=state.barriers+(key to RemovalBarrier(removed)))
        if(next.roster.none {it.active && it.deviceId!=p[deviceId]})next=SetupReconcile.retire(next,false)
        val peers=PeerRegistry.read(p);val forgotten=peers.filter {it.deviceId==target.deviceId && it.pin==target.fingerprint}
        SetupReconcile.capacity(next,peers-forgotten.toSet())
        PeerRegistry.write(p,peers-forgotten.toSet());forgotten.forEach {PeerRegistry.reset(p,it.deviceId)}
        SetupStorage.write(p,next);return forgotten
    }
    fun withdraw(p:MutablePreferences,binding:DeliveryBinding,generation:Long):List<Peer> {
        val state=SetupStorage.read(p);val peers=PeerRegistry.read(p)
        check(state.generation==generation && peers.any {it.deviceId==binding.pcId && it.pin==binding.pin && it.generation==binding.generation && it.setupScope==SetupScope.ENABLED})
        val removed=peers.filter {it.setupScoped}
        PeerRegistry.write(p,peers-removed.toSet());removed.forEach {PeerRegistry.reset(p,it.deviceId)}
        SetupStorage.write(p,SetupReconcile.retire(state,true))
        return removed
    }
}

internal suspend fun Preferences.setupHello(binding:DeliveryBinding,generation:Long,local:SetupHello,remote:SetupHello?,self:SetupIdentity,discoveryId:String?=null,current:()->Boolean={true}):SetupHelloResult=policy.locked {
    var result=SetupHelloResult.DIRECT
    edit {check(current()) {"Connection changed"};result=SetupTransactions.hello(it,binding,generation,local,remote,self,discoveryId=discoveryId)}
    result
}
internal suspend fun Preferences.requestSetupLink(confirmed:ConfirmedBinding,displayedPrompts:List<SetupPrompt>?=null) =policy.locked {
    edit {SetupTransactions.link(it,confirmed,displayedPrompts)}
}
internal suspend fun Preferences.answerSetupPrompt(prompt:SetupPrompt,allow:Boolean)=policy.locked {
    edit {SetupTransactions.answer(it,prompt,allow)}
}
internal fun checkSetup(p:androidx.datastore.preferences.core.Preferences,binding:DeliveryBinding,generation:Long,setupId:String):Pair<SetupState,Peer> {
    val state=SetupStorage.read(p);val peer=PeerRegistry.read(p).find {it.deviceId==binding.pcId && it.pin==binding.pin && it.generation==binding.generation} ?: throw SetupUnavailable()
    if(state.generation!=generation || state.setupId!=setupId || state.withdrawn || peer.setupScope!=SetupScope.ENABLED ||
        binding.pcId in PeerRegistry.resets(p) || peer.pairingLost)throw SetupUnavailable()
    if(state.denies(peer.deviceId,peer.pin))throw RemovedSetupSender()
    return state to peer
}
internal suspend fun Preferences.prepareMembers(binding:DeliveryBinding,generation:Long,setupId:String,current:()->Boolean={true}):JsonObject=policy.locked {
    var result:JsonObject?=null
    edit {p->
        check(current()) {"Connection changed"}
        val (state,_)=checkSetup(p,binding,generation,setupId)
        val old=state.sends[binding.pcId]?.takeIf {it.bindingGeneration==binding.generation && it.setupGeneration==generation}
        val current=MembersSnapshot(randomId(),setupId,state.roster)
        val reuse=old?.pending?.takeIf {it.isNotEmpty()} ?: old?.acknowledged?.takeIf {it.isNotEmpty()}
            ?.takeIf {SetupWire.decode(com.google.gson.JsonParser.parseString(it).asJsonObject).canonical()==current.canonical()}
        result=reuse?.let {com.google.gson.JsonParser.parseString(it).asJsonObject} ?: current.json()
        SetupStorage.write(p,state.copy(sends=state.sends+(binding.pcId to SetupSend(binding.generation,generation,result.toString(),old?.acknowledged.orEmpty()))))
    }
    requireNotNull(result)
}
internal suspend fun Preferences.acknowledgeMembers(binding:DeliveryBinding,generation:Long,setupId:String,body:JsonObject,current:()->Boolean={true})=policy.locked {
    edit {p->
        check(current()) {"Connection changed"}
        val (state,_)=checkSetup(p,binding,generation,setupId)
        val old=state.sends[binding.pcId] ?: throw SetupUnavailable()
        check(old.bindingGeneration==binding.generation && old.setupGeneration==generation && old.pending==body.toString())
        SetupStorage.write(p,state.copy(sends=state.sends+(binding.pcId to old.copy(pending="",acknowledged=body.toString()))))
    }
}
internal suspend fun Preferences.receiveMembers(binding:DeliveryBinding,generation:Long,setupId:String,body:JsonObject,
    self:SetupIdentity,attempt:SetupAttemptTarget?,queue:QueueDao,now:Long=System.currentTimeMillis(),current:()->Boolean={true}):Pair<Boolean,SetupPlan?> =policy.locked {
    var result:Pair<Boolean,SetupPlan?>?=null
    edit {p->
        check(current()) {"Connection changed"}
        val (state,sender)=checkSetup(p,binding,generation,setupId)
        if(body["setup_id"].asString!=setupId)throw SetupUnavailable()
        val snapshot=SetupWire.decode(body)
        SetupReconcile.validate(snapshot,self,sender,now)
        val canonical=snapshot.canonical();val hash=sha256(canonical.toByteArray())
        val existingReceipt=queue.receipt(sender.deviceId,snapshot.transferId)
        if(existingReceipt!=null && (existingReceipt.origin!="members" || existingReceipt.hash!=hash))throw SetupReceiptConflict()
        val old=state.receipts.find {it.sender==sender.deviceId && it.transferId==snapshot.transferId && it.generation==generation}
        if(old!=null && old.hash!=hash)throw SetupReceiptConflict()
        if(old!=null) {result=true to null;return@edit}
        // Complete the single cross-store journal before admitting another
        // membership operation. An outage cannot grow unbounded intents.
        check(state.receiptIntents.isEmpty()) {"Membership recovery pending"}
        val plan=SetupReconcile.plan(state,PeerRegistry.read(p),self,sender,snapshot,attempt,now)
        if(existingReceipt==null)check(queue.insertReceipt(Receipt().apply {
            peerId=sender.deviceId;transferId=snapshot.transferId;this.hash=hash;origin="members"
        })!=-1L)
        beforeMutation("membersReserved")
        check(current()) {"Connection changed"}
        var next=SetupTransactions.commit(p,plan,sender,beforeMutation)
        val receipt=SetupReceipt(sender.deviceId,snapshot.transferId,generation,hash)
        // Withdrawal/dissolution intentionally retires the receipt with its setup.
        // The intent still completes this transaction's shared Room namespace.
        next=next.copy(receiptIntents=next.receiptIntents+receipt,
            receipts=if(next.generation==generation)SetupStorage.receiptWindow(state.receipts+receipt) else next.receipts)
        SetupStorage.write(p,next)
        beforeMutation("membersReceiptStaged")
        beforeMutation("membersCommittedInEdit")
        result=false to plan
    }
    beforeMutation("membersCommitted")
    requireNotNull(result)
}
internal suspend fun Preferences.finishMembersIntents(queue:QueueDao)=policy.locked {
    edit {p->
        val state=SetupStorage.read(p)
        for(intent in state.receiptIntents) {
            val hash=intent.hash;val old=queue.receipt(intent.sender,intent.transferId)
            check(old==null || old.origin=="members" && old.hash==hash) {"Membership receipt conflict"}
            if(old==null)check(queue.insertReceipt(Receipt().apply {peerId=intent.sender;transferId=intent.transferId;this.hash=hash;origin="members";this.state="applied"})!=-1L)
            else if(old.state!="applied")queue.updateReceipt(old.apply {this.state="applied"})
        }
        state.receiptIntents.map {it.sender}.distinct().forEach {queue.pruneMembersReceipts(it)}
        SetupStorage.write(p,state.copy(receiptIntents=emptyList()))
    }
}
