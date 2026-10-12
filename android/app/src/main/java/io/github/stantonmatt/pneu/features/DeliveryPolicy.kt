package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.connection.Connectivity

/** Pure admission/transport rules and last-writer-wins text ordering. */
object DeliveryPolicy {
    fun skipAutomatic(paused: Boolean, enabled: Boolean, queue: Boolean, connected: Boolean, connectivity: Connectivity, mobile: Boolean) =
        paused || !enabled || (!queue && (!connected || !connectivity.available) && !connectivity.waitingForWifi(mobile))
    fun hold(kind: String, automatic: Boolean, paused: Boolean, cellular: Boolean, originals: Boolean, attempted: Boolean, optimized: Boolean): Boolean =
        (automatic && paused) || (cellular && !originals && (kind == "recording" || (kind in setOf("screenshot","photo","clipboard_image") && attempted && !optimized)))
    fun optimize(kind: String, mime: String, cellular: Boolean, originals: Boolean, attempted: Boolean, optimized: Boolean=false) =
        cellular && !originals && !attempted && !optimized && kind != "file" && mime.startsWith("image/")
    fun transport(tailscaleRoute:Boolean,wifi:Boolean,tailscale:Boolean,connectivity:Connectivity,mobile:Boolean)=
        connectivity.available && !connectivity.waitingForWifi(mobile) && if(tailscaleRoute)tailscale else wifi
    fun ownedEcho(owned:Boolean,hash:String,received:String)=owned && hash.isNotEmpty() && hash==received
    fun newer(timestamp:Long,lastApplied:Long)=timestamp>lastApplied
}
