package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.Prefs

internal data class TilePresentation(val available:Boolean,val active:Boolean=false,val subtitle:String?=null,val recovery:Boolean=false)

internal object TilePolicy {
    fun present(p:Prefs,status:String,route:String,clipboard:Boolean=false):TilePresentation = when {
        !p.loaded || !p.quickSettings -> TilePresentation(false)
        p.pairingLost -> TilePresentation(true,subtitle="Pairing lost",recovery=true)
        p.peer==null -> TilePresentation(false,subtitle="Not paired")
        clipboard -> TilePresentation(true,subtitle=if(status=="Connected")null else "Offline")
        p.paused -> TilePresentation(true,subtitle="Paused")
        else -> TilePresentation(true,true,when(status) {"Connected"->route;"Connecting…"->status;else->"Offline"})
    }
}
