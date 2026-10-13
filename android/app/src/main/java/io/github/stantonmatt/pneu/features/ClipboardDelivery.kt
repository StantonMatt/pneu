package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.ClipboardManager
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.Receipt
import io.github.stantonmatt.pneu.data.DeliveryBinding
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/** Last writer wins. Durable ID receipts and the last-applied timestamp; no inbound barriers. */
class ClipboardDelivery(private val app: AppGraph, private val applyText:suspend (String,String)->Unit={text,hash->
    withContext(Dispatchers.Main) {app.context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText(app.context.getString(R.string.app_name),text).apply {description.extras=android.os.PersistableBundle().apply {putBoolean(OWN_CLIP,true);putString(OWN_HASH,hash)}})}
}) {
    companion object {const val OWN_CLIP="io.github.stantonmatt.pneu.clipboard";const val OWN_HASH="io.github.stantonmatt.pneu.clipboard.hash"}
    private val incoming=Mutex()
    suspend fun receive(peerId:String,body:com.google.gson.JsonObject):Frame.Control=receive(peerId,body,DeliveryBinding.from(app.awaitPrefs()))
    suspend fun receive(peerId:String,body:com.google.gson.JsonObject,binding:DeliveryBinding?):Frame.Control=incoming.withLock {
        if(binding?.pcId!=peerId)throw java.io.IOException("Binding changed")
        // A local receipt/settings read failure is a recoverable NACK, not a TLS failure.
        // Binding revocation remains outside this catch and invalidates the old session.
        val result=try {app.withBinding(binding) {
            try {receiveCurrent(peerId,body)}
            catch(e:CancellationException) {throw e}
            catch(_:Exception) {Frame.Control(Type.NACK,json("transfer_id" to body["transfer_id"].asString,"code" to "storage_failed","retryable" to true))}
        }} catch(e:java.io.IOException) {Frame.Control(Type.NACK,json("transfer_id" to body["transfer_id"].asString,"code" to "storage_failed","retryable" to true))}
        result ?: throw java.io.IOException("Binding changed")
    }
    private suspend fun receiveCurrent(peerId:String,body:com.google.gson.JsonObject):Frame.Control {
        val id=body["transfer_id"].asString;val text=body["text"].asString
        val hash=sha256(text.toByteArray());val timestamp=body["created_at"].asLong;val origin=body["origin"]?.asString ?: "clipboard"
        fun nack(code: String)=Frame.Control(Type.NACK,json("transfer_id" to id,"code" to code,"retryable" to (code !in setOf("id_conflict","unsupported_kind"))))
        val existing=app.queue.receipt(peerId,id)
        if(existing != null) {
            if(existing.hash != hash || existing.createdAt != timestamp || existing.origin != origin)return nack("id_conflict")
            if(existing.state!="pending")return Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to true))
        }
        // Legacy pending receipts have no role in last-writer-wins delivery.
        if(existing?.state=="pending")app.queue.deleteReceipt(peerId,id)
        val record=Receipt().apply {this.peerId=peerId;transferId=id;this.hash=hash;createdAt=timestamp;this.origin=origin}
        val p=app.preferences.read()
        val stale=!DeliveryPolicy.newer(timestamp,app.queue.lastApplied(peerId))
        if(!stale && !p.paused && p.clipboardFromPc) {
            try {
                // The clip itself carries its ownership hash, including across crash replay.
                applyText(text,hash)
                app.preferences.setString("lastReceivedHash",hash)
            } catch(e:CancellationException) {throw e}
            catch(_: Exception) {return nack("clipboard_unavailable")}
        }
        val applied=!stale && !p.paused && p.clipboardFromPc
        if(applied) {
            record.state="applied"
            try {app.queue.completeReceipt(record)} catch(e:CancellationException) {throw e}
            catch(_: Exception) {return nack("storage_failed")}
        }
        return Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to stale))
    }
}
