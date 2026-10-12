package io.github.stantonmatt.pneu.data

import android.content.Context
import androidx.datastore.preferences.core.*
import androidx.datastore.preferences.preferencesDataStore
import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.connection.TlsIdentity
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex

private val Context.pneuDataStore by preferencesDataStore("pneu")
data class Peer(val name: String, val pin: String, val deviceId: String, val addresses: List<Endpoint>)
data class Prefs(
    val setup: Boolean = false, val paused: Boolean = false,
    val screenshots: Boolean = true, val photos: Boolean = false, val recordings: Boolean = false,
    val wifi: Boolean = true, val tailscale: Boolean = true, val mobile: Boolean = true,
    val originals: Boolean = false, val queue: Boolean = true, val clipboardButton: Boolean = true,
    val clipboardFromPc: Boolean = true, val shareSheet: Boolean = true, val quickSettings: Boolean = true,
    val deviceId: String = "", val peer: Peer? = null, val pending: Invitation? = null,
    val lastLan: String = "", val lastConnected: Long = 0, val mediaSince: Long = 0,
    val pairingLost: Boolean = false, val neverSleeping: Boolean = false,
    val photosSince: Long = 0, val recordingsSince: Long = 0, val photosAfterId: Long = 0, val recordingsAfterId: Long = 0,
    val lastReceivedHash: String = "", val lastSentHash: String = "", val localCapacity: Boolean = false,
    val deliveryReset: Boolean=false,
    val imageVersion: String="",val videoVersion: String="",val imageGeneration: Long=0,val videoGeneration: Long=0,
    val imagePairId: Long=0,val videoPairId: Long=0, val lostPcName: String="", val tlsAlias:String="",
    val imageFloor:Long=0,val videoFloor:Long=0,val policyEpoch:Long=0,val screenshotsAfterId:Long=0, val pairingRevision:Long=0, val loaded:Boolean=true
)
class Preferences(private val context: Context) {
    internal var beforeMutation:(String)->Unit={}
    @Volatile internal var beforeRead:suspend ()->Unit={}
    @Volatile internal var beforeReadStart:suspend ()->Unit={}
    @Volatile internal var readBoundary:suspend (Boolean)->Long={MediaCatalog.latest(context,it)}
    internal val policy=AdmissionPolicy()
    @Volatile internal var readCatalogVersion:suspend ()->String={MediaCatalog.version(context)}
    @Volatile internal var readSnapshot:suspend ()->CatalogSnapshot={MediaCatalog.snapshot(context)}
    @androidx.annotation.VisibleForTesting(otherwise=androidx.annotation.VisibleForTesting.PRIVATE)
    internal val store = context.pneuDataStore
    private fun b(p: androidx.datastore.preferences.core.Preferences, key: String, default: Boolean = false) = p[booleanPreferencesKey(key)] ?: default
    private fun s(p: androidx.datastore.preferences.core.Preferences, key: String) = p[stringPreferencesKey(key)] ?: ""
    private fun l(p: androidx.datastore.preferences.core.Preferences, key: String) = p[longPreferencesKey(key)] ?: 0
    private val raw: Flow<Prefs> = store.data.onEach {beforeRead()}.map { p ->
        val addresses = s(p, "addresses").split('|').filter(String::isNotEmpty).mapNotNull { runCatching { Endpoint.parse(it) }.getOrNull() }
        val peer = s(p, "pin").takeIf { it.isNotEmpty() }?.let { Peer(s(p, "pcName"), it, s(p, "pcId"), addresses) }
        val pending = s(p, "pending").takeIf { it.isNotEmpty() }?.let { runCatching { Invitation.parse(it) }.getOrNull() }
        Prefs(b(p,"setup"),b(p,"paused"),b(p,"screenshots",true),b(p,"photos"),b(p,"recordings"),b(p,"wifi",true),b(p,"tailscale",true),b(p,"mobile",true),b(p,"originals"),b(p,"queue",true),b(p,"clipboardButton",true),b(p,"clipboardFromPc",true),b(p,"shareSheet",true),b(p,"quickSettings",true),s(p,"deviceId"),peer,pending,s(p,"lastLan"),l(p,"lastConnected"),l(p,"mediaSince"),b(p,"pairingLost"),b(p,"neverSleeping"),l(p,"photosSince"),l(p,"recordingsSince"),l(p,"photosAfterId"),l(p,"recordingsAfterId"),s(p,"lastReceivedHash"),s(p,"lastSentHash"),b(p,"localCapacity"),b(p,"deliveryReset"),s(p,"imageVersion"),s(p,"videoVersion"),l(p,"imageGeneration"),l(p,"videoGeneration"),l(p,"imagePairId"),l(p,"videoPairId"),s(p,"lostPcName"),s(p,"tlsAlias"),l(p,"imageFloor"),l(p,"videoFloor"),l(p,"mediaEpoch"),l(p,"screenshotsAfterId"),l(p,"pairingRevision"))
    }
    val flow=raw.recoverReads()
    suspend fun read():Prefs=storageDeadline {beforeReadStart();raw.first()}
    /** Generate the key before publishing its ID/alias in the same durable edit.
     * Upgrades preserve the old fixed certificate for the existing identity. */
    suspend fun ensureIdentity():Unit=policy.locked {
        beforeMutation("ensureIdentity")
        var retired=""
        store.edit {
            it.remove(stringPreferencesKey("pauseHistory")) // One-time legacy state removal.
            val existing=s(it,"deviceId");val savedAlias=s(it,"tlsAlias")
            // Legacy offline forget changed IDs without certificates. With no
            // binding/recovery record, migrate that possibly mismatched identity.
            val legacyUnpaired=existing.isNotEmpty() && savedAlias.isEmpty() && s(it,"pin").isEmpty() && s(it,"pending").isEmpty()
            val id=if(legacyUnpaired)randomId() else existing.ifEmpty {randomId()}
            val alias=savedAlias.ifEmpty {if(existing.isNotEmpty() && !legacyUnpaired)TlsIdentity.LEGACY_ALIAS else TlsIdentity.aliasFor(id)}
            TlsIdentity(alias,create=true)
            if(existing!=id || savedAlias!=alias)advanceEpoch(it)
            it[stringPreferencesKey("deviceId")]=id;it[stringPreferencesKey("tlsAlias")]=alias
            if(legacyUnpaired)retired=TlsIdentity.LEGACY_ALIAS
        }
        runCatching {TlsIdentity.retire(retired)};Unit
    }
    private fun advanceEpoch(p:MutablePreferences) {p[longPreferencesKey("mediaEpoch")]=AdmissionPolicy.next(l(p,"mediaEpoch"))}
    private val policyKeys=setOf("paused","screenshots","photos","recordings","wifi","tailscale","mobile","originals","queue","deliveryReset")
    suspend fun toggle(key:String,value:Boolean)=kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
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
            store.edit {
                if(!value && ((key=="wifi" && !b(it,"tailscale",true)) || (key=="tailscale" && !b(it,"wifi",true))))throw IllegalArgumentException("Pneu needs Wi-Fi or Tailscale")
                if(key in policyKeys && b(it,key,key in listOf("screenshots","wifi","tailscale","mobile","queue"))!=value)advanceEpoch(it)
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
        if(key in policyKeys)policy.locked {apply()} else apply()
    }
    /** Automatic bytes/skips are published only under the same policy boundary.
     * Reading the live namespace here rejects URI reuse after a catalog rebuild. */
    internal suspend fun publishAutomatic(token:AdmissionToken,publicationMutex:Mutex,body:suspend (Prefs)->Boolean):Boolean=policy.publishing(publicationMutex) {
        // Validate AFTER waiting for the queue, not before. The checked value
        // goes straight to its commit; no second settings read can suspend it.
        val current=read()
        if(!AdmissionPolicy.accepts(token,current,readCatalogVersion()) || !MediaCatalog.fullAccess(context,token.video))false else body(current)
    }
    suspend fun setLong(key: String, value: Long) { store.edit { it[longPreferencesKey(key)] = value } }
    suspend fun setString(key: String, value: String) {
        if(key in setOf("deviceId","tlsAlias"))policy.locked {store.edit {if(s(it,key)!=value)advanceEpoch(it);it[stringPreferencesKey(key)]=value}}
        else store.edit {it[stringPreferencesKey(key)]=value}
    }
    /** Old sessions cannot repopulate a replacement PC's connection cache. */
    internal suspend fun connected(binding:DeliveryBinding,endpoint:Endpoint,at:Long=System.currentTimeMillis()):Boolean {
        var recorded=false
        store.edit {
            if(s(it,"deviceId")!=binding.phoneId || s(it,"pin")!=binding.pin || s(it,"pcId")!=binding.pcId || b(it,"deliveryReset") || b(it,"pairingLost"))return@edit
            if(!endpoint.tailscale)it[stringPreferencesKey("lastLan")]=endpoint.toString()
            it[longPreferencesKey("lastConnected")]=at
            recorded=true
        }
        return recorded
    }
    suspend fun pending(uri: String)=policy.locked {beforeMutation("pending");store.edit {advanceEpoch(it);it[stringPreferencesKey("pending")]=uri;it[booleanPreferencesKey("pairingLost")]=false}}

