package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*

/** Local consent, deliberately independent of PEER_SETTINGS and the peer's advertised capabilities. */
data class NotificationConsent(val enabled:Boolean=false,val initialized:Boolean=false,val namesOnly:Boolean=false,
    val computers:Set<String> = emptySet(),val apps:Set<String> = emptySet()) {
    companion object {
        fun computer(peer:Peer)="${peer.deviceId}/${peer.pin}/${peer.generation}"
        fun app(profile:Int,pkg:String)="$profile/$pkg"
    }
    fun allows(p:Prefs,peer:Peer,access:Boolean,capable:Boolean)=enabled && access && capable && !p.paused &&
        peer.role=="computer" && !peer.pairingLost && peer.deviceId !in p.resetPeers && computer(peer) in computers
}

/** OS-provided content only. No Android key, tag, intent or profile identifier reaches the wire. */
data class MirrorNotice(val localKey:String,val profile:Int,val pkg:String,val label:String,val time:Long,
    val title:String?=null,val text:String?=null,val icon:String?=null,val secret:Boolean=false,
    val ongoing:Boolean=false,val summary:Boolean=false,val alerting:Boolean=true,val postSequence:Long=0) {
    fun eligible(consent:NotificationConsent)=!secret && !ongoing && !summary && alerting &&
        NotificationConsent.app(profile,pkg) in consent.apps
}

internal fun notificationParagraph(value:String?,limit:Int):String?=value?.let {
    utf8Prefix(it.replace("\u0000","").lineSequence().firstOrNull().orEmpty().trim(),limit)
}

/** Reconcile only live posts after the connection's local post fence. No payload queue or disk history. */
class NotificationMirrorSession(private var afterPost:Long=0,private val newKey:()->String={randomId()}) {
    private var cleared=false
    private var authorizedSinceClear=false
    private val keys=linkedMapOf<String,String>()
    private val sent=linkedMapOf<String,NotificationEvent>()
    private val icons=mutableSetOf<String>()
    private var iconBytes=0
    private var cursor=0
    fun next(active:List<MirrorNotice>,consent:NotificationConsent,authorized:Boolean):NotificationEvent? {
        if(!authorized)afterPost=maxOf(afterPost,active.maxOfOrNull {it.postSequence} ?: 0)
        if(!cleared || !authorized && authorizedSinceClear)return NotificationEvent(NotificationAction.CLEAR)
        if(!authorized)return null
        authorizedSinceClear=true
        val current=active.asSequence().filter {it.postSequence>afterPost && it.eligible(consent)}.sortedWith(compareByDescending<MirrorNotice> {it.time}.thenBy {it.localKey}).take(128).toList()
        val live=current.map {it.localKey}.toSet()
        keys.keys.retainAll(live+sent.keys)
        sent.keys.firstOrNull {it !in live}?.let {return NotificationEvent(NotificationAction.REMOVE,keys[it])}
        // Rotate through active keys so a rapidly updating app cannot starve the rest.
        for(i in current.indices) {
            val notice=current[(cursor+i)%current.size]
            val key=keys.getOrPut(notice.localKey,newKey)
            val event=NotificationEvent(NotificationAction.UPSERT,key,notificationParagraph(notice.pkg,255),
                notificationParagraph(notice.label,128)?.ifEmpty {notice.pkg.take(128)},notice.time.coerceIn(0,MAX_TIMESTAMP),
                if(consent.namesOnly)null else notificationParagraph(notice.title,1024),
                if(consent.namesOnly)null else notificationParagraph(notice.text,8192))
            if(sent[notice.localKey]!=event) {
                val icon=notice.icon?.takeIf {notice.pkg !in icons && icons.size<128 && it.length/2<=8192 && iconBytes+it.length/2<=1_048_576}
                val withIcon=event.copy(iconPng=icon)
                // Worst-case JSON escaping can multiply text bytes by six. Drop the icon first.
                return if(com.google.gson.Gson().toJson(withIcon).toByteArray(Charsets.UTF_8).size>65_536)event else withIcon
            }
        }
        return null
    }
    fun committed(event:NotificationEvent) {
        when(event.action) {
            NotificationAction.CLEAR -> {cleared=true;authorizedSinceClear=false;sent.clear();keys.clear()}
            NotificationAction.REMOVE -> keys.entries.find {it.value==event.key}?.key?.let {keys.remove(it);sent.remove(it)}
            NotificationAction.UPSERT -> {
                val local=keys.entries.first {it.value==event.key}.key
                sent[local]=event.copy(iconPng=null);cursor++
                if(event.iconPng!=null && icons.add(requireNotNull(event.appPackage)))iconBytes+=event.iconPng.length/2
            }
        }
    }
    /** Recompute immediately before starting a frame, after acquiring the transport/policy fences. */
    fun current(event:NotificationEvent,active:List<MirrorNotice>,consent:NotificationConsent,authorized:Boolean)=
        event.action!=NotificationAction.UPSERT || next(active,consent,authorized)==event
}

/** Shared, fair global token bucket. A waiting connection retains its turn, never its payload. */
class NotificationRateLimit(private val clock:()->Long) {
    private var tokens=20.0
    private var last=clock()
    private val waiting=linkedMapOf<String,Pair<Long,Boolean>>()
    @Synchronized fun take(owner:String,cleanup:Boolean):Boolean {
        val now=clock();tokens=minOf(20.0,tokens+(now-last).coerceAtLeast(0)/100.0);last=now
        waiting.entries.removeAll {now-it.value.first>=500}
        waiting[owner]=now to cleanup
        val first=waiting.entries.firstOrNull {it.value.second}?.key ?: waiting.keys.first()
        if(tokens<1 || owner!=first)return false
        tokens--;waiting.remove(owner);return true
    }
    @Synchronized fun forget(owner:String) {waiting.remove(owner)}
}
