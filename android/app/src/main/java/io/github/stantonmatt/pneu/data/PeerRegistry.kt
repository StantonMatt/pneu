package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.Gson
import com.google.gson.reflect.TypeToken
import io.github.stantonmatt.pneu.protocol.Endpoint

/** All changes run inside the same atomic DataStore transaction. Malformed
 * storage throws; it must never silently restore stale legacy trust. */
internal object PeerRegistry {
    const val MAX_PEERS=16
    private val key=stringPreferencesKey("peers")
    private val resetKey=stringSetPreferencesKey("resetPeers")
    private val gson=Gson()
    fun read(p:androidx.datastore.preferences.core.Preferences):List<Peer> =
        p[key]?.let {requireNotNull(gson.fromJson<List<Peer>>(it,object:TypeToken<List<Peer>>(){}.type)).also {peers->require(peers.size<=MAX_PEERS)}} ?: emptyList()
    fun write(p:MutablePreferences,peers:List<Peer>) {require(peers.size<=MAX_PEERS);require(peers.map {it.deviceId}.distinct().size==peers.size);val encoded=gson.toJsonTree(peers.map {PairSettings.initialize(it,p[stringPreferencesKey("deviceId")].orEmpty())}).asJsonArray
        encoded.forEach {if(it.asJsonObject["settingsReceipts"]?.asJsonObject?.size().let {size->size==null || size==0})it.asJsonObject.remove("settingsReceipts")}
        p[key]=gson.toJson(encoded)}
    fun resets(p:androidx.datastore.preferences.core.Preferences)=p[resetKey].orEmpty()
    fun reset(p:MutablePreferences,id:String) {p[resetKey]=resets(p)+id}
    fun finishReset(p:MutablePreferences,id:String) {p[resetKey]=resets(p)-id}
    fun remove(p:MutablePreferences,id:String) {write(p,read(p).filter {it.deviceId!=id})}
    fun migrate(p:MutablePreferences) {
        if(p[key]!=null) {read(p);return}
        fun s(k:String)=p[stringPreferencesKey(k)].orEmpty()
        fun l(k:String)=p[longPreferencesKey(k)] ?: 0
        val peers=if(s("pin").isEmpty())emptyList() else listOf(Peer(s("pcName"),s("pin"),s("pcId"),
            s("addresses").split('|').filter {it.isNotEmpty()}.map(Endpoint::parse),lastLan=s("lastLan"),lastConnected=l("lastConnected"),
            generation=l("pairingRevision"),pairingLost=p[booleanPreferencesKey("pairingLost")] ?: false,
            clipboardReceive=p[booleanPreferencesKey("clipboardFromPc")] ?: true))
        write(p,peers)
        p[stringPreferencesKey("legacyQueuePeer")]=peers.firstOrNull()?.deviceId.orEmpty()
        p[longPreferencesKey("legacyQueueGeneration")]=peers.firstOrNull()?.generation ?: 0
        if(p[booleanPreferencesKey("deliveryReset")]==true)peers.forEach {reset(p,it.deviceId)}
        listOf("pin","pcName","pcId","addresses","lastLan").forEach {p.remove(stringPreferencesKey(it))}
    }
}
