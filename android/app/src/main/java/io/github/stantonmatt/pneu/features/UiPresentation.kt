package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.protocol.Endpoint

object UiPresentation {
    fun failedItem(error:String):String = when(error) {
        "disk_full" -> "Failed · PC disk is full"
        "not_writable" -> "Failed · PC can't save files"
        "unsupported_kind" -> "Failed · PC can't receive this"
        "image_decode" -> "Failed · Couldn't read image"
        "local_file_missing" -> "Failed · File is gone"
        else -> "Failed"
    }

    /** One LAN and one Tailscale IPv4 literal; names and IPv6 are not useful here. */
    fun pairedAddresses(addresses:List<Endpoint>):List<String> {
        val ipv4=addresses.filter {':' !in it.host && Endpoint.literalAddress(it.host)?.address?.size==4}
        return listOfNotNull(ipv4.firstOrNull {!it.tailscale}?.host,ipv4.firstOrNull {it.tailscale}?.host)
    }
}
