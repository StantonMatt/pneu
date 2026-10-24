package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.ClipDescription
import android.content.Context
import kotlinx.coroutines.CancellationException
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.sha256

/** Resolve provenance once per admission and carry it through picker, staging
 * and queue admission. Clipboard content uses the latest sender in the clipboard
 * register/history, falling back to its OS marker (deduplication may retain an
 * older marker/URI); never consult that URI's receive journal. A shared received
 * file instead uses its validated receive-journal source, even when clipboard
 * history knows identical bytes from another sender. Other staged files/media
 * use their actual byte hash when staging finishes. Identical clipboard content
 * has only its latest sender as source: deliberate sends skip that computer,
 * while automatic received-content echoes are suppressed for every computer. */
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
        return targets(p,eligible,clipboardProvenance(clip,p,image)).isNotEmpty()
    }
    fun targets(clip:ClipData,p:Prefs,captured:List<DeliveryBinding>,automatic:Boolean=false):List<DeliveryBinding> {
        return targets(p,captured,clipboardProvenance(clip,p),automatic)
    }
    fun clipboardProvenance(clip:ClipData,p:Prefs,image:Boolean=clip.itemCount>0 && clip.getItemAt(0).uri!=null):SendProvenance {
        val extras=clip.description.extras
        if(extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)==true)return SendProvenance(sensitive=true)
        val owned=extras?.getBoolean(ClipboardDelivery.OWN_CLIP)==true
        val item=if(clip.itemCount>0)clip.getItemAt(0) else null
        val hash=if(image)(if(owned)extras?.getString(ClipboardDelivery.OWN_HASH) else null).orEmpty()
            else item?.text?.toString()?.let {sha256(it.toByteArray())}.orEmpty()
        val markedSource=if(owned)extras?.getString(ClipboardDelivery.OWN_SOURCE).orEmpty() else ""
        // A legacy marker without a hash is decisive only if history cannot
        // name a different sender. Otherwise staging must identify its bytes.
        val competingSource=p.recentClipboardSources.values.any {it.isNotEmpty() && it!=markedSource} ||
            p.lastReceivedHash.isNotEmpty() && p.lastReceivedSource.isNotEmpty() && p.lastReceivedSource!=markedSource
        return if(hash.isEmpty() && image && (markedSource.isEmpty() || competingSource))
            SendProvenance(received=owned,source=markedSource,needsHash=true)
            else provenance(p,hash,owned,markedSource)
    }
    /** Ordinary files never enter clipboard history. Resolve their publication
     * journal before staging drops the source URI, including document-provider
     * references to the same MediaStore item. Catalog/generation checks prevent
     * a stale receipt from claiming a recycled item or a replacement pairing. */
    suspend fun fileProvenance(app:io.github.stantonmatt.pneu.AppGraph,uri:android.net.Uri,p:Prefs):SendProvenance {
        val source=fileSource(app,uri,p)
        return fileProvenance(source)
    }
    fun fileProvenance(source:String):SendProvenance =
        if(source.isEmpty())SendProvenance(needsHash=true) else SendProvenance(received=true,source=source)
    private suspend fun fileSource(app:io.github.stantonmatt.pneu.AppGraph,uri:android.net.Uri,p:Prefs):String =
        ProviderReads.call {signal->
            val media=if(uri.authority=="media")uri else if(android.provider.DocumentsContract.isDocumentUri(app.context,uri))
                android.provider.MediaStore.getMediaUri(app.context,uri) else null
            if(media==null)return@call ""
            // Files, Downloads, Images and document-provider aliases may refer
            // to one row. Resolve its volume/ID instead of comparing URI text.
            val id=app.context.contentResolver.query(media,arrayOf("_id","volume_name"),null,null,null,signal)?.use {
                if(it.moveToFirst() && it.getString(1)==MediaCatalog.VOLUME)it.getLong(0) else null
            } ?: return@call ""
            val storage=ReceivedMediaStore(app.context)
            val row=app.queue.receivedFileByMediaId(storage.version(),id)
            if(row!=null && p.devices.any {it.deviceId==row.peerId && it.generation==row.generation} &&
                ReceivedFileActions.published(storage,row))row.peerId else ""
        }
    /** Staged shares use their actual bytes, including text files and images
     * whose share intents no longer carry our clipboard ownership extras. */
    fun targets(p:Prefs,captured:List<DeliveryBinding>,hash:String,automatic:Boolean=false):List<DeliveryBinding> {
        return targets(p,captured,provenance(p,hash),automatic)
    }
    fun provenance(p:Prefs,hash:String,owned:Boolean=false,markedSource:String=""):SendProvenance {
        val received=owned || hash.isNotEmpty() && (hash==p.lastReceivedHash || hash in p.recentClipboardHashes || hash in p.recentClipboardSources)
        val rememberedSource=if(hash.isEmpty())"" else p.recentClipboardSources[hash].orEmpty().ifEmpty {
            if(hash==p.lastReceivedHash)p.lastReceivedSource else ""
        }
        // A deduplicated receive advances provenance without replacing the OS marker.
        val source=rememberedSource.ifEmpty {markedSource}
        return SendProvenance(received=received,source=source)
    }
    fun targets(p:Prefs,captured:List<DeliveryBinding>,provenance:SendProvenance,automatic:Boolean=false):List<DeliveryBinding> {
        if(provenance.sensitive || automatic && provenance.received)return emptyList()
        // An old image marker may know a sender but lack its hash. Wait for
        // staged bytes before excluding anyone: history may name a newer sender.
        return RecipientSelection.surviving(p,captured).filter {provenance.needsHash || it.pcId!=provenance.source}
    }
}
