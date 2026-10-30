package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.connection.ConnectionStatus
import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.utf8Prefix
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

data class DeviceState(val peer:Peer,val connection:LinkState,val pendingSwitches:Set<String>,val settingsSupported:Boolean?) {
    val setupScope get()=peer.setupScope
    val addedVia get()=peer.addedViaName
    val settingsPending get()=pendingSwitches.isNotEmpty() && settingsSupported!=false
    val lastSeen get()=peer.lastConnected
}
/** Connected devices first, then by last seen. No manual reordering. */
internal object DeviceProjection {
    fun states(prefs:Prefs,links:Map<String,LinkState>)=prefs.devices.map {peer->
        DeviceState(peer,links[peer.deviceId] ?: LinkState(),
            PairSettings.keys(prefs.deviceId,peer.deviceId).zip(listOf("sendClipboard","receiveClipboard","automaticMedia"))
                .filter {it.first in peer.localSettingsEdits}.map {it.second}.toSet(),peer.settingsSupported)
    }.sortedWith(compareByDescending<DeviceState> {it.connection.connectionStatus==ConnectionStatus.CONNECTED}.thenByDescending {it.lastSeen})
}
/** State/actions for Home's Devices card and the device pages. Actions suspend so the
 * caller can use the existing UI action-error presentation and confirmation patterns. */
class DevicesViewModel(application:Application):AndroidViewModel(application) {
    private val app=application.graph
    val devices=combine(app.prefs,app.connection.devices,DeviceProjection::states)
        .stateIn(viewModelScope,SharingStarted.WhileSubscribed(5000),emptyList())
    val setup=app.prefs.map {p->p.membership.copy(notices=p.membership.notices.filter {it.peerId !in p.resetPeers})}.stateIn(viewModelScope,SharingStarted.WhileSubscribed(5000),SetupState())
    val setupProblem=app.connection.setupProblem.asStateFlow()
    val pairing=app.connection.attempt
    suspend fun linkExistingDevice(confirmed:ConfirmedBinding) {app.linkExistingDevice(confirmed);PneuService.start(app.context)}
    suspend fun allowSetup(prompt:SetupPrompt) {app.answerSetupPrompt(prompt,true);PneuService.start(app.context)}
    suspend fun denySetup(prompt:SetupPrompt)=app.answerSetupPrompt(prompt,false)
    fun confirmRemoveFromAllDevices(key:String)=ConfirmedSetupMember.from(app.prefs.value,key)
    suspend fun removeFromAllDevices(confirmed:ConfirmedSetupMember)=app.removeSetupMember(confirmed)
    suspend fun dismissJoinedNotice(id:String)=app.withBindingLifecycle {app.preferences.edit {p->
        val state=SetupStorage.read(p);SetupStorage.write(p,state.copy(notices=state.notices.filterNot {it.id==id}))
    }}
    val chooseDevicesEachTime=app.prefs.map {it.chooseDevicesEachTime}
    suspend fun setChooseDevicesEachTime(value:Boolean)=app.preferences.toggle("chooseDevicesEachTime",value)
    suspend fun setPairSwitch(authority:CapturedAuthority,key:String,value:Boolean) {check(app.setPairSwitch(authority,key,value)) {"Pairing changed during setting edit"}}
    suspend fun addDevice(invitation:String,alsoLinkDevices:Boolean=true):Boolean=withContext(Dispatchers.IO) {
        val captured=app.prefs.value
        check(captured.loaded) {"Settings not loaded"}
        val expected=ConfirmedBinding.forInvitation(captured,io.github.stantonmatt.pneu.protocol.Invitation.parse(invitation))
        val accepted=app.requestPairing(invitation,expected,setupEnabled=alsoLinkDevices)
        if(accepted)PneuService.start(app.context)
        accepted
    }
    suspend fun unpairOne(confirmed:ConfirmedBinding)=app.connection.unpair(confirmed)
    suspend fun renameSelf(name:String)=withContext(Dispatchers.IO) {
        val value=name.trim();require(value.isNotEmpty() && utf8Prefix(value,128)==value && '\u0000' !in value)
        app.withBindingLifecycle {app.preferences.setString("selfName",value)}
    }
    fun targets(kind:String,selected:Set<String>?=null)=app.recipients(app.prefs.value,kind,selected=selected)
}
