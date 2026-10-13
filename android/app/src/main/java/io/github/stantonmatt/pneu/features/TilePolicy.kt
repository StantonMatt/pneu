package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.Prefs

internal data class TilePresentation(val available:Boolean,val active:Boolean=false,val subtitle:Int?=null,val recovery:Boolean=false)

internal object TilePolicy {
    fun present(p:Prefs,status:String,route:String,clipboard:Boolean=false):TilePresentation = when {
        !p.loaded || !p.quickSettings -> TilePresentation(false)
        p.pairingLost -> TilePresentation(true,subtitle=R.string.status_pairing_lost,recovery=true)
        p.peer==null -> TilePresentation(false,subtitle=R.string.not_paired)
        clipboard -> TilePresentation(true,subtitle=if(status=="Connected")null else R.string.tile_offline)
        p.paused -> TilePresentation(true,subtitle=R.string.status_paused)
        else -> TilePresentation(true,true,when(status) {"Connected"->when(route){"Wi-Fi"->R.string.tile_wifi;"Tailscale"->R.string.tile_tailscale;else->R.string.tile_mobile};"Connecting…"->R.string.status_connecting;else->R.string.tile_offline})
    }
}
