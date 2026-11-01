package io.github.stantonmatt.pneu.features

import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import androidx.compose.runtime.*
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner

/** What an unreachable device's card says, from what the phone can tell. */
enum class UnreachableHint { SAME_WIFI, OFF_OR_OTHER_WIFI, TAILSCALE_OFF, LAST_CONNECTED }

/** The Connect anywhere sheet's Tailscale row, named by what's missing on this phone. */
enum class TailscaleRow { GET, TURN_ON, OPEN, ON }

/** wifi: any Wi-Fi or Ethernet network is up, even one Android doesn't use by default.
 * vpn: the default network is a VPN, as RoutePolicy reads it. */
data class PhoneNetwork(val wifi:Boolean,val vpn:Boolean)

object ReachabilityHints {
    const val TAILSCALE="com.tailscale.ipn"
    /** Tailscale counts as set up when it's installed, allowed in Settings → Tailscale when away,
     * and the computer has a Tailscale address. Otherwise the phone's own network explains it. */
    fun unreachable(network:PhoneNetwork,tailscaleInstalled:Boolean,tailscaleAllowed:Boolean,peerHasTailscale:Boolean)=when {
        tailscaleInstalled && tailscaleAllowed && peerHasTailscale -> if(network.vpn)UnreachableHint.LAST_CONNECTED else UnreachableHint.TAILSCALE_OFF
        network.wifi -> UnreachableHint.OFF_OR_OTHER_WIFI
        else -> UnreachableHint.SAME_WIFI
    }
    /** On only with the VPN up, the same signal that keeps the card from saying Tailscale is off. */
    fun tailscaleRow(network:PhoneNetwork,tailscaleInstalled:Boolean,tailscaleAllowed:Boolean)=when {
        !tailscaleInstalled -> TailscaleRow.GET
        !tailscaleAllowed -> TailscaleRow.TURN_ON
        network.vpn -> TailscaleRow.ON
        else -> TailscaleRow.OPEN
    }
    fun tailscaleInstalled(context:Context)=runCatching {context.packageManager.getPackageInfo(TAILSCALE,0)}.isSuccess
    fun current(manager:ConnectivityManager):PhoneNetwork {
        val caps=manager.activeNetwork?.let(manager::getNetworkCapabilities)
        return PhoneNetwork(caps?.local()==true,caps?.hasTransport(NetworkCapabilities.TRANSPORT_VPN)==true)
    }
    private fun NetworkCapabilities.local()=hasTransport(NetworkCapabilities.TRANSPORT_WIFI) || hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)
    /** Play first; a phone without Play gets the web listing. */
    fun openTailscaleListing(context:Context) {
        try {context.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse("market://details?id=$TAILSCALE")))}
        catch(_:android.content.ActivityNotFoundException) {context.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse("https://play.google.com/store/apps/details?id=$TAILSCALE")))}
    }
    fun openTailscale(context:Context)=context.startActivity(requireNotNull(context.packageManager.getLaunchIntentForPackage(TAILSCALE)))
    /** Android's Wi-Fi panel over Pneu, or Wi-Fi settings where there is no panel. */
    fun openWifiSettings(context:Context) {
        try {context.startActivity(Intent(Settings.Panel.ACTION_WIFI))}
        catch(_:android.content.ActivityNotFoundException) {context.startActivity(Intent(Settings.ACTION_WIFI_SETTINGS))}
    }
}

/** Live: joining or leaving Wi-Fi updates the hint without reopening the screen. */
@Composable internal fun rememberPhoneNetwork():PhoneNetwork {
    val context=LocalContext.current
    val manager=remember(context) {context.getSystemService(ConnectivityManager::class.java)}
    var local by remember {mutableStateOf(emptySet<Network>())}
    val initial=remember(manager) {ReachabilityHints.current(manager)}
    var vpn by remember {mutableStateOf(initial.vpn)}
    var reported by remember {mutableStateOf(false)}
    DisposableEffect(manager) {
        val main=Handler(Looper.getMainLooper())
        val lan=object:ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network:Network) {local=local+network;reported=true}
            override fun onLost(network:Network) {local=local-network;reported=true}
        }
        val default=object:ConnectivityManager.NetworkCallback() {
            override fun onCapabilitiesChanged(network:Network,caps:NetworkCapabilities) {vpn=caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)}
            override fun onLost(network:Network) {vpn=false}
        }
        // INTERNET is dropped so a Wi-Fi without internet access still counts.
        val request=NetworkRequest.Builder().removeCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI).addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET).build()
        runCatching {manager.registerNetworkCallback(request,lan,main);manager.registerDefaultNetworkCallback(default,main)}
        onDispose {runCatching {manager.unregisterNetworkCallback(lan)};runCatching {manager.unregisterNetworkCallback(default)}}
    }
    return PhoneNetwork(if(reported)local.isNotEmpty() else initial.wifi,vpn)
}

/** Rechecked on every resume, so installing Tailscale from Play shows on return. */
@Composable internal fun rememberTailscaleInstalled():Boolean {
    val context=LocalContext.current;val lifecycle=LocalLifecycleOwner.current
    var installed by remember {mutableStateOf(ReachabilityHints.tailscaleInstalled(context))}
    DisposableEffect(lifecycle) {
        val observer=LifecycleEventObserver {_,event->if(event==Lifecycle.Event.ON_RESUME)installed=ReachabilityHints.tailscaleInstalled(context)}
        lifecycle.lifecycle.addObserver(observer);onDispose {lifecycle.lifecycle.removeObserver(observer)}
    }
    return installed
}
