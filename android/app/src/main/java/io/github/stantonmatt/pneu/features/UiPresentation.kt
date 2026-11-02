package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.connection.ConnectionStatus
import io.github.stantonmatt.pneu.connection.PairingIssue
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.ReinstallReplacement
import io.github.stantonmatt.pneu.protocol.Endpoint

object UiPresentation {
    /** Desktop's decimal units, chosen once from the total for both numbers. */
    fun sizePair(sent:Long,total:Long):String {
        val size=total.coerceAtLeast(0)
        val (unit,scale)=when {size<1_000->"B" to 1.0;size<1_000_000->"KB" to 1e3;size<1_000_000_000->"MB" to 1e6;else->"GB" to 1e9}
        fun number(bytes:Long):String {
            val value=bytes/scale
            val digits=if(value<10 && scale>1 && (unit=="GB" || value%1>=.05))1 else 0
            return String.format(java.util.Locale.ROOT,"%.${digits}f",value)
        }
        return "${number(sent.coerceIn(0,size))} of ${number(size)} $unit"
    }
    fun sendingProgress(context:android.content.Context,sent:Long,total:Long)=context.getString(R.string.item_sending,sizePair(sent,total))
    fun deviceTransfer(context:android.content.Context,link:io.github.stantonmatt.pneu.connection.LinkState):String? =
        sending(context,link)?.let {"$it · ${sizePair(link.sent,link.total)}"}

    fun clipboardReceiveLabel(context:android.content.Context,p:io.github.stantonmatt.pneu.data.Prefs,peer:io.github.stantonmatt.pneu.data.Peer)=
        if(p.devices.size==1)context.getString(R.string.pref_clipboard_from_pc) else context.getString(R.string.pref_clipboard_from_device,peer.name)
    fun connectionDetail(context:android.content.Context,p:io.github.stantonmatt.pneu.data.Prefs,
                         links:Map<String,io.github.stantonmatt.pneu.connection.LinkState>,
                         aggregate:io.github.stantonmatt.pneu.connection.LinkState,computers:Boolean=false):String {
        val connected=p.devices.filter {links[it.deviceId]?.connectionStatus==ConnectionStatus.CONNECTED && !it.pairingLost && it.deviceId !in p.resetPeers}
        if(p.devices.size>1) {
            if(aggregate.connectionStatus!=ConnectionStatus.CONNECTED)return context.getString(if(computers)R.string.detail_computers else R.string.detail_devices,p.devices.size)
            if(connected.size<p.devices.size)return context.getString(if(computers)R.string.detail_connected_computers else R.string.detail_connected_devices,connected.size,p.devices.size)
            if(connected.size==2)return context.getString(R.string.detail_two_devices,connected[0].name,connected[1].name)
            return context.getString(if(computers)R.string.detail_computers else R.string.detail_devices,connected.size)
        }
        val peer=connected.firstOrNull() ?: p.devices.firstOrNull()
        return if(aggregate.connectionStatus==ConnectionStatus.CONNECTED)context.getString(R.string.detail_route,peer?.name.orEmpty(),status(context,aggregate.route)) else peer?.name.orEmpty()
    }

