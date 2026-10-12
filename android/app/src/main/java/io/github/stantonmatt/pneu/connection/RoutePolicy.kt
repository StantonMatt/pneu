package io.github.stantonmatt.pneu.connection

import android.net.ConnectivityManager
import android.net.NetworkCapabilities

/** Absence is an offline route, not a costly route waiting for Wi-Fi. */
enum class Connectivity {
    NONE, METERED, UNMETERED;
    val available get() = this != NONE
    val mobile get() = this == METERED
    fun waitingForWifi(mobileAllowed:Boolean) = mobile && !mobileAllowed
    companion object {
        fun from(notMetered:Boolean?):Connectivity = when(notMetered) {
            null -> NONE
            false -> METERED
            true -> UNMETERED
        }
    }
}

/** Cost comes from the active network, including the underlying cost inherited
 * by a VPN. Transport flags do not describe whether data is metered. */
internal object RoutePolicy {
    fun label(tailscale:Boolean,mobile:Boolean) =
        if(tailscale) "Tailscale" else if(mobile) "Mobile data" else "Wi-Fi"

    data class Active(val connectivity:Connectivity,val vpn:Boolean) {
        val available get()=connectivity.available
        val mobile get()=connectivity.mobile
        fun label(tailscale:Boolean)=if(!available) "No network" else RoutePolicy.label(tailscale,mobile)
    }
    fun active(manager:ConnectivityManager):Active =
        capabilities(manager.activeNetwork?.let(manager::getNetworkCapabilities))

    fun capabilities(caps:NetworkCapabilities?):Active = Active(
        connectivity=Connectivity.from(caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)),
        // Presence is only used for the UI's Open Tailscale hint, never for cost.
        vpn=caps?.hasTransport(NetworkCapabilities.TRANSPORT_VPN)==true
    )
}