    suspend fun cancelPending(expected: Invitation?=null):Boolean=policy.locked {
        beforeMutation("cancelPending");var cancelled=false
        store.edit {
            val current=s(it,"pending").takeIf(String::isNotEmpty)?.let {raw->runCatching {Invitation.parse(raw)}.getOrNull()}
            if(expected==null || expected==current) {if(current!=null)advanceEpoch(it);it.remove(stringPreferencesKey("pending"));cancelled=true}
        }
        cancelled
    }
    suspend fun paired(invitation: Invitation, pcId: String, expectedPending: Invitation?=null): Boolean=policy.locked {
        val boundary=MediaCatalog.boundary(context);var installed=false
        store.edit {
            if(expectedPending!=null && runCatching {Invitation.parse(s(it,"pending"))}.getOrNull()!=expectedPending)return@edit
            val reset=s(it,"pin")!=invitation.fingerprint || s(it,"pcId")!=pcId
            advanceEpoch(it)
            it[stringPreferencesKey("pin")]=invitation.fingerprint;it[stringPreferencesKey("pcName")]=invitation.name
            it[stringPreferencesKey("pcId")]=pcId;it[stringPreferencesKey("addresses")]=invitation.addresses.joinToString("|")
            it.remove(stringPreferencesKey("pending"));it[booleanPreferencesKey("pairingLost")]=false;it.remove(stringPreferencesKey("lostPcName"))
            it[longPreferencesKey("pairingRevision")]=l(it,"pairingRevision")+1
            if(reset) {
                listOf("lastLan","lastReceivedHash","lastSentHash").forEach {key->it.remove(stringPreferencesKey(key))}
                it[longPreferencesKey("lastConnected")]=0
                it[booleanPreferencesKey("deliveryReset")]=true
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
            installed=true
        }
        installed
    }
    /** Scan progress is monotonic; baselines/floor changes invalidate older work. */
    suspend fun checkpoint(video:Boolean,version:String,generation:Long,pairId:Long?=null,floor:Long?=null,expected:AdmissionToken?=null):Boolean=policy.locked {
        if(expected!=null && !AdmissionPolicy.accepts(expected.copy(version=version),read(),readCatalogVersion()))return@locked false
        store.edit {
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
    suspend fun forget(lostPcName: String?=null):Unit=policy.locked {
        beforeMutation("forget")
        var retired=""
        store.edit {
        val id=randomId();val alias=TlsIdentity.aliasFor(id)
        advanceEpoch(it)
        // If Keystore fails, the old DataStore identity and binding remain intact.
        TlsIdentity(alias,create=true)
        retired=s(it,"tlsAlias").ifEmpty {TlsIdentity.LEGACY_ALIAS}
        listOf("pin","pcName","pcId","addresses","pending","lastLan","lastReceivedHash","lastSentHash").forEach { key -> it.remove(stringPreferencesKey(key)) }
        it[booleanPreferencesKey("pairingLost")] = lostPcName != null; it[booleanPreferencesKey("paused")] = false
        it[stringPreferencesKey("lostPcName")]=lostPcName.orEmpty()
        it[longPreferencesKey("lastConnected")]=0;it[booleanPreferencesKey("localCapacity")]=false
        it[stringPreferencesKey("deviceId")] = id;it[stringPreferencesKey("tlsAlias")]=alias;it[booleanPreferencesKey("deliveryReset")]=true
    }
        // Existing authenticated sockets no longer need CertificateVerify. A
        // failed retirement is harmless and must not undo durable revocation.
        runCatching {TlsIdentity.retire(retired)};Unit
    }
}
