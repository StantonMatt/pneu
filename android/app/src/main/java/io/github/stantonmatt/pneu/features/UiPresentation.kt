package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.protocol.Endpoint

object UiPresentation {
    fun status(context:android.content.Context,value:String):String {return context.getString(when(value) {
        "Connected"->R.string.status_connected;"Connecting…"->R.string.status_connecting
        "PC unreachable"->R.string.status_unreachable;"Waiting for Wi-Fi"->R.string.status_waiting_wifi
        "No network"->R.string.status_no_network;"Paused"->R.string.status_paused
        "Pairing lost"->R.string.status_pairing_lost;"Wi-Fi"->R.string.tile_wifi
        "Tailscale"->R.string.tile_tailscale;"Mobile data"->R.string.tile_mobile
        else->return value
    })}
    fun pairing(context:android.content.Context,value:String):String {return context.getString(when {
        value==context.getString(R.string.pair_not_pneu)->R.string.pair_not_pneu
        value==context.getString(R.string.pair_expired)->R.string.pair_expired
        value.startsWith("Your PC's clock")->R.string.pair_clock_ahead
        else->return value
    })}
    fun sending(context:android.content.Context,state:io.github.stantonmatt.pneu.connection.LinkState):String? {
        if(state.transferMediaId.isEmpty())return null
        return when(state.transferKind) {
            "photo"->context.getString(R.string.notif_sending_photo)
            "recording"->context.getString(R.string.notif_sending_recording)
            "file"->context.getString(R.string.notif_sending_file,state.transferFileName.ifEmpty {context.getString(R.string.kind_file)})
            "clipboard_image"->context.getString(R.string.notif_sending_clipboard_image)
            else->context.getString(R.string.notif_sending_screenshot)
        }
    }

    fun failedItem(error:String):Int = when(error) {
        "disk_full" -> R.string.item_failed_disk
        "not_writable" -> R.string.item_failed_write
        "unsupported_kind" -> R.string.item_failed_unsupported
        "image_decode" -> R.string.item_failed_image
        "local_file_missing" -> R.string.item_failed_gone
        else -> R.string.item_failed
    }

    /** One LAN and one Tailscale IPv4 literal; names and IPv6 are not useful here. */
    fun pairedAddresses(addresses:List<Endpoint>):List<String> {
        val ipv4=addresses.filter {':' !in it.host && Endpoint.literalAddress(it.host)?.address?.size==4}
        return listOfNotNull(ipv4.firstOrNull {!it.tailscale}?.host,ipv4.firstOrNull {it.tailscale}?.host)
    }
}
