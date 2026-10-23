package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.connection.ConnectionStatus
import io.github.stantonmatt.pneu.data.Prefs

internal data class TilePresentation(val available:Boolean,val active:Boolean=false,val subtitle:Int?=null,val recovery:Boolean=false)

internal object TilePolicy {
    fun present(p:Prefs,status:String,route:String,clipboard:Boolean=false,clipboardAvailable:Boolean=status==ConnectionStatus.CONNECTED.key)=
        present(p,ConnectionStatus.fromKey(status),route,clipboard,clipboardAvailable)
    fun present(p:Prefs,status:ConnectionStatus,route:String,clipboard:Boolean=false,clipboardAvailable:Boolean=status==ConnectionStatus.CONNECTED):TilePresentation = when {
        !p.loaded || !p.quickSettings -> TilePresentation(false)
        p.pairingLost -> TilePresentation(true,subtitle=R.string.status_pairing_lost,recovery=true)
        p.devices.isEmpty() -> TilePresentation(false,subtitle=R.string.not_paired)
        clipboard -> TilePresentation(clipboardAvailable,subtitle=if(status==ConnectionStatus.CONNECTED)null else R.string.tile_offline)
        p.paused -> TilePresentation(true,subtitle=R.string.status_paused)
        else -> TilePresentation(true,true,when(status) {ConnectionStatus.CONNECTED->when(route){"Wi-Fi"->R.string.tile_wifi;"Tailscale"->R.string.tile_tailscale;else->R.string.tile_mobile};ConnectionStatus.CONNECTING->R.string.status_connecting;ConnectionStatus.UPDATE_REQUIRED->R.string.device_update_required;ConnectionStatus.FINGERPRINT_MISMATCH->R.string.status_fingerprint_mismatch;else->R.string.tile_offline})
    }
}
