package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.protocol.Endpoint

object UiPresentation {
    fun clipboardReceiveLabel(context:android.content.Context,p:io.github.stantonmatt.pneu.data.Prefs,peer:io.github.stantonmatt.pneu.data.Peer)=
        if(p.devices.size==1)context.getString(R.string.pref_clipboard_from_pc) else context.getString(R.string.pref_clipboard_from_device,peer.name)
    fun connectionDetail(context:android.content.Context,p:io.github.stantonmatt.pneu.data.Prefs,
                         links:Map<String,io.github.stantonmatt.pneu.connection.LinkState>,
                         aggregate:io.github.stantonmatt.pneu.connection.LinkState):String {
        val connected=p.devices.filter {links[it.deviceId]?.status=="Connected" && !it.pairingLost && it.deviceId !in p.resetPeers}
        if(p.devices.size>1) {
            if(aggregate.status!="Connected")return context.getString(R.string.detail_devices,p.devices.size)
            if(connected.size<p.devices.size)return context.getString(R.string.detail_connected_devices,connected.size,p.devices.size)
            if(connected.size==2)return context.getString(R.string.detail_two_devices,connected[0].name,connected[1].name)
            return context.getString(R.string.detail_devices,connected.size)
        }
        val peer=connected.firstOrNull() ?: p.devices.firstOrNull()
        return if(aggregate.status=="Connected")context.getString(R.string.detail_route,peer?.name.orEmpty(),status(context,aggregate.route)) else peer?.name.orEmpty()
    }

    fun sendingTo(context:android.content.Context,peers:List<io.github.stantonmatt.pneu.data.Peer>):String =
        if(peers.size==1)context.getString(R.string.toast_sending_to,peers.single().name)
        else context.resources.getQuantityString(R.plurals.toast_sending_computers,peers.size,peers.size)
    fun recentOutcome(context:android.content.Context,item:RecentItem):String =when {
        // Keep a recipient failure visible while another computer is still
        // sending. The row's combined progress bar continues independently.
        item.failed.size==1 -> context.getString(R.string.item_failed_on,item.failed.single().name.ifEmpty {context.getString(R.string.computer)})
        item.failed.isNotEmpty() -> context.resources.getQuantityString(R.plurals.item_failed_computers,item.failed.size,item.failed.size)
        item.sending -> context.getString(R.string.item_sending,
            android.text.format.Formatter.formatShortFileSize(context,item.sent),
            android.text.format.Formatter.formatShortFileSize(context,item.total))
        item.sentCount==0 -> context.getString(R.string.item_waiting)
        else -> context.getString(R.string.item_sent_to,item.sentCount,item.recipients.size)
    }
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
