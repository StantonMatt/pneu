package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.DeliveryPolicy
import java.util.concurrent.atomic.AtomicInteger

/** Cancellation before the first frame is local; an emitted text keeps its receipt. */
internal class QueuedEmission(val row:Outgoing,val binding:DeliveryBinding) {
    private val phase=AtomicInteger(0) // queued, emitting, cancelled
    fun begin()=phase.compareAndSet(0,1)
    fun cancel():Boolean = !phase.compareAndSet(0,2) && phase.get()==1
}

internal object EmissionPolicy {
    fun eligible(selected:Outgoing,current:Outgoing?,p:Prefs,binding:DeliveryBinding,
                 transportAllowed:Boolean,mobile:Boolean,capabilities:Set<String>,newerText:Boolean=false):Boolean =
        binding.matches(p) && transportAllowed && current!=null &&
        current.state in setOf("waiting","failed") && current.mediaId==selected.mediaId && current.transferId==selected.transferId &&
        current.revision==selected.revision && !newerText &&
        (if(current.kind=="text")"clipboard_text" in capabilities else
            "files" in capabilities && (current.kind!="clipboard_image" || "clipboard_image" in capabilities)) &&
        !DeliveryPolicy.hold(current.kind,current.automatic,p.paused,mobile,p.originals,current.attempted,current.optimized) &&
        (current.kind=="text" || !DeliveryPolicy.optimize(current.kind,current.mime,mobile,p.originals,current.attempted,current.optimized))
}
