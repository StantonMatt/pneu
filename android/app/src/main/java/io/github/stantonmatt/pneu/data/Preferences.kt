package io.github.stantonmatt.pneu.data

import android.content.Context
import androidx.datastore.preferences.core.*
import androidx.datastore.preferences.preferencesDataStore
import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.connection.TlsIdentity
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

private val Context.pneuDataStore by preferencesDataStore("pneu")
data class Peer(val name: String, val pin: String, val deviceId: String, val addresses: List<Endpoint>,
    val role:String="computer",val lastLan:String="",val lastConnected:Long=0,val generation:Long=0,
    val pairingLost:Boolean=false,val clipboardSend:Boolean=true,val clipboardReceive:Boolean=true,val automaticMedia:Boolean=true,
    val settings:Map<String,PairField> = emptyMap(),val pendingSettings:String="",val confirmedSettings:String="",
    private val settingsReceipts:Map<String,String>? = null,
    @com.google.gson.annotations.SerializedName("localSettingsEdits") private val localEdits:Set<String>? = null,val settingsSupported:Boolean?=null,
    private val acknowledgedSettings:String?=null,
    @com.google.gson.annotations.SerializedName("setupScope") private val setupDecision:SetupScope?=null,
    val addedViaId:String?=null,val addedViaName:String?=null,val established:Boolean=false,val setupSupported:Boolean=false,
    /** Last authenticated HELLO observation for this binding; never local consent. */
    val remoteSetupScope:SetupScope?=null,val helloObserved:Boolean=false,val setupRefused:Boolean=false,val readyObserved:Boolean=false) {
    val helloSeen get()=helloObserved || remoteSetupScope!=null || lastConnected>0
    val setupScope get()=setupDecision ?: SetupScope.ASK
    val setupScoped get()=addedViaId!=null || setupScope==SetupScope.ENABLED && setupSupported
    internal val acknowledgedSnapshot get()=acknowledgedSettings.orEmpty()
    val localSettingsEdits get()=localEdits.orEmpty()
    internal val legacySettingsReceipts get()=settingsReceipts.orEmpty()
}
data class Prefs(
    val setup: Boolean = false, val paused: Boolean = false,
    val screenshots: Boolean = true, val photos: Boolean = false, val recordings: Boolean = false,
    val wifi: Boolean = true, val tailscale: Boolean = true, val mobile: Boolean = true,
    val originals: Boolean = false, val queue: Boolean = false, val clipboardButton: Boolean = true,
    val clipboardFromPc: Boolean = true, val shareSheet: Boolean = true, val quickSettings: Boolean = true,
    val deviceId: String = "", val peer: Peer? = null,
    val lastLan: String = "", val lastConnected: Long = 0, val mediaSince: Long = 0,
    val pairingLost: Boolean = false, val neverSleeping: Boolean = false,
    val photosSince: Long = 0, val recordingsSince: Long = 0, val photosAfterId: Long = 0, val recordingsAfterId: Long = 0,
    val lastReceivedHash: String = "", val lastSentHash: String = "", val localCapacity: Boolean = false,
    val deliveryReset: Boolean=false,
    val imageVersion: String="",val videoVersion: String="",val imageGeneration: Long=0,val videoGeneration: Long=0,
    val imagePairId: Long=0,val videoPairId: Long=0, val lostPcName: String="", val tlsAlias:String="",
    val imageFloor:Long=0,val videoFloor:Long=0,val policyEpoch:Long=0,val screenshotsAfterId:Long=0, val pairingRevision:Long=0, val loaded:Boolean=true,
    val peers:List<Peer> = emptyList(), val resetPeers:Set<String> = emptySet(),
    val recentClipboardHashes:List<String> = emptyList(),val legacyQueuePeer:String="",val legacyQueueGeneration:Long=0,val selfName:String="",val chooseDevicesEachTime:Boolean=false,val lastReceivedSource:String="",
    val recentClipboardSources:Map<String,String> = emptyMap(),
    val automaticRevocations:Map<String,Long> = emptyMap(),
    val membership:SetupState=SetupState(),
    val showBatteryLevel:Boolean=true,val allowRinging:Boolean=true
) {
    fun automaticRevocation(kind:String)=automaticRevocations[when(kind) {"photo"->"photos";"recording"->"recordings";else->"screenshots"}] ?: 0
    // Compatibility projection for the existing single-computer screens/fixtures.
    val devices get()=peers.ifEmpty {listOfNotNull(peer)}
    fun installedAfter(revision:Long,pin:String?=null):DeliveryBinding? = devices.filter {it.generation>revision && (pin==null || it.pin==pin)}.maxByOrNull {it.generation}?.let {DeliveryBinding.from(this,it.deviceId)}
    fun forPeer(id:String):Prefs {
        val selected=devices.find {it.deviceId==id}
        return copy(peer=selected,lastLan=selected?.lastLan.orEmpty(),lastConnected=selected?.lastConnected ?: 0,
            pairingLost=selected?.pairingLost ?: false,pairingRevision=selected?.generation ?: pairingRevision)
    }
}
class Preferences(private val context:Context,internal val store:androidx.datastore.core.DataStore<androidx.datastore.preferences.core.Preferences> = context.pneuDataStore) {
    internal var beforeMutation:(String)->Unit={}
    @Volatile internal var beforeRead:suspend ()->Unit={}
    @Volatile internal var beforeReadStart:suspend ()->Unit={}
    @Volatile internal var readBoundary:suspend (Boolean)->Long={MediaCatalog.latest(context,it)}
    internal val policy=AdmissionPolicy()
    /** Serializes computer-feature effects with local permission/pause commits. */
    internal val computerPolicy=Mutex()
    @Volatile internal var readCatalogVersion:suspend ()->String={MediaCatalog.version(context)}
    @Volatile internal var pairingBoundary:suspend ()->CatalogBoundary={MediaCatalog.boundary(context)}
    @Volatile internal var readSnapshot:suspend ()->CatalogSnapshot={MediaCatalog.snapshot(context)}
    private data class ClipboardEffect(val stamp:ClipboardStamp,val hash:String,val peerId:String,val generation:Long,val id:String,val legacyTime:Long?=null)
    private val unfinishedEffects=java.util.concurrent.ConcurrentHashMap<String,ClipboardEffect>()
    internal fun clipboardEffect(peerId:String,generation:Long,id:String)=unfinishedEffects[completionKey(peerId,generation,id)]?.stamp
    /** Register the OS effect before any fallible persistence. Shared by every
     * receiver and every device-wide ordering/ownership decision. */
    internal fun recordClipboardEffect(stamp:ClipboardStamp,hash:String,peerId:String,generation:Long,id:String,legacyTime:Long?=null) {
        unfinishedEffects[completionKey(peerId,generation,id)]=ClipboardEffect(stamp,hash,peerId,generation,id,legacyTime)
    }
    private fun effectiveClipboardRegister(p:androidx.datastore.preferences.core.Preferences):ClipboardStamp? =
        (unfinishedEffects.values.map {it.stamp}+listOfNotNull(MeshClock.register(p))).maxOrNull()
    private fun reconcileClipboardEffects(p:MutablePreferences,effects:Collection<ClipboardEffect>) {
        for(effect in effects.sortedBy {it.stamp}) {
            if(MeshClock.wins(p,effect.stamp)) {
                MeshClock.applied(p,effect.stamp)
                ClipboardHistory.remember(p,effect.hash,effect.peerId)
            }
            val current=PeerRegistry.read(p).any {it.deviceId==effect.peerId && it.generation==effect.generation}
            if(current && effect.legacyTime!=null) {
                val time=longPreferencesKey("legacyImageTime:${effect.peerId}")
                val generation=longPreferencesKey("legacyImageGeneration:${effect.peerId}")
                p[time]=if(p[generation]==effect.generation)maxOf(p[time] ?: -1,effect.legacyTime) else effect.legacyTime
                p[generation]=effect.generation
            }
            if(current)p[clipboardCompletions]=p[clipboardCompletions].orEmpty()+completionKey(effect.peerId,effect.generation,effect.id)
        }
    }
    /** Any successful settings write also persists abandoned clipboard effects.
     * Remove the in-memory fence only after the DataStore commit has succeeded. */
    internal suspend fun edit(body:suspend (MutablePreferences)->Unit) {
        var effects=emptyList<ClipboardEffect>()
        store.edit {p->
            effects=unfinishedEffects.values.toList()
            reconcileClipboardEffects(p,effects)
            body(p)
        }
        effects.forEach {unfinishedEffects.remove(completionKey(it.peerId,it.generation,it.id),it)}
    }
    internal suspend fun reconcileClipboardEffects() {edit {}}
    private fun b(p: androidx.datastore.preferences.core.Preferences, key: String, default: Boolean = false) = p[booleanPreferencesKey(key)] ?: default
    private fun s(p: androidx.datastore.preferences.core.Preferences, key: String) = p[stringPreferencesKey(key)] ?: ""
    private fun l(p: androidx.datastore.preferences.core.Preferences, key: String) = p[longPreferencesKey(key)] ?: 0
    private val raw: Flow<Prefs> = flow {
        // A DataStore edit is atomic: no migrated snapshot or delivery authority is
        // exposed until the peer list and original queue owner are durable.
        edit {PeerRegistry.migrate(it);it.remove(stringPreferencesKey("pending"))}
        emitAll(store.data)
    }.onEach {beforeRead()}.map { durable ->
        val p=durable.toMutablePreferences().also {reconcileClipboardEffects(it,unfinishedEffects.values.toList())}
        val peers=PeerRegistry.read(p)
        val peer=peers.firstOrNull()
        Prefs(b(p,"setup"),b(p,"paused"),b(p,"screenshots",true),b(p,"photos"),b(p,"recordings"),b(p,"wifi",true),b(p,"tailscale",true),b(p,"mobile",true),b(p,"originals"),b(p,"queue"),b(p,"clipboardButton",true),b(p,"clipboardFromPc",true),b(p,"shareSheet",true),b(p,"quickSettings",true),s(p,"deviceId"),peer,s(p,"lastLan"),l(p,"lastConnected"),l(p,"mediaSince"),b(p,"pairingLost"),b(p,"neverSleeping"),l(p,"photosSince"),l(p,"recordingsSince"),l(p,"photosAfterId"),l(p,"recordingsAfterId"),s(p,"lastReceivedHash"),s(p,"lastSentHash"),b(p,"localCapacity"),b(p,"deliveryReset"),s(p,"imageVersion"),s(p,"videoVersion"),l(p,"imageGeneration"),l(p,"videoGeneration"),l(p,"imagePairId"),l(p,"videoPairId"),s(p,"lostPcName"),s(p,"tlsAlias"),l(p,"imageFloor"),l(p,"videoFloor"),l(p,"mediaEpoch"),l(p,"screenshotsAfterId"),l(p,"pairingRevision")).copy(peers=peers,clipboardFromPc=peer?.clipboardReceive ?: b(p,"clipboardFromPc",true),lastLan=peer?.lastLan.orEmpty(),lastConnected=peers.maxOfOrNull {it.lastConnected} ?: 0,
            pairingLost=peers.isNotEmpty() && peers.all {it.pairingLost} || peers.isEmpty() && b(p,"pairingLost"),
            pairingRevision=l(p,"pairingRevision"),lastReceivedSource=s(p,"lastReceivedSource"),resetPeers=PeerRegistry.resets(p),
            legacyQueuePeer=s(p,"legacyQueuePeer"),legacyQueueGeneration=l(p,"legacyQueueGeneration"),
            recentClipboardHashes=s(p,"recentClipboardHashes").split('|').filter {it.isNotEmpty()},recentClipboardSources=ClipboardHistory.read(p),selfName=s(p,"selfName"),chooseDevicesEachTime=b(p,"chooseDevicesEachTime"),
            showBatteryLevel=b(p,"showBatteryLevel",true),allowRinging=b(p,"allowRinging",true),
            membership=SetupStorage.read(p),automaticRevocations=listOf("screenshots","photos","recordings").associateWith {l(p,it+"AdmissionEpoch")}.filterValues {it!=0L})
    }
    val flow=raw.recoverReads()
    suspend fun read():Prefs=kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {storageDeadline {beforeReadStart();raw.first()}}
    /** Generate the key before publishing its ID/alias in the same durable edit.
     * Upgrades preserve the old fixed certificate for the existing identity. */
    suspend fun ensureIdentity():Unit=policy.locked {
        beforeMutation("ensureIdentity")
        edit {
            PeerRegistry.migrate(it)
            it.remove(stringPreferencesKey("pauseHistory"))
            val existing=s(it,"deviceId");val savedAlias=s(it,"tlsAlias")
            val id=existing.ifEmpty {randomId()}
            val alias=savedAlias.ifEmpty {if(existing.isNotEmpty())TlsIdentity.LEGACY_ALIAS else TlsIdentity.aliasFor(id)}
            TlsIdentity(alias,create=true)
            if(existing!=id || savedAlias!=alias)advanceEpoch(it)
            it[stringPreferencesKey("deviceId")]=id;it[stringPreferencesKey("tlsAlias")]=alias
        }
        Unit
    }
    private fun advanceEpoch(p:MutablePreferences) {p[longPreferencesKey("mediaEpoch")]=AdmissionPolicy.next(l(p,"mediaEpoch"))}
    private val policyKeys=setOf("paused","screenshots","photos","recordings","wifi","tailscale","mobile","originals","queue","deliveryReset")
    suspend fun toggle(key:String,value:Boolean)=kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
        if(key=="clipboardFromPc") {read().peer?.let {setPairSwitch(it.deviceId,"receiveClipboard",value)};return@withContext}
        suspend fun apply() {
            beforeMutation(key)
            // All decisions and provider reads belong to the policy lock, including
            // a delayed Resume's current pause state and its catalog floor.
            val current=read()
            val resume=key=="paused" && !value && current.paused
            val enable=key in listOf("screenshots","photos","recordings") && value
            if(resume && ((current.screenshots || current.photos) && !MediaCatalog.fullAccess(context,false) || current.recordings && !MediaCatalog.fullAccess(context,true)))throw java.io.IOException("Full media access required to resume")
            if(enable && !MediaCatalog.fullAccess(context,key=="recordings"))throw java.io.IOException("Full media access required")
            val snapshot=if(resume || enable)readSnapshot() else null
            val afterId=if(enable)readBoundary(key=="recordings") else 0
            if(snapshot!=null && readCatalogVersion()!=snapshot.version)throw java.io.IOException("Catalog changed")
            edit {
                if(!value && ((key=="wifi" && !b(it,"tailscale",true)) || (key=="tailscale" && !b(it,"wifi",true))))throw IllegalArgumentException("Pneu needs Wi-Fi or Tailscale")
                if(key in policyKeys && b(it,key,key in listOf("screenshots","wifi","tailscale","mobile"))!=value) {
                    advanceEpoch(it)
                    // Only user observation boundaries revoke discoveries.
                    // Startup/permission baselines must preserve pending work,
                    // and changing encoding/network policy only retries its copy.
                    val revoked=when(key) {"paused"->listOf("screenshots","photos","recordings");"screenshots","photos","recordings"->listOf(key);else->emptyList()}
                    for(feature in revoked)it[longPreferencesKey(feature+"AdmissionEpoch")]=AdmissionPolicy.next(l(it,feature+"AdmissionEpoch"))
                }
                if(resume) {
                    requireNotNull(snapshot)
                    for(prefix in listOf("image","video")) {
                        val oldVersion=s(it,prefix+"Version")
                        it[longPreferencesKey(prefix+"Generation")]=AdmissionPolicy.watermark(oldVersion,l(it,prefix+"Generation"),snapshot.version,snapshot.generation+1)
                        it[longPreferencesKey(prefix+"Floor")]=AdmissionPolicy.watermark(oldVersion,l(it,prefix+"Floor"),snapshot.version,snapshot.generation+1)
                        if(oldVersion!=snapshot.version) {
                            // IDs are namespaced too. The new generation floor
                            // excludes existing rows; old ID guards must not hide new ones.
                            it[longPreferencesKey(prefix+"PairId")]=0
                            for(key in if(prefix=="image")listOf("photosAfterId","screenshotsAfterId") else listOf("recordingsAfterId"))it[longPreferencesKey(key)]=0
                        }
                        it[stringPreferencesKey(prefix+"Version")]=snapshot.version
                    }
                }
                if(enable && !b(it,key,key=="screenshots")) {
                    it[longPreferencesKey(key+"Since")]=System.currentTimeMillis()
                    val prefix=if(key=="recordings")"video" else "image"
                    it[longPreferencesKey(key+"AfterId")]=AdmissionPolicy.watermark(s(it,prefix+"Version"),l(it,key+"AfterId"),requireNotNull(snapshot).version,afterId)
                }
                it[booleanPreferencesKey(key)]=value
            }
        }
        suspend fun mutate() {if(key in policyKeys)policy.locked {apply()} else apply()}
        if(key in setOf("showBatteryLevel","allowRinging","paused"))computerPolicy.withLock {mutate()} else mutate()
    }
    /** Automatic bytes/skips are published only under the same policy boundary.
     * Reading the live namespace here rejects URI reuse after a catalog rebuild. */
    internal suspend fun publishAutomatic(token:AdmissionToken,publicationMutex:Mutex,body:suspend (Prefs)->Boolean):Boolean=policy.publishing(publicationMutex) {
        // Validate AFTER waiting for the queue, not before. The checked value
        // goes straight to its commit; no second settings read can suspend it.
        val current=read()
        if(!AdmissionPolicy.accepts(token,current,readCatalogVersion()) || !MediaCatalog.fullAccess(context,token.video))false else body(current)
    }
    suspend fun setLong(key: String, value: Long) { edit { it[longPreferencesKey(key)] = value } }
    suspend fun setString(key: String, value: String) {
        if(key in setOf("deviceId","tlsAlias"))policy.locked {edit {if(s(it,key)!=value)advanceEpoch(it);it[stringPreferencesKey(key)]=value}}
        else edit {p->
            if(key=="selfName") {
                val state=SetupStorage.read(p);val own=state.member(s(p,"deviceId"))
                if(own!=null && own.name!=value) {
                    val stamp=MeshClock.next(p,s(p,"deviceId"),false)
                    val next=state.copy(roster=state.roster.map {if(it.key==own.key)it.copy(name=value,hlc=stamp.hlc.toString(),writer=stamp.source) else it})
                    SetupReconcile.capacity(next,PeerRegistry.read(p));SetupStorage.write(p,next)
                }
            }
            p[stringPreferencesKey(key)]=value
        }
    }
    /** Old sessions cannot repopulate a replacement PC's connection cache. */
    internal suspend fun connected(binding:DeliveryBinding,endpoint:Endpoint,at:Long=System.currentTimeMillis()):Boolean {
        var recorded=false
        edit {
            val peers=PeerRegistry.read(it);val peer=peers.find {peer->peer.deviceId==binding.pcId} ?: return@edit
            if(s(it,"deviceId")!=binding.phoneId || peer.pin!=binding.pin || peer.generation!=binding.generation || peer.deviceId in PeerRegistry.resets(it) || peer.pairingLost)return@edit
            PeerRegistry.write(it,peers.map {if(it==peer)it.copy(established=true,readyObserved=true,setupRefused=false,lastLan=if(endpoint.tailscale)it.lastLan else endpoint.toString(),lastConnected=at) else it})
            recorded=true
        }
        return recorded
    }
    suspend fun paired(invitation: Invitation, pcId: String,
                       replacement:ConfirmedBinding?=null,setupChoice:Boolean=true,setupFence:SetupState?=null,beforeInstall:(MutablePreferences,DeliveryBinding)->Unit={_,_->},onInstalled:(DeliveryBinding)->Unit={}): Boolean=policy.locked {
        val boundary=if(read().devices.isEmpty())pairingBoundary() else null;var installed:DeliveryBinding?=null
        edit {
            val peers=PeerRegistry.read(it)
            val prior=PairingIdentity.prior(peers,invitation,pcId,s(it,"deviceId"))
            val current=Prefs(deviceId=s(it,"deviceId"),peers=peers,pairingRevision=l(it,"pairingRevision"))
            if(prior!=null &&
                (replacement==null || replacement.pcId!=prior.deviceId || !replacement.matches(current)))return@edit
            if(replacement!=null && (prior==null || !replacement.matches(current)))return@edit
            require(pcId!=s(it,"deviceId")) {"Remote device ID is the phone's own ID"}
            if(prior!=null && prior.pin!=invitation.fingerprint && !prior.pairingLost)return@edit
            val setup=SetupStorage.read(it)
            if(setupFence!=null && (setup.withdrawalRevision!=setupFence.withdrawalRevision || setup.barriers["$pcId/${invitation.fingerprint}"]?.pairingFence()!=setupFence.barriers["$pcId/${invitation.fingerprint}"]?.pairingFence()))return@edit
            val first=peers.isEmpty()
            val unchanged=prior!=null && !PairingIdentity.changes(prior,invitation,pcId)
            val generation=if(unchanged)requireNotNull(prior).generation else Math.addExact(l(it,"pairingRevision"),1)
            if(!unchanged)it[longPreferencesKey("pairingRevision")]=generation
            var peer=if(unchanged)requireNotNull(prior).copy(name=invitation.name,addresses=invitation.addresses,pairingLost=false) else Peer(invitation.name,invitation.fingerprint,pcId,invitation.addresses,generation=generation,
                lastLan=prior?.takeIf {it.deviceId==pcId && it.pin==invitation.fingerprint}?.lastLan.orEmpty(),
                lastConnected=prior?.takeIf {it.deviceId==pcId && it.pin==invitation.fingerprint}?.lastConnected ?: 0,
                clipboardSend=prior?.clipboardSend ?: true,clipboardReceive=prior?.clipboardReceive ?: true,automaticMedia=prior?.automaticMedia ?: true,
                settings=prior?.takeIf {it.deviceId==pcId}?.settings.orEmpty(),localEdits=prior?.takeIf {it.deviceId==pcId}?.localSettingsEdits.orEmpty(),settingsSupported=prior?.settingsSupported)
            peer=peer.copy(setupDecision=prior?.setupScope ?: if(setupChoice)SetupScope.ENABLED else SetupScope.DISABLED,
                addedViaId=null,addedViaName=null)
            SetupTransactions.paired(it,setup,peer,prior,setupChoice,setupFence!=null,
                selfPin=if(setup.withdrawn)sha256(TlsIdentity(s(it,"tlsAlias")).certificate.encoded) else null)
            PeerRegistry.write(it,if(prior==null)peers+peer else peers.map {p->if(p==prior)peer else p})
            if(prior!=null && !unchanged)PeerRegistry.reset(it,prior.deviceId)
            it[booleanPreferencesKey("pairingLost")]=false;it.remove(stringPreferencesKey("lostPcName"))
            if(first) {
                requireNotNull(boundary)
                advanceEpoch(it)
                for(prefix in listOf("image","video")) {
                    val oldVersion=s(it,prefix+"Version")
                    it[longPreferencesKey(prefix+"Generation")]=AdmissionPolicy.watermark(oldVersion,l(it,prefix+"Generation"),boundary.version,boundary.generation+1)
                    it[longPreferencesKey(prefix+"Floor")]=AdmissionPolicy.watermark(oldVersion,l(it,prefix+"Floor"),boundary.version,boundary.generation+1)
                    val id=if(prefix=="image")boundary.imageId else boundary.videoId
                    it[longPreferencesKey(prefix+"PairId")]=AdmissionPolicy.watermark(oldVersion,l(it,prefix+"PairId"),boundary.version,id)
                    for(key in if(prefix=="image")listOf("photosAfterId","screenshotsAfterId") else listOf("recordingsAfterId"))it[longPreferencesKey(key)]=AdmissionPolicy.watermark(oldVersion,l(it,key),boundary.version,id)
                    it[stringPreferencesKey(prefix+"Version")]=boundary.version
                }
            }
            it[longPreferencesKey("mediaSince")]=System.currentTimeMillis()/1000
            installed=DeliveryBinding(s(it,"deviceId"),peer.pin,peer.deviceId,peer.generation)
            beforeInstall(it,requireNotNull(installed))
            onInstalled(requireNotNull(installed))
        }
        installed!=null
    }
    /** Scan progress is monotonic; baselines/floor changes invalidate older work. */
    suspend fun checkpoint(video:Boolean,version:String,generation:Long,pairId:Long?=null,floor:Long?=null,expected:AdmissionToken?=null):Boolean=policy.locked {
        if(expected!=null && !AdmissionPolicy.accepts(expected.copy(version=version),read(),readCatalogVersion()))return@locked false
        edit {
            val prefix=if(video)"video" else "image"
            val oldVersion=s(it,prefix+"Version")
            val nextFloor=floor?.let {f->AdmissionPolicy.watermark(oldVersion,l(it,prefix+"Floor"),version,f)}
            if(oldVersion!=version || floor!=null || pairId!=null)advanceEpoch(it)
            it[longPreferencesKey(prefix+"Generation")]=AdmissionPolicy.watermark(oldVersion,l(it,prefix+"Generation"),version,generation)
            it[stringPreferencesKey(prefix+"Version")]=version
            if(nextFloor!=null)it[longPreferencesKey(prefix+"Floor")]=nextFloor
            if(pairId!=null) {
                it[longPreferencesKey(prefix+"PairId")]=if(oldVersion==version)maxOf(l(it,prefix+"PairId"),pairId) else pairId
                for(key in if(video)listOf("recordingsAfterId") else listOf("photosAfterId","screenshotsAfterId")) {
                    it[longPreferencesKey(key)]=if(oldVersion==version)maxOf(l(it,key),pairId) else pairId
                }
            }
        }
        true
    }
    internal suspend fun originateText(floor:Long,apply:Boolean=true):Pair<ClipboardStamp,Long> =originateTexts(1,floor,apply).single()
    /** The entire batch shares one policy lock and one durable edit. No local or
     * remote origination can acquire an order between its elements. */
    internal suspend fun originateTexts(count:Int,floor:Long,apply:Boolean=true):List<Pair<ClipboardStamp,Long>> =policy.locked {
        require(count>=0)
        var result=emptyList<Pair<ClipboardStamp,Long>>()
        if(count>0)edit {p->
            result=List(count) {index->
                val time=maxOf(System.currentTimeMillis(),Math.addExact(l(p,"outgoingTextTime"),1),Math.addExact(floor,1))
                val stamp=MeshClock.next(p,s(p,"deviceId"),apply)
                p[longPreferencesKey("outgoingTextTime")]=time
                beforeMutation("textOrderReserved:$index")
                stamp to time
            }
        }
        result
    }
    suspend fun originateClipboard(apply:Boolean=true):ClipboardStamp=policy.locked {
        var stamp:ClipboardStamp?=null
        edit {stamp=MeshClock.next(it,s(it,"deviceId"),apply)}
        requireNotNull(stamp)
    }
    internal suspend fun legacyClipboardStamp(peerId:String):ClipboardStamp {
        var stamp:ClipboardStamp?=null
        edit {stamp=MeshClock.next(it,peerId,false)}
        return requireNotNull(stamp)
    }
    internal suspend fun rememberClipboardHash(hash:String,source:String="") {edit {
        ClipboardHistory.remember(it,hash,source)
    }}
    internal suspend fun observeClipboard(stamp:ClipboardStamp) {edit {MeshClock.observe(it,stamp.hlc)}}
    /** Legacy image effects also fence older legacy text when either durable
     * follow-up fails. Scope this auxiliary gate to the original binding. */
    internal suspend fun legacyClipboardTime(peer:String,generation:Long,durable:Long):Long {
        val p=store.data.first()
        val saved=if(p[longPreferencesKey("legacyImageGeneration:$peer")]==generation)p[longPreferencesKey("legacyImageTime:$peer")] ?: -1 else -1
        val unfinished=unfinishedEffects.values.filter {it.peerId==peer && it.generation==generation}.mapNotNull {it.legacyTime}.maxOrNull() ?: -1
        return maxOf(durable,saved,unfinished)
    }
    internal suspend fun clipboardWins(stamp:ClipboardStamp)=effectiveClipboardRegister(store.data.first())?.let {stamp>it} ?: true
    private val clipboardCompletions=stringSetPreferencesKey("clipboardCompletions")
    private fun completionKey(peerId:String,generation:Long,id:String)="$peerId/$generation/$id"
    internal suspend fun unfinishedClipboardCompletions(peerId:String,generation:Long):Set<String> {
        val prefix="$peerId/$generation/"
        return store.data.first()[clipboardCompletions].orEmpty().filter {it.startsWith(prefix)}.map {it.removePrefix(prefix)}.toSet()
    }
    /** Commit the ordering register, ownership and unfinished Room completion
     * together. A Room retry finishes the journal without another OS effect. */
    internal suspend fun appliedClipboard(stamp:ClipboardStamp,hash:String,peerId:String,generation:Long,id:String) {
        beforeMutation("clipboardApplied")
        edit {p->
            if(MeshClock.wins(p,stamp)) {MeshClock.applied(p,stamp);ClipboardHistory.remember(p,hash,peerId)}
            val current=PeerRegistry.read(p).map {"${it.deviceId}/${it.generation}/"}
            p[clipboardCompletions]=p[clipboardCompletions].orEmpty().filter {key->current.any(key::startsWith)}.toSet()+completionKey(peerId,generation,id)
        }
    }
    internal suspend fun finishClipboardCompletion(peerId:String,generation:Long,id:String) {edit {p->
        p[clipboardCompletions]=p[clipboardCompletions].orEmpty()-completionKey(peerId,generation,id)
    }}
    suspend fun setPairSwitch(peerId:String,feature:String,enabled:Boolean)=policy.locked {setPairSwitchLocked(peerId,feature,enabled)}
    /** Caller holds policy, including across the associated queue transaction. */
    internal suspend fun setPairSwitchLocked(peerId:String,feature:String,enabled:Boolean)=edit {p->
        beforeMutation("pairSwitch")
        val peers=PeerRegistry.read(p);val peer=requireNotNull(peers.find {it.deviceId==peerId})
        val key=PairSettings.keys(s(p,"deviceId"),peerId)[when(feature) {"sendClipboard"->0;"receiveClipboard"->1;"automaticMedia"->2;else->error("Unknown pair switch")}]
        if(peer.settings.getValue(key).enabled!=enabled) {
            val stamp=MeshClock.next(p,s(p,"deviceId"),false)
            PeerRegistry.write(p,peers.map {if(it==peer)it.copy(settings=it.settings+(key to PairField(enabled,stamp.hlc.toString(),stamp.source)),localEdits=it.localSettingsEdits+key) else it})
        }
    }
    internal suspend fun prepareSettings(binding:DeliveryBinding):com.google.gson.JsonObject=policy.locked {
        var result:com.google.gson.JsonObject?=null
        edit {p->
            val peers=PeerRegistry.read(p);val peer=requireNotNull(peers.find {it.deviceId==binding.pcId && it.generation==binding.generation && it.pin==binding.pin})
            result=if(peer.pendingSettings.isNotEmpty())com.google.gson.JsonParser.parseString(peer.pendingSettings).asJsonObject
                else peer.acknowledgedSnapshot.takeIf(String::isNotEmpty)?.let {com.google.gson.JsonParser.parseString(it).asJsonObject}
                    ?.takeIf {PairSettings.canonical(it)==PairSettings.signature(peer)}
                    ?: json("transfer_id" to randomId(),"settings" to PairSettings.snapshot(peer))
            PeerRegistry.write(p,peers.map {if(it==peer)it.copy(pendingSettings=result.toString()) else it})
        }
        requireNotNull(result)
    }
    internal suspend fun acknowledgeSettings(binding:DeliveryBinding,body:com.google.gson.JsonObject)=policy.locked {edit {p->
        PeerRegistry.write(p,PeerRegistry.read(p).map {peer->
            if(peer.deviceId==binding.pcId && peer.generation==binding.generation && peer.pendingSettings==body.toString())
                peer.copy(pendingSettings="",acknowledgedSettings=body.toString(),confirmedSettings=body["settings"].toString(),
                    localEdits=peer.localSettingsEdits.filterNot {key->PairSettings.contains(body, key,peer.settings.getValue(key))}.toSet()) else peer
        })
    }}
    /** Room reserves the ID before DataStore merges. A pending reservation is a
     * crash journal: identical retry can finish, conflicting reuse never can.
     * No ACK is emitted until both durable stores have completed. */
    internal suspend fun receiveSettings(binding:DeliveryBinding,body:com.google.gson.JsonObject,queue:QueueDao):Boolean=policy.locked {
        val id=body["transfer_id"].asString
        val hash=sha256(PairSettings.canonical(body).toByteArray())
        var duplicate=false
        edit {p->
            val peers=PeerRegistry.read(p);val peer=requireNotNull(peers.find {it.deviceId==binding.pcId && it.generation==binding.generation && it.pin==binding.pin})
            require(s(p,"deviceId")==binding.phoneId && binding.pcId !in PeerRegistry.resets(p) && !peer.pairingLost)
            // Validate the entire snapshot, including all clocks, before reserving.
            PairSettings.validate(peer,s(p,"deviceId"),body)
            val old=queue.receipt(binding.pcId,id)
            if(old!=null && (old.origin!="peer_settings" || old.hash!=hash))throw SettingsConflict()
            val merged=PairSettings.merge(peer,s(p,"deviceId"),body)
            if(old==null) {
                // Indexed Room history has no lifetime count limit. Preserve ID
                // conflicts for the binding lifetime, just like file receipts.
                // Actual storage failures remain retryable; reconnects replay
                // the acknowledged ID instead of growing this history.
                check(queue.insertReceipt(Receipt().apply {peerId=binding.pcId;transferId=id;this.hash=hash;origin="peer_settings";createdAt=System.currentTimeMillis()})!=-1L)
            }
            beforeMutation("settingsReserved")
            duplicate=old?.state=="applied"
            body["settings"].asJsonArray.forEach {MeshClock.observe(p,it.asJsonObject["hlc"].asString.toBigInteger())}
            PeerRegistry.write(p,peers.map {if(it==peer)merged.copy(localEdits=merged.localSettingsEdits.filterNot {key->PairSettings.contains(body,key,merged.settings.getValue(key))}.toSet()) else it})
        }
        beforeMutation("settingsMerged")
        queue.updateReceipt(requireNotNull(queue.receipt(binding.pcId,id)).apply {state="applied"})
        duplicate
    }
    internal suspend fun migrateSettingsReceipts(queue:QueueDao)=policy.locked {
        edit {p->
            val peers=PeerRegistry.read(p)
            peers.filter {it.legacySettingsReceipts.isNotEmpty()}.forEach {peer->
                peer.legacySettingsReceipts.forEach {(id,hash)->
                    val old=queue.receipt(peer.deviceId,id)
                    if(old!=null && (old.origin!="peer_settings" || old.hash!=hash))throw SettingsConflict()
                    queue.insertReceipt(Receipt().apply {peerId=peer.deviceId;transferId=id;this.hash=hash;origin="peer_settings";state="applied"})
                }
            }
            if(peers.any {it.legacySettingsReceipts.isNotEmpty()})PeerRegistry.write(p,peers.map {it.copy(settingsReceipts=emptyMap())})
        }
    }
    internal class SettingsConflict:Exception()
    internal suspend fun hello(binding:DeliveryBinding,name:String,addresses:List<Endpoint>,mesh:Boolean=true,setup:Boolean=false,remoteSetupScope:SetupScope?=null) {edit {p->
        PeerRegistry.write(p,PeerRegistry.read(p).map {if(it.deviceId==binding.pcId && it.generation==binding.generation && it.pin==binding.pin)
            it.copy(name=name,helloObserved=true,settingsSupported=mesh,setupSupported=setup,remoteSetupScope=remoteSetupScope.takeIf {setup},
                addresses=if(addresses.isEmpty())it.addresses else (addresses+it.addresses).distinct().take(16)) else it})
    }}
    internal suspend fun introducedRefusal(binding:DeliveryBinding,authenticated:Boolean) {edit {p->
        PeerRegistry.write(p,PeerRegistry.read(p).map {peer->refusedPeer(peer,binding,authenticated)})
    }}
    suspend fun finishPeerReset(peerId:String) {edit {PeerRegistry.finishReset(it,peerId)}}
    suspend fun forget(lostPcName:String?=null,peerId:String?=null):Unit=policy.locked {
        beforeMutation("forget")
        edit {
            PeerRegistry.migrate(it)
            val peers=PeerRegistry.read(it)
            val target=peers.find {peer->peer.deviceId==(peerId ?: peers.firstOrNull()?.deviceId)}
            if(target!=null) {
                if(lostPcName!=null)PeerRegistry.write(it,peers.map {peer->if(peer==target)peer.copy(pairingLost=true) else peer})
                else {
                    PeerRegistry.remove(it,target.deviceId)
                    SetupStorage.write(it,SetupStorage.read(it))
                }
                PeerRegistry.reset(it,target.deviceId)
                it.remove(longPreferencesKey("legacyImageTime:${target.deviceId}"))
                it.remove(longPreferencesKey("legacyImageGeneration:${target.deviceId}"))
            }
            if(peers.size<=1)advanceEpoch(it)
            it[booleanPreferencesKey("pairingLost")]=lostPcName!=null
            it[stringPreferencesKey("lostPcName")]=lostPcName.orEmpty()
            // Device ID and Keystore alias belong to the phone, never to a pair.
        }
    }
}

internal fun refusedPeer(peer:Peer,binding:DeliveryBinding,authenticated:Boolean)=
    if(authenticated && peer.addedViaId!=null && peer.deviceId==binding.pcId && peer.pin==binding.pin && peer.generation==binding.generation)
        peer.copy(setupRefused=true) else peer
