package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.ClipboardManager
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/** Last writer wins. Durable ID receipts and the last-applied timestamp; no inbound barriers. */
class ClipboardDelivery(private val app: AppGraph, private val applyText:(suspend (String,String)->Unit)?=null) {
    companion object {
        const val OWN_CLIP="io.github.stantonmatt.pneu.clipboard"
        const val OWN_SOURCE="io.github.stantonmatt.pneu.clipboard.source"
        const val OWN_HASH="io.github.stantonmatt.pneu.clipboard.hash"
        private const val OWN_TRANSFER="io.github.stantonmatt.pneu.clipboard.transfer"
        private const val OWN_GENERATION="io.github.stantonmatt.pneu.clipboard.generation"
        private const val OWN_ORDER="io.github.stantonmatt.pneu.clipboard.order"
        internal fun receivedClip(label:String,text:String,hash:String,source:String,id:String="",order:String="",generation:Long=0,sensitive:Boolean=false)=ClipData.newPlainText(label,text).apply {
            description.extras=android.os.PersistableBundle().apply {putBoolean(OWN_CLIP,true);if(sensitive)putBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE,true);putString(OWN_HASH,hash);putString(OWN_SOURCE,source);putString(OWN_TRANSFER,id);putString(OWN_ORDER,order);putLong(OWN_GENERATION,generation)}
        }
        internal fun disabledDirection(id:String,mesh:Boolean)=if(mesh)
            Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "unsupported_kind","retryable" to false))
        else Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to false))
    }
    private val incoming=Mutex()
    /** Recover any surviving OS marker, regardless of the next sender/ID. The
     * reservation proves that these extras belong to an unfinished operation. */
    private suspend fun recoverOsEffect() {
        if(applyText!=null)return
        val extras=withContext(Dispatchers.Main) {
            app.context.getSystemService(ClipboardManager::class.java).primaryClipDescription?.extras
        } ?: return
        if(!extras.getBoolean(OWN_CLIP))return
        val peer=extras.getString(OWN_SOURCE) ?: return
        val id=extras.getString(OWN_TRANSFER) ?: return
        val hash=extras.getString(OWN_HASH) ?: return
        val generation=extras.getLong(OWN_GENERATION)
        val order=extras.getString(OWN_ORDER)?.toBigIntegerOrNull() ?: return
        val record=app.queue.receipt(peer,id) ?: return
        if(record.state!="reserved" || record.hash!=hash)return
        MeshClock.validate(order)
        if(app.preferences.read().devices.none {it.deviceId==peer && it.generation==generation})return
        app.preferences.recordClipboardEffect(ClipboardStamp(order,peer),hash,peer,generation,id)
    }
    /** Startup and all subsequent traffic recover effects across every peer. */
    internal suspend fun reconcile()=app.preferences.policy.locked {reconcileCurrent()}
    private suspend fun reconcileCurrent() {
        recoverOsEffect()
        app.preferences.reconcileClipboardEffects()
        for(peer in app.preferences.read().devices) {
            for(id in app.preferences.unfinishedClipboardCompletions(peer.deviceId,peer.generation)) {
                val retained=app.queue.receipt(peer.deviceId,id)
                if(retained?.state=="reserved")app.queue.completeReceipt(retained.apply {state="applied"})
                PostCommit.maintainSuspending {app.preferences.finishClipboardCompletion(peer.deviceId,peer.generation,id)}
            }
        }
    }
    suspend fun receive(peerId:String,body:com.google.gson.JsonObject):Frame.Control=receive(peerId,body,DeliveryBinding.from(app.awaitPrefs(),peerId))
    suspend fun receive(peerId:String,body:com.google.gson.JsonObject,binding:DeliveryBinding?,mesh:Boolean=false):Frame.Control=incoming.withLock {
        val stamp=if(mesh) {
            if(!body.has("hlc") || body["source_device"]?.asString!=peerId)throw MeshViolation()
            ClipboardStamp(body["hlc"].asString.toBigInteger(),peerId).also {MeshClock.validate(it.hlc)}
        } else null
        if(binding?.pcId!=peerId)throw java.io.IOException("Binding changed")
        // A local receipt/settings read failure is a recoverable NACK, not a TLS failure.
        // Binding revocation remains outside this catch and invalidates the old session.
        var phase="binding_prefs"
        val result=try {app.withBinding(binding) {
            try {app.preferences.policy.locked {receiveCurrent(peerId,requireNotNull(binding).generation,body,stamp) {phase=it}}}
            catch(e:CancellationException) {throw e}
            catch(e:Exception) {
                ReceiveDiagnostics.warn(peerId,body["transfer_id"].asString,"text",phase,e)
                Frame.Control(Type.NACK,json("transfer_id" to body["transfer_id"].asString,"code" to "storage_failed","retryable" to true))
            }
        }} catch(e:java.io.IOException) {
            ReceiveDiagnostics.warn(peerId,body["transfer_id"].asString,"text",phase,e)
            Frame.Control(Type.NACK,json("transfer_id" to body["transfer_id"].asString,"code" to "storage_failed","retryable" to true))
        }
        result ?: throw java.io.IOException("Binding changed")
    }
    private suspend fun receiveCurrent(peerId:String,generation:Long,body:com.google.gson.JsonObject,stamp:ClipboardStamp?,phase:(String)->Unit):Frame.Control {
        phase("read_body")
        val sensitive=body["sensitive"]?.asBoolean==true
        val id=body["transfer_id"].asString;val text=body["text"].asString
        val hash=sha256(text.toByteArray());val timestamp=body["created_at"].asLong;val origin=body["origin"]?.asString ?: "clipboard"
        fun nack(code: String)=Frame.Control(Type.NACK,json("transfer_id" to id,"code" to code,"retryable" to (code !in setOf("id_conflict","unsupported_kind"))))
        if(stamp!=null) {phase("observe_hlc");app.preferences.observeClipboard(stamp)}
        // Retained-ID conflicts are decided before fallible journal recovery.
        phase("receipt_lookup")
        val reserved=app.queue.receipt(peerId,id)
        fun conflicts(record:Receipt)=record.sensitive!=sensitive || record.hash!=hash || record.createdAt!=timestamp || record.origin!=origin || record.hlc!=stamp?.hlc?.toString().orEmpty() || record.sourceDevice!=stamp?.source.orEmpty()
        if(reserved!=null && conflicts(reserved))return nack("id_conflict")
        // Conflicts remain authoritative even when recovery storage is failing.
        phase("reconcile")
        reconcileCurrent()
        phase("receipt_lookup")
        val existing=app.queue.receipt(peerId,id)
        if(existing != null) {
            if(conflicts(existing))return nack("id_conflict")
            if(existing.state !in setOf("pending","reserved")) {
                return Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to true))
            }
        }
        // Old pending reservations predate this journal and never proved an
        // effect. New reserved rows retain metadata through completion failures.
        if(existing?.state=="pending") {phase("delete_legacy");app.queue.deleteReceipt(peerId,id)}
        val record=Receipt().apply {this.sensitive=sensitive;this.peerId=peerId;transferId=id;this.hash=hash;createdAt=timestamp;this.origin=origin;hlc=stamp?.hlc?.toString().orEmpty();sourceDevice=stamp?.source.orEmpty();state="reserved"}
        suspend fun finish(applied:Boolean) {
            phase("receipt_complete")
            record.state="applied"
            if(applied)app.queue.completeReceipt(record) else app.queue.retainClipboardReceipt(record)
            PostCommit.maintainSuspending {app.preferences.finishClipboardCompletion(peerId,generation,id)}
        }
        phase("read_state")
        val p=app.preferences.read()
        val peer=p.devices.find {it.deviceId==peerId}
        if(peer?.clipboardReceive!=true)return disabledDirection(id,stamp!=null)
        val stale=if(stamp==null)!DeliveryPolicy.newer(timestamp,app.preferences.legacyClipboardTime(peerId,generation,app.queue.lastApplied(peerId))) else !app.preferences.clipboardWins(stamp)
        var deduplicated=stale
        if(!stale && !p.paused) {
            // Reserve the full wire metadata before touching the OS. A changed
            // retry must conflict even when the final receipt write failed.
            phase("reserve")
            if(existing?.state!="reserved")check(app.queue.insertReceipt(record)!=-1L)
            phase("read_state")
            val appliedStamp=stamp ?: app.preferences.legacyClipboardStamp(peerId)
            var effected=false
            withContext(NonCancellable) {
                phase("apply")
                try {
                    if(applyText!=null)applyText.invoke(text,hash) else withContext(Dispatchers.Main) {
                        val clipboard=app.context.getSystemService(ClipboardManager::class.java)
                        deduplicated=ClipboardContent.textMatches(runCatching {clipboard.primaryClip}.getOrNull(),text) && (!sensitive || clipboard.primaryClipDescription?.extras?.getBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE)==true)
                        if(!deduplicated)clipboard.setPrimaryClip(receivedClip(app.context.getString(R.string.app_name),text,hash,peerId,id,appliedStamp.hlc.toString(),generation,sensitive))
                    }
                }catch(e:CancellationException) {phase("apply_cleanup");app.queue.deleteReceipt(peerId,id);throw e}
                catch(e:Exception) {
                    ReceiveDiagnostics.warn(peerId,id,"text","apply",e,code="clipboard_unavailable")
                    phase("apply_cleanup");app.queue.deleteReceipt(peerId,id);return@withContext
                }
                effected=true
                phase("applied_prefs")
                app.preferences.recordClipboardEffect(appliedStamp,hash,peerId,generation,id)
                app.preferences.appliedClipboard(appliedStamp,hash,peerId,generation,id)
            }
            if(!effected)return nack("clipboard_unavailable")
            PostCommit.maintainSuspending {app.savedClips.recordText(text,peer.name,sensitive,source=peerId)}
            finish(true)
        } else if(existing?.state=="reserved")finish(false)
        return Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to deduplicated))
    }
}
