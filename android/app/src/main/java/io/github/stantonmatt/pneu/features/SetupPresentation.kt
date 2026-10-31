package io.github.stantonmatt.pneu.features

import android.content.Context
import android.content.res.Resources
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.SetupScope
import androidx.core.content.edit
import kotlinx.coroutines.flow.MutableStateFlow

/** The row above Remove from all devices or Unpair on a device page. */
enum class LinkRow { NONE, LINK, PENDING, THEIRS, ASKED, UPDATE, SEPARATE }
/** key: the roster key when the device is one of your devices (Remove from all devices), else a plain pairing. */
data class SetupPage(val key:String?,val addedVia:String?,val row:LinkRow) {val member get()=key!=null}
/** Another phone in your devices: listed so it can be removed, never connected to. */
data class PhoneMember(val key:String,val name:String,val addedVia:String?)
sealed interface LinkBanner {
    data class Offer(val peerId:String):LinkBanner
    data class Waiting(val names:List<String>):LinkBanner
}
/** One message for Home's snackbar. Keys identify it so it is shown once. */
sealed interface SetupNews {
    val id:String
    data class AlsoLinked(val names:List<String>,val notices:List<String>):SetupNews {override val id get()="also:"+notices.joinToString(",")}
    data class Separate(val peerId:String,val name:String,val afterPairing:Boolean):SetupNews {override val id get()="separate:$peerId"}
    data class Joined(val notice:SetupNotice,val key:String?):SetupNews {override val id get()="joined:"+notice.id}
    data object Linked:SetupNews {override val id get()="linked"}
}

/** UI-only memory. The core's durable state stays authoritative for what is linked. */
data class SetupUiState(val bannerDismissed:Boolean=false,val linkStarted:Boolean=false,
    val joinedPin:String="",val joinedAt:Long=0,val seenWithdrawal:Long=0,val announcedSeparate:Set<String> = emptySet())

