package io.github.stantonmatt.pneu.protocol

import com.google.gson.*
import java.math.BigInteger

enum class SetupScope(val wire:String) { ENABLED("enabled"), DISABLED("disabled"), ASK("ask") }
data class SetupHello(val scope:SetupScope,val setupId:String?=null) {
    fun json()=json("scope" to scope.wire).apply {if(setupId!=null && scope!=SetupScope.DISABLED)addProperty("setup_id",setupId)}
}
data class MemberReach(val addresses:List<String>,val hlc:String,val writer:String) {
    fun json()=json("addresses" to addresses,"relay" to emptyList<String>(),"hlc" to hlc.toBigInteger(),"writer_device" to writer)
}
data class MemberReadmits(val hlc:String,val writer:String) {
    fun json()=json("hlc" to hlc.toBigInteger(),"writer_device" to writer)
}
data class SetupMember(val deviceId:String,val fingerprint:String,val role:String,val name:String,
                       val status:String,val hlc:String,val writer:String,val reach:MemberReach?=null,val readmits:MemberReadmits?=null) {
    val key get()="$deviceId/$fingerprint"
    val active get()=status=="member"
    fun json()=json("device_id" to deviceId,"fingerprint" to fingerprint,"role" to role,"name" to name,
        "status" to status,"hlc" to hlc.toBigInteger(),"writer_device" to writer).apply {reach?.let {add("reach",it.json())};readmits?.let {add("readmits",it.json())}}
    fun value()=copy(reach=null,readmits=null)
}
data class MembersSnapshot(val transferId:String,val setupId:String,val members:List<SetupMember>) {
    fun json()=json("transfer_id" to transferId,"setup_id" to setupId).apply {
        add("members",JsonArray().also {array->members.forEach {array.add(it.json())}})
    }
    // Semantic receipt identity, independent of JSON/entry order; reach order matters.
    fun canonical()=copy(transferId="0".repeat(32),members=members.sortedBy {it.key}).json().toString()
}
internal object SetupWire {
    val helloFields=setOf("scope","setup_id")
    val reachFields=setOf("addresses","relay","hlc","writer_device")
    val readmitsFields=setOf("hlc","writer_device")
    val memberFields=setOf("device_id","fingerprint","role","name","status","hlc","writer_device","reach","readmits")
    private fun text(e:JsonElement?,max:Int,min:Int=1):String {
        require(e is JsonPrimitive && e.isString)
        return e.asString.also {s->
            require('\u0000' !in s && s.toByteArray().size in min..max)
            var i=0
            while(i<s.length) {val c=s[i++];if(c.isHighSurrogate())require(i<s.length && s[i++].isLowSurrogate()) else require(!c.isLowSurrogate())}
        }
    }
    private fun hex(e:JsonElement?,n:Int)=text(e,n).also {require(it.matches(Regex("[0-9a-f]{$n}")))}
    private fun clock(e:JsonElement?):String {
        require(e is JsonPrimitive && e.isNumber && e.asString.matches(Regex("0|[1-9][0-9]*")))
        return e.asString.also {require(it.toBigInteger()<=BigInteger("18446744073709551615"))}
    }
    fun hello(o:JsonElement):SetupHello {
        require(o is JsonObject)
        val scope=SetupScope.entries.single {it.wire==text(o["scope"],8)}
        val id=if(o.has("setup_id"))hex(o["setup_id"],32) else null
        require(scope!=SetupScope.DISABLED || id==null)
        return SetupHello(scope,id)
    }
    fun validateHello(o:JsonObject,caps:List<String>) {
        if(o.has("setup"))hello(o["setup"])
        if("setup" in caps)require("mesh" in caps && o.has("role") && o.has("setup"))
    }
    fun decode(o:JsonObject):MembersSnapshot {
        val id=hex(o["transfer_id"],32);val setup=hex(o["setup_id"],32)
        val list=o["members"];require(list is JsonArray && list.size() in 1..64)
        val members=list.map {raw->
            require(raw is JsonObject)
            val device=hex(raw["device_id"],32);val pin=hex(raw["fingerprint"],64)
            val role=text(raw["role"],8).also {require(it in setOf("phone","computer"))}
            val name=text(raw["name"],128)
            val status=text(raw["status"],7).also {require(it in setOf("member","removed"))}
            val hlc=clock(raw["hlc"]);val writer=hex(raw["writer_device"],32)
            val reach=if(raw.has("reach")) {
                require(status=="member")
                val r=raw["reach"];require(r is JsonObject)
                val a=r["addresses"];require(a is JsonArray && a.size()<=8)
                val addresses=a.map {text(it,259).also {s->Protocol.validateAddress(s)}}
                require(addresses.distinct().size==addresses.size && (role!="phone" || addresses.isEmpty()))
                val relay=r["relay"];require(relay is JsonArray && relay.size()==0)
                MemberReach(addresses,clock(r["hlc"]),hex(r["writer_device"],32).also {require(it==device)})
            } else null
            val readmits=if(raw.has("readmits")) {
                require(status=="member")
                val r=raw["readmits"];require(r is JsonObject)
                MemberReadmits(clock(r["hlc"]),hex(r["writer_device"],32))
            } else null
            SetupMember(device,pin,role,name,status,hlc,writer,reach,readmits)
        }
        require(members.map {it.key}.distinct().size==members.size)
        require(members.count {it.active}<=17)
        require(members.groupBy {it.deviceId}.values.all {it.size<=4 && it.count {m->m.active}<=1})
        require(members.filter {it.active}.map {it.fingerprint}.distinct().size==members.count {it.active})
        return MembersSnapshot(id,setup,members)
    }
}
