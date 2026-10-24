package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*

internal object SettingsDeliveryPolicy {
    fun rejection(row:Outgoing,peer:Peer,capabilities:Set<String>) =
        if("mesh" in capabilities && (row.automatic || row.kind=="clipboard_image"))
            "settings:"+PairSettings.signature(peer) else capabilities.sorted().joinToString(",")
    fun releasable(row:Outgoing,peer:Peer,capabilities:Set<String>)=row.error=="unsupported_kind" &&
        (row.automatic || row.kind=="clipboard_image") &&
        (!row.rejectedCapabilities.startsWith("settings:") || row.rejectedCapabilities!="settings:"+PairSettings.signature(peer)) &&
        (!row.automatic || peer.automaticMedia) && (row.kind!="clipboard_image" || peer.clipboardSend) &&
        "files" in capabilities && (row.kind!="clipboard_image" || "clipboard_image" in capabilities)
}
