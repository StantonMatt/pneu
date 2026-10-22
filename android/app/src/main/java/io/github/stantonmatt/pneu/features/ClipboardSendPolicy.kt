package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.ClipDescription
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.sha256

/** Explicit sends originate new operations; automatic echoes never do. */
internal object ClipboardSendPolicy {
    fun targets(clip:ClipData,p:Prefs,captured:List<DeliveryBinding>,automatic:Boolean=false):List<DeliveryBinding> {
        val extras=clip.description.extras
        if(extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)==true)return emptyList()
        val hash=if(clip.itemCount>0)clip.getItemAt(0).text?.toString()?.let {sha256(it.toByteArray())}.orEmpty() else ""
        val owned=extras?.getBoolean(ClipboardDelivery.OWN_CLIP)==true
        val received=owned || hash.isNotEmpty() && (hash==p.lastReceivedHash || hash in p.recentClipboardHashes || hash in p.recentClipboardSources)
        if(automatic && received)return emptyList()
        val rememberedSource=if(hash.isEmpty())"" else p.recentClipboardSources[hash].orEmpty().ifEmpty {
            if(hash==p.lastReceivedHash)p.lastReceivedSource else ""
        }
        val source=if(owned)extras?.getString(ClipboardDelivery.OWN_SOURCE).orEmpty().ifEmpty {
            rememberedSource
        } else rememberedSource
        return RecipientSelection.surviving(p,captured).filter {it.pcId!=source}
    }
}