    fun sendingTo(context:android.content.Context,peers:List<io.github.stantonmatt.pneu.data.Peer>,items:Int=1):String =
        if(items>1)context.resources.getQuantityString(R.plurals.toast_sending_items_computers,peers.size,items,peers.size)
        else if(peers.size==1)context.getString(R.string.toast_sending_to,peers.single().name)
        else context.resources.getQuantityString(R.plurals.toast_sending_computers,peers.size,peers.size)
    /** A row with several recipients reports the worst one. Recipient failures stay
     * visible while another device is still sending; the combined bar continues. */
    fun recentOutcome(context:android.content.Context,item:RecentItem):String =when {
        item.failed.size==1 && item.failed.single().error==ReinstallReplacement.CANCELLED -> context.getString(R.string.replacement_not_sent)
        item.failed.size==1 -> item.failed.single().let {failed->
            val name=failed.name.ifEmpty {context.getString(R.string.computer)}
            if(failed.error=="connection_lost")return@let context.getString(R.string.item_failed_on_reason,name,
                connectionLost(context,failed.resumable,failed.sent,failed.total))
            failureReason(failed.error)?.let {context.getString(R.string.item_failed_on_reason,name,context.getString(it))} ?: context.getString(R.string.item_failed_on,name)
        }
        item.failed.isNotEmpty() -> context.resources.getQuantityString(R.plurals.item_failed_computers,item.failed.size,item.failed.size)
        item.sending -> context.getString(R.string.item_sending_devices,item.recipients.size,sizePair(item.sent,item.total))
        item.recipients.any {it.preparing} -> context.getString(R.string.item_preparing)
        item.sentCount==0 -> context.getString(R.string.item_waiting)
        item.waiting.size==1 -> context.getString(R.string.item_waiting_for,item.waiting.single().name.ifEmpty {context.getString(R.string.computer)})
        else -> context.resources.getQuantityString(R.plurals.item_waiting_for_devices,item.waiting.size,item.waiting.size)
    }
    /** Sent rows name their recipients only when more than one device is paired. */
    fun recentSent(context:android.content.Context,item:RecentItem?,clipboard:Boolean,time:String):String? {
        val names=item?.recipients?.filter {it.state=="sent"}?.map {it.name}.orEmpty()
        if(names.isEmpty())return null
        return when {
            names.size==1 -> context.getString(if(clipboard)R.string.recent_clipboard_to_device else R.string.recent_to_device,names.single(),time)
            else -> context.getString(if(clipboard)R.string.recent_clipboard_to_devices else R.string.recent_to_devices,names.size,time)
        }
    }
    fun failureReason(error:String):Int? = when(error) {
        "disk_full" -> R.string.reason_disk
        "not_writable" -> R.string.reason_write
        "unsupported_kind" -> R.string.reason_unsupported
        "image_decode" -> R.string.reason_image
        "local_file_missing" -> R.string.reason_gone
        else -> null
    }
    fun status(context:android.content.Context,value:ConnectionStatus):String=context.getString(when(value) {
        ConnectionStatus.CONNECTED->R.string.status_connected
        ConnectionStatus.CONNECTING->R.string.status_connecting
        ConnectionStatus.UNREACHABLE->R.string.status_unreachable
        ConnectionStatus.UPDATE_REQUIRED->R.string.device_update_required
        ConnectionStatus.PAIRING_LOST->R.string.status_pairing_lost
        ConnectionStatus.FINGERPRINT_MISMATCH->R.string.status_fingerprint_mismatch
        ConnectionStatus.WAITING_WIFI->R.string.status_waiting_wifi
        ConnectionStatus.NO_NETWORK->R.string.status_no_network
        ConnectionStatus.PAUSED->R.string.status_paused
        ConnectionStatus.LOCAL_NETWORK_OFF->R.string.status_local_network_off
        ConnectionStatus.STORAGE_UNAVAILABLE->R.string.load_failed
        ConnectionStatus.LOADING->R.string.load_pending
    })
    fun status(context:android.content.Context,value:String):String=when(value) {
        "Wi-Fi"->context.getString(R.string.tile_wifi)
        "Tailscale"->context.getString(R.string.tile_tailscale)
        "Mobile data"->context.getString(R.string.tile_mobile)
        else->value
    }
    fun pairing(context:android.content.Context,value:UiFeedback):String=
        value.status?.let {status(context,it)} ?: when(value.issue) {PairingIssue.CLOCK->context.getString(R.string.pair_clock_ahead);PairingIssue.EXPIRED->context.getString(R.string.pair_expired);else->value.message}
    fun recentReceived(context:android.content.Context,item:RecentItem,time:String,pairedCount:Int):String?=
        item.received?.let {if(pairedCount>1)context.getString(R.string.recent_from_computer,it.senderName,time) else time}

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

    fun connectionLostFailure(context:android.content.Context,state:String,error:String,resumable:Boolean,sent:Long,total:Long):String? =
        if(state=="failed" && error=="connection_lost")connectionLost(context,resumable,sent,total) else null

    fun connectionLost(context:android.content.Context,resumable:Boolean,sent:Long,total:Long):String =
        if(resumable && sent>0)context.getString(R.string.item_connection_lost_progress,
            sizePair(sent,total))
        else context.getString(R.string.item_connection_lost)

    fun failedItem(error:String):Int = when(error) {
        ReinstallReplacement.CANCELLED -> R.string.replacement_not_sent
        "disk_full" -> R.string.item_failed_disk
        "not_writable" -> R.string.item_failed_write
        "unsupported_kind" -> R.string.item_failed_unsupported
        "image_decode" -> R.string.item_failed_image
        "local_file_missing" -> R.string.item_failed_gone
        else -> R.string.item_failed
    }

    fun pairedAddress(addresses:List<Endpoint>,route:String):String? = addresses.firstOrNull {
        it.tailscale==(route=="Tailscale") && ':' !in it.host && Endpoint.literalAddress(it.host)?.address?.size==4
    }?.host

    /** One LAN and one Tailscale IPv4 literal; names and IPv6 are not useful here. */
    fun pairedAddresses(addresses:List<Endpoint>):List<String> {
        val ipv4=addresses.filter {':' !in it.host && Endpoint.literalAddress(it.host)?.address?.size==4}
        return listOfNotNull(ipv4.firstOrNull {!it.tailscale}?.host,ipv4.firstOrNull {it.tailscale}?.host)
    }
}