/** Maps the core's setup state to what the screens show. Pure, so tests can drive every state. */
internal object SetupPresentation {
    const val LIMIT=17
    /** Only for wording a separate-setup outcome after a recent pairing, never for join notices. */
    const val PAIR_OUTCOME_WINDOW_MS=10*60*1000L
    private fun active(p:Prefs)=p.membership.setupId!=null && !p.membership.withdrawn
    /** Requests still waiting for this phone's answer, for current bindings only. */
    fun prompts(p:Prefs)=p.membership.prompts.filter {prompt->!prompt.allowRequested && prompt.setupGeneration==p.membership.generation &&
        p.devices.any {it.deviceId==prompt.peerId && it.pin==prompt.pin && it.generation==prompt.bindingGeneration && !it.pairingLost}}
    fun promptKey(prompt:SetupPrompt)="${prompt.peerId}/${prompt.pin}/${prompt.bindingGeneration}/${prompt.setupGeneration}"
    fun promptNames(p:Prefs,prompts:List<SetupPrompt>)=prompts.map {prompt->p.devices.find {it.deviceId==prompt.peerId}?.name ?: prompt.name}
    fun memberKey(p:Prefs,peer:Peer)=p.membership.member(peer.deviceId)?.takeIf {active(p) && it.fingerprint==peer.pin}?.key
    private fun participating(p:Prefs,peer:Peer)=memberKey(p,peer)!=null &&
        (peer.remoteSetupScope==null || peer.remoteSetupScope==SetupScope.ENABLED)
    fun page(p:Prefs,peer:Peer):SetupPage {
        val s=p.membership;val key=memberKey(p,peer)
        val row=when {
            peer.pairingLost || s.withdrawn->LinkRow.NONE
            peer.deviceId in s.separateLinks->LinkRow.SEPARATE
            prompts(p).any {it.peerId==peer.deviceId}->LinkRow.ASKED
            // Allowed here; it links when that device next reconnects.
            s.prompts.any {it.peerId==peer.deviceId && it.allowRequested}->LinkRow.NONE
            participating(p,peer) && peer.setupScope==SetupScope.ENABLED->LinkRow.NONE
            !peer.setupSupported->LinkRow.UPDATE
            peer.setupScope==SetupScope.ENABLED && peer.remoteSetupScope==SetupScope.DISABLED->LinkRow.THEIRS
            peer.setupScope==SetupScope.ENABLED->LinkRow.PENDING
            else->LinkRow.LINK
        }
        return SetupPage(key,peer.addedViaName ?: key?.let {s.provenance[it]?.name},row)
    }
    fun hasDevices(p:Prefs)=p.devices.isNotEmpty() || phones(p).isNotEmpty()
    fun phones(p:Prefs)=if(!active(p))emptyList() else p.membership.roster.filter {it.active && it.role=="phone" && it.deviceId!=p.deviceId}
        .map {PhoneMember(it.key,it.name,p.membership.provenance[it.key]?.name)}
    /** Pairings not decided since the update, to devices that can link. */
    fun undecided(p:Prefs):List<Peer> {
        val s=p.membership
        if(s.withdrawn || s.prompts.any {it.allowRequested})return emptyList()
        return p.devices.filter {peer->!peer.pairingLost && peer.setupScope==SetupScope.ASK && peer.setupSupported &&
            peer.deviceId !in s.separateLinks && s.prompts.none {it.peerId==peer.deviceId}}
    }
    /** Linked here, but that device hasn't allowed it yet. */
    fun waiting(p:Prefs)=if(p.membership.withdrawn)emptyList() else p.devices.filter {peer->!peer.pairingLost &&
        peer.setupScope==SetupScope.ENABLED && peer.setupSupported && peer.remoteSetupScope!=SetupScope.DISABLED &&
        peer.deviceId !in p.membership.separateLinks && !participating(p,peer)}
    fun banner(p:Prefs,ui:SetupUiState):LinkBanner? {
        if(ui.bannerDismissed || prompts(p).isNotEmpty())return null
        undecided(p).firstOrNull()?.let {return LinkBanner.Offer(it.deviceId)}
        val waiting=waiting(p)
        return if(ui.linkStarted && waiting.isNotEmpty())LinkBanner.Waiting(waiting.map {it.name}) else null
    }
    fun linked(p:Prefs)=active(p) && p.membership.roster.any {it.active && it.deviceId!=p.deviceId}
    /** A Link this phone started has nothing left to wait for. */
    fun linkSettled(p:Prefs)=waiting(p).isEmpty() && undecided(p).isEmpty() && prompts(p).isEmpty()
    fun atLimit(p:Prefs)=p.membership.roster.count {it.active}>=LIMIT
    fun notices(p:Prefs)=p.membership.notices.filter {notice->notice.peerId !in p.resetPeers && p.devices.any {it.deviceId==notice.peerId}}
    private fun recentPairVia(p:Prefs,ui:SetupUiState,now:Long)=
        ui.joinedPin.takeIf {it.isNotEmpty() && now-ui.joinedAt in 0 until PAIR_OUTCOME_WINDOW_MS}?.let {pin->p.devices.find {it.pin==pin}}
    fun alsoLinked(p:Prefs)=notices(p).filter {it.alsoLinked}
    fun joined(p:Prefs)=notices(p).filterNot {it.alsoLinked}
    /** Home's next message, in order: what this phone linked with, separate devices, joins, then Linked. */
    fun news(p:Prefs,ui:SetupUiState,now:Long,presented:Set<String> = emptySet()):SetupNews? {
        if(!p.loaded)return null
        alsoLinked(p).filterNot {it.id in presented}.takeIf {it.isNotEmpty()}?.let {list->return SetupNews.AlsoLinked(list.map {it.name},list.map {it.id})}
        val via=recentPairVia(p,ui,now)
        p.membership.separateLinks.firstOrNull {it !in ui.announcedSeparate}?.let {id->
            p.devices.find {it.deviceId==id}?.let {peer->return SetupNews.Separate(id,peer.name,peer.deviceId==via?.deviceId)}
        }
        joined(p).firstOrNull {it.id !in presented}?.let {notice->
            val peer=p.devices.find {it.deviceId==notice.peerId}
            return SetupNews.Joined(notice,peer?.let {memberKey(p,it)})
        }
        return if(ui.linkStarted && linkSettled(p) && linked(p))SetupNews.Linked else null
    }

