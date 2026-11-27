package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.*
import io.github.stantonmatt.pneu.protocol.*
import java.math.BigInteger

internal class MeshViolation(val code:String="malformed_message"):Exception(code)
data class ClipboardStamp(val hlc:BigInteger,val source:String):Comparable<ClipboardStamp> {
    override fun compareTo(other:ClipboardStamp)=hlc.compareTo(other.hlc).takeIf {it!=0} ?: source.compareTo(other.source)
}
/** Unsigned wire clocks stay BigInteger; never narrow a remote u64 to Long. */
internal object MeshClock {
    private val seen=stringPreferencesKey("meshSeen")
    private val applied=stringPreferencesKey("meshApplied")
    private val source=stringPreferencesKey("meshSource")
    private val maximum=BigInteger("18446744073709551615")
    fun validate(value:BigInteger,now:Long=System.currentTimeMillis()) {
        if(value.signum()<0 || value>maximum || value>BigInteger.valueOf(now.coerceAtLeast(0))+BigInteger.valueOf(60000))throw MeshViolation()
    }
    fun observe(p:MutablePreferences,value:BigInteger,now:Long=System.currentTimeMillis()) {
        validate(value,now);p[seen]=maxOf(p[seen]?.toBigInteger() ?: BigInteger.ZERO,value).toString()
    }
    fun next(p:MutablePreferences,id:String,apply:Boolean,now:Long=System.currentTimeMillis()):ClipboardStamp {
        val value=maxOf(BigInteger.valueOf(now.coerceAtLeast(0)),maxOf(p[seen]?.toBigInteger() ?: BigInteger.ZERO,register(p)?.hlc ?: BigInteger.ZERO)+BigInteger.ONE)
        check(value<=maximum) {"Clipboard clock exhausted"};p[seen]=value.toString()
        return ClipboardStamp(value,id).also {if(apply)applied(p,it)}
    }
    fun register(p:androidx.datastore.preferences.core.Preferences)=p[applied]?.let {ClipboardStamp(it.toBigInteger(),p[source].orEmpty())}
    fun wins(p:androidx.datastore.preferences.core.Preferences,stamp:ClipboardStamp)=register(p)?.let {stamp>it} ?: true
    fun applied(p:MutablePreferences,stamp:ClipboardStamp) {p[applied]=stamp.hlc.toString();p[source]=stamp.source}
}

data class PairField(val enabled:Boolean,val hlc:String,val writer:String) {
    val stamp get()=ClipboardStamp(hlc.toBigInteger(),writer)
}
internal object PairSettings {
    fun keys(phone:String,pc:String,sms:Boolean=false)=listOf("$phone/$pc/clipboard","$pc/$phone/clipboard","$phone/$pc/automatic_media")+if(sms)listOf("$phone/$pc/sms") else emptyList()
    fun initialize(peer:Peer,phone:String):Peer {
        val keys=keys(phone,peer.deviceId,peer.smsSupported);val writer=minOf(phone,peer.deviceId)
        val values=listOf(peer.clipboardSend,peer.clipboardReceive,peer.automaticMedia,peer.sendSms)
        val fields=keys.mapIndexed {i,key->key to (peer.settings[key] ?: PairField(values[i],if(values[i])"0" else "1",if(values[i])writer else phone))}.toMap()
        return peer.copy(settings=fields+peer.settings.filterKeys {it.endsWith("/sms") && !peer.smsSupported},localEdits=peer.localSettingsEdits.intersect(keys.toSet()),clipboardSend=fields.getValue(keys[0]).enabled,
            clipboardReceive=fields.getValue(keys[1]).enabled,automaticMedia=fields.getValue(keys[2]).enabled)
    }
    fun snapshot(peer:Peer)=JsonArray().apply {peer.settings.filterKeys {!it.endsWith("/sms") || peer.smsSupported}.toSortedMap().forEach {(key,value)->
        add(json("key" to key,"enabled" to value.enabled,"hlc" to value.hlc.toBigInteger(),"writer_device" to value.writer))
    }}
    fun signature(peer:Peer)=snapshot(peer).toString()
    fun syncing(peer:Peer)=signature(peer)!=peer.confirmedSettings
    fun pending(peer:Peer)=peer.localSettingsEdits.isNotEmpty()
    fun contains(body:JsonObject,key:String,field:PairField)=body["settings"].asJsonArray.any {raw->
        val value=raw.asJsonObject
        value["key"].asString==key && value["enabled"].asBoolean==field.enabled &&
            value["hlc"].asString.toBigInteger()==field.hlc.toBigInteger() && value["writer_device"].asString==field.writer
    }
    fun canonical(body:JsonObject)=JsonArray().apply {body["settings"].asJsonArray.sortedBy {it.asJsonObject["key"].asString}.forEach {raw->
        val entry=raw.asJsonObject
        add(json("key" to entry["key"].asString,"enabled" to entry["enabled"].asBoolean,"hlc" to entry["hlc"].asString.toBigInteger(),"writer_device" to entry["writer_device"].asString))
    }}.toString()
    fun validate(peer:Peer,phone:String,body:JsonObject,now:Long=System.currentTimeMillis()) {
        Protocol.validate(Type.PEER_SETTINGS,body)
        val entries=body["settings"].asJsonArray.map {it.asJsonObject}
        if(entries.map {it["key"].asString}.toSet()!=keys(phone,peer.deviceId,peer.smsSupported).toSet())throw MeshViolation("unexpected_message")
        entries.forEach {entry->
            val field=PairField(entry["enabled"].asBoolean,entry["hlc"].asString,entry["writer_device"].asString)
            MeshClock.validate(field.hlc.toBigInteger(),now)
        }
    }
    fun merge(peer:Peer,phone:String,body:JsonObject,now:Long=System.currentTimeMillis()):Peer {
        validate(peer,phone,body,now)
        val incoming=body["settings"].asJsonArray.associate {entry->entry.asJsonObject.let {it["key"].asString to PairField(it["enabled"].asBoolean,it["hlc"].asString,it["writer_device"].asString)}}
        val merged=peer.settings.toMutableMap()
        incoming.forEach {(key,value)->
            val old=merged.getValue(key)
            if(value.stamp==old.stamp && value.enabled!=old.enabled)throw MeshViolation()
            if(value.stamp>old.stamp)merged[key]=value
        }
        return initialize(peer.copy(settings=merged),phone)
    }
}
