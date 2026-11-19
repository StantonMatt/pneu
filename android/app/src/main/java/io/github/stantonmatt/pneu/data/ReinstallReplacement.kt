package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.Gson
import android.icu.lang.UCharacter

/** Local consent only. Neither these identities nor decisions travel on the wire. */
data class ReplacementChoice(val old:ConfirmedBinding,val fresh:ConfirmedBinding,val role:String,val name:String,val lastConnected:Long,val implied:Boolean=false) {
    val key get()="${old.phoneId}/${old.pcId}/${old.pin}/${old.revision}/${fresh.pcId}/${fresh.pin}/${fresh.revision}"
    fun matches(p:Prefs,connected:(String)->Boolean):Boolean {
        val a=p.devices.find {it.deviceId==old.pcId} ?: return false
        val b=p.devices.find {it.deviceId==fresh.pcId} ?: return false
        return old.matches(p) && fresh.matches(p) && a.deviceId!=b.deviceId && a.pin!=b.pin &&
            a.role==role && b.role==role && ReinstallReplacement.name(a.name)==ReinstallReplacement.name(name) &&
            ReinstallReplacement.name(b.name)==ReinstallReplacement.name(name) && !connected(a.deviceId)
    }
}
data class ReplacementState(val offers:List<ReplacementChoice> = emptyList(),val declined:Set<String> = emptySet(),
    val replaced:List<ReplacementChoice> = emptyList(),val notificationNotice:ReplacementChoice?=null,val failed:String?=null) {
    fun replacement(id:String,generation:Long)=replaced.lastOrNull {it.old.pcId==id && it.old.revision==generation}
}
internal object ReinstallReplacement {
    const val CANCELLED="old_device_replaced"
    private val key=stringPreferencesKey("reinstallReplacement")
    private val gson=Gson()
    fun name(value:String)=UCharacter.foldCase(value.trim {it.isWhitespace() || Character.isSpaceChar(it)},true)
    fun read(p:androidx.datastore.preferences.core.Preferences):ReplacementState=p[key]?.let {gson.fromJson(it,ReplacementState::class.java)} ?: ReplacementState()
    private fun write(p:MutablePreferences,state:ReplacementState) {p[key]=gson.toJson(state)}
    private fun prefs(p:androidx.datastore.preferences.core.Preferences)=Prefs(deviceId=p[stringPreferencesKey("deviceId")].orEmpty(),peers=PeerRegistry.read(p),replacements=read(p))
    /** Called only by a committed fresh token pairing, after authenticated HELLO. */
    fun paired(p:MutablePreferences,binding:DeliveryBinding,pairAgain:ConfirmedBinding?,connected:(String)->Boolean) {
        val current=prefs(p);val fresh=current.devices.single {it.deviceId==binding.pcId}
        val candidates=current.devices.filter {it.deviceId!=fresh.deviceId && it.pin!=fresh.pin && it.role==fresh.role && name(it.name)==name(fresh.name) && !connected(it.deviceId)}
        val old=if(pairAgain!=null)candidates.find {it.deviceId==pairAgain.pcId && pairAgain.matches(current)}
            else candidates.sortedWith(compareByDescending<Peer> {it.lastConnected}.thenBy {it.deviceId}).firstOrNull()
        old ?: return
        val choice=ReplacementChoice(ConfirmedBinding.from(current,old.deviceId),ConfirmedBinding.from(current,fresh.deviceId),fresh.role,fresh.name,old.lastConnected,implied=pairAgain!=null)
        if(choice.key in read(p).declined)return
        write(p,read(p).let {it.copy(offers=it.offers.filterNot {offer->offer.fresh==choice.fresh}+choice)})
    }
    fun decline(p:MutablePreferences,choice:ReplacementChoice) {
        val state=read(p)
        write(p,state.copy(offers=state.offers.filterNot {it.key==choice.key},declined=state.declined+choice.key))
    }
    /** Removal, fresh local switch edits and the reset recovery journal are atomic. */
    fun replace(p:MutablePreferences,choice:ReplacementChoice,connected:(String)->Boolean) {
        val consent=read(p)
        check(consent.offers.any {it.key==choice.key} && choice.key !in consent.declined && choice.matches(prefs(p),connected)) {"Replacement changed"}
        val peers=PeerRegistry.read(p);val old=peers.single {it.deviceId==choice.old.pcId}
        val fresh=peers.single {it.deviceId==choice.fresh.pcId}
        // Offer `Turn on` only when the old entry was actually mirroring. Read this
        // before removal, which drops the old entry's grant.
        val mirrored=p[booleanPreferencesKey("mirrorEnabled")]==true &&
            io.github.stantonmatt.pneu.features.NotificationConsent.computer(old) in p[stringSetPreferencesKey("mirrorComputers")].orEmpty()
        val setup=SetupStorage.read(p)
        val member=setup.roster.find {it.active && it.deviceId==old.deviceId && it.fingerprint==old.pin}
        if(member!=null)SetupTransactions.remove(p,member.key,setup.generation)
        else {PeerRegistry.remove(p,old.deviceId);PeerRegistry.reset(p,old.deviceId);SetupStorage.write(p,setup)}
        val phone=choice.old.phoneId
        var updated=PairSettings.initialize(fresh,phone)
        val values=listOf(old.clipboardSend,old.clipboardReceive,old.automaticMedia)
        PairSettings.keys(phone,fresh.deviceId).forEachIndexed {index,key->
            val stamp=MeshClock.next(p,phone,false)
            updated=updated.copy(settings=updated.settings+(key to PairField(values[index],stamp.hlc.toString(),stamp.source)),localEdits=updated.localSettingsEdits+key)
        }
        updated=PairSettings.initialize(updated,phone)
        // Keep the replacement in the old entry's list position.
        PeerRegistry.write(p,peers.mapNotNull {when(it.deviceId) {old.deviceId->updated;fresh.deviceId->null;else->it}})
        p.remove(longPreferencesKey("legacyImageTime:${old.deviceId}"));p.remove(longPreferencesKey("legacyImageGeneration:${old.deviceId}"))
        val state=read(p)
        write(p,state.copy(offers=state.offers.filterNot {it.key==choice.key},replaced=state.replaced+choice,notificationNotice=if(mirrored)choice else state.notificationNotice))
    }
    fun fail(p:MutablePreferences,choice:ReplacementChoice) {
        decline(p,choice)
        write(p,read(p).copy(failed=choice.key))
    }
    fun finishFailure(p:MutablePreferences) {write(p,read(p).copy(failed=null))}
    fun finishNotice(p:MutablePreferences,key:String) {
        val state=read(p)
        if(state.notificationNotice?.key==key)write(p,state.copy(notificationNotice=null))
    }
}
internal suspend fun Preferences.declineReplacement(choice:ReplacementChoice)=policy.locked {edit {ReinstallReplacement.decline(it,choice)}}
internal suspend fun Preferences.replaceReinstalled(choice:ReplacementChoice,connected:(String)->Boolean)=policy.locked {
    edit {ReinstallReplacement.replace(it,choice,connected);beforeMutation("replacementStaged")}
}
internal suspend fun Preferences.finishReplacementNotice(key:String)=edit {ReinstallReplacement.finishNotice(it,key)}