    fun names(r:Resources,names:List<String>)=when(names.size) {
        0->"";1->names[0];2->r.getString(R.string.list_two,names[0],names[1])
        else->r.getQuantityString(R.plurals.setup_names_more,names.size-2,names[0],names[1],names.size-2)
    }
    fun requestTitle(r:Resources,names:List<String>)=when(names.size) {
        1->r.getString(R.string.setup_request_one,names[0])
        2->r.getString(R.string.setup_request_two,names[0],names[1])
        else->r.getQuantityString(R.plurals.setup_request_many,names.size-1,names[0],names.size-1)
    }
    fun bannerText(r:Resources,banner:LinkBanner)=when(banner) {
        is LinkBanner.Offer->r.getString(R.string.setup_banner_link)
        is LinkBanner.Waiting->if(banner.names.size>2)r.getQuantityString(R.plurals.setup_banner_allow_many,banner.names.size,banner.names.size)
            else r.getString(R.string.setup_banner_allow,names(r,banner.names))
    }
    fun newsText(r:Resources,news:SetupNews)=when(news) {
        is SetupNews.AlsoLinked->r.getString(R.string.setup_also_linked,names(r,news.names))
        is SetupNews.Separate->r.getString(if(news.afterPairing)R.string.setup_paired_only else R.string.setup_not_linked,news.name)
        is SetupNews.Joined->r.getString(R.string.setup_joined,news.notice.name)+" · "+r.getString(R.string.setup_added_via,news.notice.introducerName)
        SetupNews.Linked->r.getString(R.string.setup_linked)
    }
}

/** SharedPreferences for the setup screens' one-time UI memory, plus process memory for
 * requests: a dismissed request shows again once after Pneu restarts. */
internal class SetupUiStore private constructor(context:Context) {
    private val prefs=context.getSharedPreferences("setup_ui",Context.MODE_PRIVATE)
    val state=MutableStateFlow(read())
    val dismissedPrompts=MutableStateFlow(emptySet<String>())
    val notifiedPrompts=MutableStateFlow(emptySet<String>())
    val presentedNotices=MutableStateFlow(prefs.getStringSet("presentedNotices",emptySet()).orEmpty().toSet())
    @Synchronized fun presentNotices(ids:Set<String>) {
        val next=(presentedNotices.value+ids).toList().takeLast(128).toSet()
        prefs.edit(commit=true) {putStringSet("presentedNotices",next)}
        presentedNotices.value=next
    }
    private fun read()=SetupUiState(prefs.getBoolean("bannerDismissed",false),prefs.getBoolean("linkStarted",false),
        prefs.getString("joinedPin","").orEmpty(),prefs.getLong("joinedAt",0),prefs.getLong("seenWithdrawal",0),prefs.getStringSet("announcedSeparate",emptySet()).orEmpty().toSet())
    @Synchronized fun update(change:(SetupUiState)->SetupUiState) {
        val next=change(state.value)
        if(next==state.value)return
        prefs.edit {
            putBoolean("bannerDismissed",next.bannerDismissed);putBoolean("linkStarted",next.linkStarted)
            putString("joinedPin",next.joinedPin);putLong("joinedAt",next.joinedAt);putLong("seenWithdrawal",next.seenWithdrawal)
            putStringSet("announcedSeparate",next.announcedSeparate)
        }
        state.value=next
    }
    fun dismissPrompts(keys:Collection<String>)=dismissedPrompts.update {it+keys}
    private fun <T> MutableStateFlow<T>.update(change:(T)->T) {synchronized(this@SetupUiStore) {value=change(value)}}
    companion object {
        @Volatile private var instance:SetupUiStore?=null
        fun get(context:Context)=instance ?: synchronized(this) {instance ?: SetupUiStore(context.applicationContext).also {instance=it}}
    }
}
internal val Context.setupUi get()=SetupUiStore.get(this)
