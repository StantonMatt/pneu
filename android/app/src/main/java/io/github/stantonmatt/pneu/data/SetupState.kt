package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.Gson
import io.github.stantonmatt.pneu.protocol.*

/** All terminal history is independent of peer cascades and of the current wire
 * roster. A cleared barrier retains the exact removal value for conflict checks. */
data class RemovalBarrier(val removed:SetupMember,val cleared:Boolean=false)
data class ConfirmedSetupMember(val setupGeneration:Long,val member:SetupMember,val binding:ConfirmedBinding?) {
    fun matches(p:Prefs)=p.membership.generation==setupGeneration && p.membership.roster.any {it==member && it.active} &&
        (binding==null && p.devices.none {it.deviceId==member.deviceId && it.pin==member.fingerprint} || binding?.matches(p)==true)
    companion object {
        fun from(p:Prefs,key:String):ConfirmedSetupMember {
            check(p.loaded)
            val member=p.membership.roster.single {it.key==key && it.active && it.deviceId!=p.deviceId}
            return ConfirmedSetupMember(p.membership.generation,member,p.devices.find {it.deviceId==member.deviceId && it.pin==member.fingerprint}?.let {ConfirmedBinding.from(p,it.deviceId)})
        }
    }
}
data class SetupPrompt(val peerId:String,val pin:String,val bindingGeneration:Long,val setupGeneration:Long,
                       val name:String,val allowRequested:Boolean=false)
data class SetupLinkBinding(val peerId:String,val pin:String,val generation:Long)
data class SetupLinkAdvertisement(val peerId:String,val name:String,val setupId:String,val pin:String,val bindingGeneration:Long)
data class PendingSetupLink(val id:String,val setupGeneration:Long,val bindings:List<SetupLinkBinding>,
                            val advertised:List<SetupLinkAdvertisement> = emptyList()) {
    val waitingFor get()=advertised.firstOrNull()?.name
}
data class SetupNotice(val id:String,val peerId:String,val name:String,val introducerId:String,val introducerName:String,val alsoLinked:Boolean=false)
data class SetupProvenance(val id:String,val name:String)
data class SetupConflict(val deviceId:String,val fingerprint:String,val reason:String)
data class SetupSend(val bindingGeneration:Long,val setupGeneration:Long,val pending:String="",val acknowledged:String="")
data class SetupReceipt(val sender:String,val transferId:String,val generation:Long,val hash:String)
data class SetupState(val setupId:String?=null,val generation:Long=0,val roster:List<SetupMember> = emptyList(),
    val barriers:Map<String,RemovalBarrier> = emptyMap(),val prompts:List<SetupPrompt> = emptyList(),
    val sends:Map<String,SetupSend> = emptyMap(),val receipts:List<SetupReceipt> = emptyList(),
    val receiptIntents:List<SetupReceipt> = emptyList(),val notices:List<SetupNotice> = emptyList(),
    val conflicts:List<SetupConflict> = emptyList(),val provenance:Map<String,SetupProvenance> = emptyMap(),val separateLinks:Set<String> = emptySet(),
    val pendingLink:PendingSetupLink?=null,val initialSnapshotPending:Boolean=false,val withdrawn:Boolean=false,val withdrawalRevision:Long=0,val retiredIds:Set<String> = emptySet()) {
    fun denies(id:String,pin:String)=barriers["$id/$pin"]?.cleared==false
    val linkStatus get()=pendingLink?.waitingFor?.let {"Waiting for $it"}
    fun linkOutcome(peerId:String)=if(peerId in separateLinks)"Direct link kept; separate setups not linked" else null
    fun member(id:String)=roster.find {it.deviceId==id && it.active}
    fun hello(scope:SetupScope)=SetupHello(scope,setupId.takeUnless {scope==SetupScope.DISABLED})
}
internal object SetupStorage {
    private val key=stringPreferencesKey("setupMembership")
    private val gson=Gson()
    fun read(p:androidx.datastore.preferences.core.Preferences):SetupState = p[key]?.let {
        requireNotNull(gson.fromJson(it,SetupState::class.java))
    } ?: SetupState()
    const val RECEIPT_BYTE_LIMIT=1024*1024
    fun receiptWindow(receipts:List<SetupReceipt>):List<SetupReceipt> {
        var window=receipts.takeLast(4096)
        while(gson.toJson(window).toByteArray(Charsets.UTF_8).size>RECEIPT_BYTE_LIMIT)window=window.drop(1)
        return window
    }
    // Transient UI/discovery state belongs to the exact surviving binding.
    fun write(p:MutablePreferences,state:SetupState) {
        val bindings=PeerRegistry.read(p).map {SetupLinkBinding(it.deviceId,it.pin,it.generation)}.toSet()
        val pending=state.pendingLink?.let {link->
            val covered=link.bindings.filter {it in bindings}
            link.copy(bindings=covered,advertised=link.advertised.filter {
                SetupLinkBinding(it.peerId,it.pin,it.bindingGeneration) in covered})
        }
        val live=state.copy(pendingLink=pending,prompts=state.prompts.filter {
            it.setupGeneration==state.generation && SetupLinkBinding(it.peerId,it.pin,it.bindingGeneration) in bindings},
            separateLinks=state.separateLinks.intersect(bindings.map {it.peerId}.toSet()),
            sends=state.sends.filter {(id,send)->bindings.any {it.peerId==id && it.generation==send.bindingGeneration}})
        p[key]=gson.toJson(live)
    }
}
internal class SetupCapacity:Exception("Setup capacity reached")
internal class SetupUnavailable:Exception("Setup is not enabled for this connection")
internal class RemovedSetupSender:Exception("Removed setup sender")
internal class SetupReceiptConflict:Exception("Membership operation ID conflict")
internal data class SetupIdentity(val id:String,val pin:String,val name:String,val role:String="phone")
internal data class SetupAttemptTarget(val deviceId:String?,val pin:String) {
    fun matchesRemoval(key:String)=key.substringAfter('/')==pin && (deviceId==null || deviceId==key.substringBefore('/'))
}
internal data class SetupPlan(val state:SetupState,val peers:List<Peer>,val removed:List<Peer>,val installed:List<Peer>,
                              val observed:List<java.math.BigInteger>,val removedKeys:Set<String>,val validatedAt:Long,val alsoLinked:Boolean=false)

