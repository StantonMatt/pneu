package io.github.stantonmatt.pneu

import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*

/** Integration seam for the phone-owned notification consent UI. The notifications
 * feature installs its handler here; until then the action grants no access. */
fun interface ReplacementNotificationAction {
    suspend fun turnOnForNewComputer(binding:DeliveryBinding)
}

suspend fun AppGraph.replaceReinstalledDevice(choice:ReplacementChoice)=withContext(Dispatchers.IO+NonCancellable) {withBindingLifecycle {
    val old=preferences.read().devices.find {it.deviceId==choice.old.pcId}
    preferences.replaceReinstalled(choice,connection::connected)
    old?.let(::changed)
    connection.bindingChanged(peerId=choice.old.pcId)
    // The reset journal survives failure/death between DataStore and Room.
    completePeerReset(requireNotNull(choice.old.pcId))
}}

/** A fresh pairing is durable before this intent runs. A failed replacement must
 * never roll back pairing or leave a token handshake waiting for a UI choice. */
internal suspend fun AppGraph.finishImpliedReplacementsLocked() {
    for(choice in preferences.read().replacements.offers.filter {it.implied}) {
        try {
            val old=preferences.read().devices.find {it.deviceId==choice.old.pcId}
            preferences.replaceReinstalled(choice,connection::connected)
            old?.let(::changed)
            connection.bindingChanged(peerId=choice.old.pcId)
        }catch(e:CancellationException) {throw e}
        catch(e:Exception) {preferences.edit {ReinstallReplacement.fail(it,choice)}}
    }
}
