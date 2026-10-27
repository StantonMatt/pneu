package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.data.Prefs

internal data class TilePresentation(val available:Boolean,val active:Boolean=false,val subtitle:Int?=null,val recovery:Boolean=false)
internal data class TileInputs(val loaded:Boolean,val peerId:String?,val devices:Set<String>,val paused:Boolean,val quickSettings:Boolean,val pairingLost:Boolean,
    val status:ConnectionStatus,val route:String,val connectedCount:Int,val clipboardAvailable:Boolean)

internal object TilePolicy {
    // Keep progress, timestamps and unrelated settings out of refresh equality.
    // Count only current pairings: the connection map can briefly retain removed devices.
    fun inputs(p:Prefs,s:LinkState,devices:Map<String,LinkState>,clipboardAvailable:Boolean)=TileInputs(
        p.loaded,p.peer?.deviceId,p.devices.map {it.deviceId}.toSet(),p.paused,p.quickSettings,p.pairingLost,s.connectionStatus,s.route,
        p.devices.count {devices[it.deviceId]?.connectionStatus==ConnectionStatus.CONNECTED},clipboardAvailable)
    fun present(p:Prefs,status:String,route:String,clipboard:Boolean=false,clipboardAvailable:Boolean=status==ConnectionStatus.CONNECTED.key)=
        present(p,ConnectionStatus.fromKey(status),route,clipboard,clipboardAvailable)
    fun present(p:Prefs,status:ConnectionStatus,route:String,clipboard:Boolean=false,clipboardAvailable:Boolean=status==ConnectionStatus.CONNECTED)=
        present(inputs(p,LinkState(status.key,route),emptyMap(),clipboardAvailable),clipboard)
    fun present(p:TileInputs,clipboard:Boolean=false):TilePresentation = when {
        !p.loaded || !p.quickSettings -> TilePresentation(false)
        p.pairingLost -> TilePresentation(true,subtitle=R.string.status_pairing_lost,recovery=true)
        p.devices.isEmpty() -> TilePresentation(false,subtitle=R.string.not_paired)
        clipboard -> TilePresentation(p.clipboardAvailable,subtitle=if(p.status==ConnectionStatus.CONNECTED)null else R.string.tile_offline)
        p.paused -> TilePresentation(true,subtitle=R.string.status_paused)
        else -> TilePresentation(true,true,when(p.status) {ConnectionStatus.CONNECTED->when(p.route){"Wi-Fi"->R.string.tile_wifi;"Tailscale"->R.string.tile_tailscale;else->R.string.tile_mobile};ConnectionStatus.CONNECTING->R.string.status_connecting;ConnectionStatus.UPDATE_REQUIRED->R.string.device_update_required;ConnectionStatus.FINGERPRINT_MISMATCH->R.string.status_fingerprint_mismatch;else->R.string.tile_offline})
    }
}