/** Pure staged reconciliation. The caller commits this whole result, the clock,
 * reset intents and receipt in one DataStore edit under the binding lock. */
internal object SetupReconcile {
    fun stamp(member:SetupMember)=ClipboardStamp(member.hlc.toBigInteger(),member.writer)
    private fun stamp(reach:MemberReach)=ClipboardStamp(reach.hlc.toBigInteger(),reach.writer)
    private fun malformed():Nothing=throw MeshViolation()
    fun capacity(state:SetupState,peers:List<Peer>) {
        val retained=(state.barriers.values.map {it.removed}+state.roster).distinctBy {it.key}
        if(retained.size>64 || retained.groupBy {it.deviceId}.values.any {it.size>4} ||
            state.roster.count {it.active}>17 || peers.size>PeerRegistry.MAX_PEERS)throw SetupCapacity()
        if(state.setupId!=null) {
            val body=MembersSnapshot("0".repeat(32),state.setupId,state.roster).json()
            if(body.toString().toByteArray().size>65536)throw SetupCapacity()
        }
    }
    private fun merged(old:SetupMember?,incoming:SetupMember):SetupMember {
        if(old==null)return incoming
        if(stamp(old)==stamp(incoming) && old.value()!=incoming.value())malformed()
        val membership=if(stamp(incoming)>stamp(old))incoming else old
        if(!membership.active)return membership.copy(reach=null)
        val a=old.reach;val b=incoming.reach
        if(a!=null && b!=null && stamp(a)==stamp(b) && a!=b)malformed()
        return membership.copy(reach=when {a==null->b;b==null->a;stamp(b)>stamp(a)->b;else->a})
    }
    fun validate(snapshot:MembersSnapshot,self:SetupIdentity,sender:Peer,now:Long):List<java.math.BigInteger> {
        // Syntax and all clocks precede every staged mutation, including replay.
        SetupWire.decode(snapshot.json())
        val observed=snapshot.members.flatMap {listOfNotNull(it.hlc.toBigInteger(),it.reach?.hlc?.toBigInteger())}
        observed.forEach {MeshClock.validate(it,now)}
        val senderRow=snapshot.members.singleOrNull {it.deviceId==sender.deviceId && it.active} ?: malformed()
        if(senderRow.fingerprint!=sender.pin || senderRow.role!=sender.role)malformed()
        for(m in snapshot.members) {
            if(m.active && (m.deviceId==self.id && (m.fingerprint!=self.pin || m.role!=self.role) ||
                m.fingerprint==self.pin && m.deviceId!=self.id || m.fingerprint==sender.pin && m.deviceId!=sender.deviceId))malformed()
        }
        return observed
    }
    fun plan(state:SetupState,peers:List<Peer>,self:SetupIdentity,sender:Peer,snapshot:MembersSnapshot,
             attempt:SetupAttemptTarget?=null,now:Long=System.currentTimeMillis()):SetupPlan {
        if(state.setupId==null || state.setupId!=snapshot.setupId || state.withdrawn || sender.setupScope!=SetupScope.ENABLED)throw SetupUnavailable()
        if(state.denies(sender.deviceId,sender.pin))throw RemovedSetupSender()
        val observed=validate(snapshot,self,sender,now)
        val roster=state.roster.associateBy {it.key}.toMutableMap()
        val barriers=state.barriers.toMutableMap()
        val conflicts=state.conflicts.toMutableList()
        val provenance=state.provenance.toMutableMap()
        val removedKeys=mutableSetOf<String>()
        for(m in snapshot.members) {
            val old=roster[m.key]
            // Equal stamps conflict even when eligibility would suppress the row.
            merged(old,m)
            val barrier=barriers[m.key]
            if(barrier!=null)merged(barrier.removed,m)
            if(!m.active) {
                if(barrier?.cleared==true && stamp(m)<=stamp(barrier.removed))continue
                val removal=if(barrier!=null && stamp(barrier.removed)>stamp(m))barrier.removed else m
                barriers[m.key]=RemovalBarrier(removal)
                // Retain the local high-water value in history, but only
                // this setup's received/current removals belong on the wire.
                roster[m.key]=old?.takeIf {!it.active && stamp(it)>stamp(m)} ?: m
                removedKeys.add(m.key)
                continue
            }
            // A retired deny is local admission history, never a tombstone in
            // this setup. Only the current roster can supply a wire tombstone.
            if(barrier?.cleared==false)continue
            val sameId=roster.values.find {it.active && it.deviceId==m.deviceId && it.fingerprint!=m.fingerprint}
            val bound=peers.find {it.deviceId==m.deviceId}
            val samePin=roster.values.find {it.active && it.fingerprint==m.fingerprint && it.deviceId!=m.deviceId}
            val boundPin=peers.find {it.pin==m.fingerprint && it.deviceId!=m.deviceId}
            if(sameId!=null || bound!=null && bound.pin!=m.fingerprint || samePin!=null || boundPin!=null) {
                conflicts.add(SetupConflict(m.deviceId,m.fingerprint,"Identity already bound"));continue
            }
            roster[m.key]=merged(old,m)
            if(m.deviceId!=self.id && m.deviceId!=sender.deviceId && peers.none {it.deviceId==m.deviceId})
                provenance.putIfAbsent(m.key,SetupProvenance(sender.deviceId,sender.name))
        }
        var result=state.copy(initialSnapshotPending=false,roster=roster.values.sortedBy {it.key},barriers=barriers,conflicts=conflicts.distinct().takeLast(64),provenance=provenance)
        var remaining=peers.toList();val removed=mutableListOf<Peer>();val installed=mutableListOf<Peer>()
        if(result.denies(self.id,self.pin)) {
            val scoped=remaining.filter {it.setupScoped}
            removed.addAll(scoped);remaining=remaining-scoped.toSet()
            result=retire(result,true)
        } else {
            for(peer in peers)if(result.denies(peer.deviceId,peer.pin)) {remaining=remaining-peer;removed.add(peer)}
            for(m in result.roster.filter {it.active && it.deviceId!=self.id}) {
                val prior=remaining.find {it.deviceId==m.deviceId}
                if(prior!=null && prior.pin==m.fingerprint) {
                    // Self observations never rename the receiver. Existing scopes,
                    // receipts, settings, generation and provenance remain intact.
                    val previous=state.roster.find {it.key==m.key}
                    val changed=previous==null || previous.value()!=m.value()
                    val reachChanged=previous==null || previous.reach!=m.reach
                    remaining=remaining.map {if(it==prior)it.copy(name=if(changed)m.name else it.name,
                        addresses=if(reachChanged)(m.reach?.addresses.orEmpty().map(Endpoint::parse)+it.addresses).distinct().take(16) else it.addresses) else it}
                } else if(prior==null && m.role=="computer" && remaining.none {it.pin==m.fingerprint} &&
                    !result.denies(m.deviceId,m.fingerprint) && (attempt==null || attempt.deviceId!=m.deviceId && attempt.pin!=m.fingerprint)) {
                    val peer=Peer(m.name,m.fingerprint,m.deviceId,m.reach?.addresses.orEmpty().map(Endpoint::parse),
                        setupDecision=SetupScope.ENABLED,addedViaId=result.provenance[m.key]?.id ?: sender.deviceId,addedViaName=result.provenance[m.key]?.name ?: sender.name)
                    installed.add(peer);remaining=remaining+peer
                }
            }
            if(result.setupId!=null && result.roster.none {it.active && it.deviceId!=self.id})result=retire(result,false)
        }
        capacity(result,remaining)
        return SetupPlan(result,remaining,removed,installed,observed,removedKeys,now,
            state.initialSnapshotPending || state.member(self.id)==null)
    }
    fun retained(state:SetupState,peers:List<Peer>,selfId:String,attempt:SetupAttemptTarget?):SetupPlan {
        if(state.setupId==null || state.withdrawn)throw SetupUnavailable()
        val installed=mutableListOf<Peer>()
        for(m in state.roster.filter {it.active && it.role=="computer" && it.deviceId!=selfId}) {
            val via=state.provenance[m.key] ?: continue
            if(peers.any {it.deviceId==m.deviceId || it.pin==m.fingerprint} || state.denies(m.deviceId,m.fingerprint) ||
                attempt!=null && (attempt.deviceId==m.deviceId || attempt.pin==m.fingerprint))continue
            installed.add(Peer(m.name,m.fingerprint,m.deviceId,m.reach?.addresses.orEmpty().map(Endpoint::parse),
                setupDecision=SetupScope.ENABLED,addedViaId=via.id,addedViaName=via.name))
        }
        capacity(state,peers+installed)
        return SetupPlan(state,peers+installed,emptyList(),installed,emptyList(),emptySet(),System.currentTimeMillis())
    }
    fun retire(state:SetupState,withdrawal:Boolean)=state.copy(setupId=null,generation=Math.addExact(state.generation,1),
        roster=emptyList(),prompts=emptyList(),pendingLink=null,initialSnapshotPending=false,sends=emptyMap(),receipts=emptyList(),
        receiptIntents=state.receiptIntents,provenance=emptyMap(),notices=emptyList(),withdrawn=state.withdrawn || withdrawal,withdrawalRevision=if(withdrawal)Math.addExact(state.withdrawalRevision,1) else state.withdrawalRevision,retiredIds=state.retiredIds+listOfNotNull(state.setupId))
}
