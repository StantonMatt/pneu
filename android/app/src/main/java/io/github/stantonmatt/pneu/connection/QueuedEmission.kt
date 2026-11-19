package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.DeliveryPolicy
import io.github.stantonmatt.pneu.features.isFolder
import java.util.concurrent.atomic.AtomicInteger

/** Cancellation before the first frame is local; an emitted text keeps its receipt. */
internal class QueuedEmission(val row:Outgoing,val binding:DeliveryBinding) {
    private val phase=AtomicInteger(0) // queued, emitting, cancelled
    var rejectedCapabilities:String=""
        private set
    fun begin()=phase.compareAndSet(0,1)
    fun begin(peer:Peer,capabilities:Set<String>):Boolean {
        if(!begin())return false
        rejectedCapabilities=SettingsDeliveryPolicy.rejection(row,peer,capabilities)
        return true
    }
    fun cancel():Boolean = !phase.compareAndSet(0,2) && phase.get()==1
}

internal object EmissionPolicy {
    fun eligible(selected:Outgoing,current:Outgoing?,p:Prefs,binding:DeliveryBinding,
                 transportAllowed:Boolean,mobile:Boolean,capabilities:Set<String>,newerText:Boolean=false):Boolean =
        binding.matches(p) && (selected.peerId.isEmpty() || selected.peerId==binding.pcId) && selected.bindingGeneration==binding.generation &&
        p.devices.find {it.deviceId==binding.pcId}?.let {peer->
            // DataStore edits can precede the settings sender's flow callback.
            // Capture only a version the peer has actually synchronized.
            ("mesh" !in capabilities || !PairSettings.syncing(peer)) &&
            (selected.kind !in setOf("text","clipboard_image") || peer.clipboardSend) && (!selected.automatic || peer.automaticMedia)
        }==true && transportAllowed && current!=null &&
        current.state in setOf("waiting","failed") && current.mediaId==selected.mediaId && current.transferId==selected.transferId &&
        current.revision==selected.revision && !newerText &&
        (if(current.kind=="text")"clipboard_text" in capabilities else
            "files" in capabilities && (current.kind!="clipboard_image" || "clipboard_image" in capabilities) &&
            (!current.isFolder() || capabilities.containsAll(setOf(io.github.stantonmatt.pneu.protocol.CAP_FOLDER,io.github.stantonmatt.pneu.protocol.CAP_FILE_RESUME)))) &&
        !DeliveryPolicy.hold(current.kind,current.automatic,p.paused,mobile,p.originals,(current.attempted || current.payloadAttempted),current.optimized) &&
        (current.kind=="text" || !DeliveryPolicy.optimize(current.kind,current.mime,mobile,p.originals,(current.attempted || current.payloadAttempted),current.optimized))
}
