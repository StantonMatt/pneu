package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.ClipDescription
import android.content.Context
import kotlinx.coroutines.CancellationException
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.sha256

/** Explicit sends originate new operations; automatic echoes never do. */
internal object ClipboardSendPolicy {
    suspend fun available(context:Context,clip:ClipData?,p:Prefs,links:Map<String,io.github.stantonmatt.pneu.connection.LinkState>):Boolean {
        if(clip==null || clip.itemCount==0 || !p.loaded || clip.description.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)==true)return false
        val uri=clip.getItemAt(0).uri
        val mime=try {uri?.let {source->ProviderReads.call {context.contentResolver.getType(source)}}}
        catch(e:CancellationException) {throw e}
        catch(_:Exception) {return false}
        return available(clip,p,links,mime)
    }
    fun available(clip:ClipData?,p:Prefs,links:Map<String,io.github.stantonmatt.pneu.connection.LinkState>,resolvedMime:String?=null):Boolean {
        if(clip==null || clip.itemCount==0 || !p.loaded)return false
        val item=clip.getItemAt(0)
        // Match ManualEntryViewModel: provider MIME decides between image and
        // text, including raw URI clips advertising only text/uri-list.
        val image=item.uri!=null && resolvedMime?.startsWith("image/")==true
        if(!image && item.text.isNullOrEmpty())return false
        val eligible=p.devices.filter {peer->!peer.pairingLost && peer.deviceId !in p.resetPeers && peer.clipboardSend &&
            links[peer.deviceId]?.let {it.connectionStatus==io.github.stantonmatt.pneu.connection.ConnectionStatus.CONNECTED &&
                (if(image)"files" in it.capabilities && "clipboard_image" in it.capabilities else "clipboard_text" in it.capabilities)}==true}
            .mapNotNull {DeliveryBinding.from(p,it.deviceId)}
        return targets(clip,p,eligible).isNotEmpty()
    }
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
