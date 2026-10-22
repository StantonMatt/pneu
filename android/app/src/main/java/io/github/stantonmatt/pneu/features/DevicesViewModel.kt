package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.utf8Prefix
import kotlinx.coroutines.flow.*

data class DeviceState(val peer:Peer,val connection:LinkState,val pendingSwitches:Set<String>,val settingsSupported:Boolean?) {
    val settingsPending get()=pendingSwitches.isNotEmpty() && settingsSupported!=false
    val lastSeen get()=peer.lastConnected
}
/** State/actions for the upcoming device screens. Actions suspend so the caller
 * can use the existing UI action-error presentation and confirmation patterns. */
class DevicesViewModel(application:Application):AndroidViewModel(application) {
    private val app=application.graph
    val devices=combine(app.prefs,app.connection.devices) {prefs,links->prefs.devices.map {peer->
        DeviceState(peer,links[peer.deviceId] ?: LinkState(),
            PairSettings.keys(prefs.deviceId,peer.deviceId).zip(listOf("sendClipboard","receiveClipboard","automaticMedia"))
                .filter {it.first in peer.localSettingsEdits}.map {it.second}.toSet(),peer.settingsSupported)
    }.sortedWith(compareByDescending<DeviceState> {it.connection.status=="Connected"}.thenByDescending {it.lastSeen})}
        .stateIn(viewModelScope,SharingStarted.WhileSubscribed(5000),emptyList())
    val chooseDevicesEachTime=app.prefs.map {false}
    suspend fun setChooseDevicesEachTime(value:Boolean) {
        check(!value) {"Device picker is not available"}
        app.preferences.toggle("chooseDevicesEachTime",false)
    }
    suspend fun setPairSwitch(peerId:String,key:String,value:Boolean)=app.setPairSwitch(peerId,key,value)
    suspend fun addDevice(invitation:String):Boolean {
        val accepted=app.requestPairing(invitation,ConfirmedBinding.forInvitation(app.awaitPrefs(),io.github.stantonmatt.pneu.protocol.Invitation.parse(invitation)))
        if(accepted)PneuService.start(app.context)
        return accepted
    }
    suspend fun unpairOne(confirmed:ConfirmedBinding)=app.connection.unpair(confirmed)
    suspend fun renameSelf(name:String) {
        val value=name.trim();require(value.isNotEmpty() && utf8Prefix(value,128)==value && '\u0000' !in value)
        app.preferences.setString("selfName",value)
    }
    fun targets(kind:String,selected:Set<String>?=null)=app.recipients(app.prefs.value,kind,selected=selected)
}
