package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.ClipboardManager
import android.net.Uri
import android.os.PersistableBundle
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel

/** Optional actions never delay a file ACK. One bounded worker serializes image
 * effects with the existing device-wide clipboard policy boundary. */
internal class ReceivedFileEffects(private val app:AppGraph) {
    private data class Effect(val file:ReceivedFile,val metadata:ReceivedMetadata,val mesh:Boolean,val authority:CapturedAuthority)
    private val pending=Channel<Effect>(16)
    init {app.scope.launch {
        for(effect in pending)try {
            applyClipboard(effect.file,effect.metadata,effect.mesh,effect.authority)
            if(effect.file.kind!="clipboard_image")ReceivedFileNotifications.post(app,effect.file,effect.authority)
        }catch(e:CancellationException) {throw e}
        catch(e:Exception) {android.util.Log.w("PneuReceiveEffect",e.javaClass.simpleName)}
    }}
    fun offer(row:ReceivedFile,metadata:ReceivedMetadata,mesh:Boolean,authority:CapturedAuthority) {pending.trySend(Effect(row,metadata,mesh,authority))}
    internal suspend fun recoverClipboardMarker() {
        val extras=withContext(Dispatchers.Main) {
            app.context.getSystemService(ClipboardManager::class.java).primaryClipDescription?.extras
        } ?: return
        if(!extras.getBoolean(IMAGE_MARKER))return
        val peer=extras.getString(ClipboardDelivery.OWN_SOURCE) ?: return
        val id=extras.getString(TRANSFER) ?: return
        val hash=extras.getString(ClipboardDelivery.OWN_HASH) ?: return
        val order=extras.getString(ORDER)?.toBigIntegerOrNull() ?: return
        val row=app.queue.receivedFile(peer,id)?.takeIf {it.state=="complete" && it.generation==extras.getLong(GENERATION)} ?: return
        val metadata=ReceivedMetadata.parse(com.google.gson.JsonParser.parseString(row.metadata).asJsonObject)
        try {MeshClock.validate(order)}catch(_:MeshViolation) {return}
        if(!extras.getBoolean(LEGACY) && metadata.hlc!=order.toString())return
        if(hash!=metadata.hash || app.preferences.read().devices.none {it.deviceId==peer && it.generation==row.generation})return
        app.preferences.recordClipboardEffect(ClipboardStamp(order,peer),hash,peer,row.generation,id,if(extras.getBoolean(LEGACY))metadata.createdAt else null)
        app.preferences.reconcileClipboardEffects()
    }
    internal suspend fun applyClipboard(row:ReceivedFile,metadata:ReceivedMetadata,mesh:Boolean,authority:CapturedAuthority,
        apply:(suspend (ClipData)->Unit)?=null) {
        // Phone clipboard arrival is explicit clipboard_image only. Ordinary
        // received photos remain ordinary files, with no clipboard write.
        if(metadata.kind!="clipboard_image")return
        if(authority.binding.pcId!=row.peerId || authority.binding.generation!=row.generation)return
        val currentImage=if(apply==null)ClipboardContent.imageSnapshot(app.context) else null
        app.withAuthority(authority) {app.preferences.policy.locked {
            val p=app.preferences.read();val peer=p.devices.find {it.deviceId==row.peerId} ?: return@locked
            if(p.paused || !peer.clipboardReceive)return@locked
            val stamp=if(mesh)metadata.hlc?.let {ClipboardStamp(it.toBigInteger(),row.peerId)} ?: return@locked
                else {
                    if(!DeliveryPolicy.newer(metadata.createdAt,app.preferences.legacyClipboardTime(row.peerId,row.generation,app.queue.lastApplied(row.peerId))))return@locked
                    app.preferences.legacyClipboardStamp(row.peerId)
                }
            if(!app.preferences.clipboardWins(stamp))return@locked
            // Metadata already supplies the validated MIME; avoid a provider
            // getType() query while holding the global OS-effect boundary.
            val clip=ClipData(app.context.getString(R.string.app_name),arrayOf(row.mime),ClipData.Item(Uri.parse(row.uri))).apply {
                description.extras=PersistableBundle().apply {
                    putBoolean(ClipboardDelivery.OWN_CLIP,true);if(metadata.sensitive)putBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE,true);putBoolean(IMAGE_MARKER,true);putBoolean(LEGACY,!mesh)
                    putString(ClipboardDelivery.OWN_SOURCE,row.peerId);putString(ClipboardDelivery.OWN_HASH,metadata.hash)
                    putString(TRANSFER,row.transferId);putString(ORDER,stamp.hlc.toString());putLong(GENERATION,row.generation)
                }
            }
            withContext(NonCancellable) {
                // Recheck after URI/ClipDescription preparation, immediately at
                // the effect boundary. The policy lock excludes every local/text
                // effect, and clipboardWins includes unfinished OS effects.
                if(!authority.stillValid(app.preferences.read(),app.connection.generation(authority.binding)) ||
                    !ReceivedFileActions.published(ReceivedMediaStore(app.context),row) || !app.preferences.clipboardWins(stamp))return@withContext
                if(apply!=null)apply(clip) else withContext(Dispatchers.Main) {
                    val clipboard=app.context.getSystemService(ClipboardManager::class.java)
                    if(!ClipboardContent.imageMatches(runCatching {clipboard.primaryClip}.getOrNull(),metadata.hash,currentImage) || metadata.sensitive && clipboard.primaryClipDescription?.extras?.getBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE)!=true)clipboard.setPrimaryClip(clip)
                }
                PostCommit.maintainSuspending {app.savedClips.recordImage(row.mime,peer.name,metadata.sensitive,source=row.peerId) {app.context.contentResolver.openInputStream(Uri.parse(row.uri))}}
                app.preferences.recordClipboardEffect(stamp,metadata.hash,row.peerId,row.generation,row.transferId,if(mesh)null else metadata.createdAt)
                app.preferences.appliedClipboard(stamp,metadata.hash,row.peerId,row.generation,row.transferId)
                if(!mesh) {
                    app.queue.insertClock(ClipboardClock().apply {peerId=row.peerId;lastApplied=metadata.createdAt})
                    app.queue.advanceClock(row.peerId,metadata.createdAt)
                }
                app.preferences.finishClipboardCompletion(row.peerId,row.generation,row.transferId)
            }
        }}
    }
    companion object {
        private const val LEGACY="io.github.stantonmatt.pneu.clipboard.legacyImage"
        private const val IMAGE_MARKER="io.github.stantonmatt.pneu.clipboard.receivedImage"
        private const val TRANSFER="io.github.stantonmatt.pneu.clipboard.transfer"
        private const val ORDER="io.github.stantonmatt.pneu.clipboard.order"
        private const val GENERATION="io.github.stantonmatt.pneu.clipboard.generation"
    }
}
