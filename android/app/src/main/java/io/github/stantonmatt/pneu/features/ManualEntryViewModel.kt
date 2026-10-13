package io.github.stantonmatt.pneu.features

import android.app.Application
import android.content.*
import android.net.Uri
import android.util.Log
import android.widget.Toast
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.sha256
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

internal data class ShareRequest(val binding:Deferred<DeliveryBinding?>,val items:List<Uri>,val texts:List<String>)
internal data class EntryResult(val message:String?=null,val pair:Boolean=false,val id:String=java.util.UUID.randomUUID().toString())

/** Retains one admission across rotation. Shares run in the process scope with
 * service-owned grants, so clearing the entry task cannot cancel them. */
internal class ManualEntryViewModel(application:Application):AndroidViewModel(application) {
    private val app=application.graph
    private val terminal=MutableStateFlow<EntryResult?>(null)
    val result=terminal.asStateFlow()
    internal val startedSignal=CompletableDeferred<Unit>()
    var started=false
        private set
    // Each element is terminal: an admitted media ID, or null for a failed item.
    // Recreation joins this same operation; it never replays completed items.
    val completed=mutableListOf<String?>()
    private fun launch(processOwned:Boolean=false,body:suspend ()->EntryResult) {
        if(started)return
        started=true;startedSignal.complete(Unit)
        (if(processOwned)app.scope else viewModelScope).launch(Dispatchers.Main.immediate) {
            terminal.value=try {body()}
            catch(e:CancellationException) {warn(processOwned,"cancelled");throw e}
            catch(e:Exception) {warn(processOwned,(e as? ShareFailure)?.reason ?: "read_failed");EntryResult(app.context.getString(R.string.toast_cant_read))}
        }
    }
    private fun warn(share:Boolean,reason:String) {if(share)ShareAdmission.warn(reason) else Log.w("Pneu","manual_entry_rejected: $reason")}
    fun interrupted(message:String) {if(!started) {Log.w("Pneu","manual_entry_rejected: interrupted");started=true;startedSignal.complete(Unit);terminal.value=EntryResult(message)}}
    fun failure(message:String,reason:String="read_failed") {if(!started) {Log.w("Pneu","manual_entry_rejected: $reason");started=true;startedSignal.complete(Unit);terminal.value=EntryResult(message)}}
    fun share(request:ShareRequest)=launch(processOwned=true) {
        val binding:DeliveryBinding?
        val p=try {binding=request.binding.await();app.preferences.read()}catch(e:Exception) {
            if(e is CancellationException)throw e
            ShareAdmission.warn("settings")
            return@launch EntryResult(app.context.getString(R.string.toast_settings))
        }
        if(p.peer==null || p.pairingLost) {ShareAdmission.warn("not_paired");return@launch EntryResult(pair=true)}
        if(!p.shareSheet) {ShareAdmission.warn("disabled");return@launch EntryResult()}
        if(binding==null || !binding.matches(p)) {ShareAdmission.warn("binding_mismatch");return@launch EntryResult()}
        var count=0;var failed=request.items.isEmpty() && request.texts.isEmpty()
        if(failed)ShareAdmission.warn("empty")
        withContext(Dispatchers.IO) {
            suspend fun admit(body:suspend ()->Outgoing) {
                try {val row=body();completed.add(row.mediaId);count++}
                catch(e:CancellationException) {throw e}
                catch(e:Exception) {ShareAdmission.warn((e as? ShareFailure)?.reason ?: "read_failed");completed.add(null);failed=true}
            }
            if(request.items.isNotEmpty())for(uri in request.items)admit {ManualEntries.file(app,uri,"file",binding)}
            else for(text in request.texts)admit {ManualEntries.text(app,text,"share",binding)}
        }
        return@launch EntryResult(when {
            failed->app.context.getString(R.string.toast_cant_read)
            app.connection.state.value.status=="Waiting for Wi-Fi"->app.context.getString(R.string.toast_queued_wifi)
            app.connection.connected->if(count>1)app.context.resources.getQuantityString(R.plurals.toast_sending_many,count,count) else app.context.getString(R.string.toast_sending)
            else->app.context.getString(R.string.toast_queued)
        })
    }
    private var clipboardBindingCaptured=false
    internal var clipboardBinding:Deferred<DeliveryBinding?>?=null
        private set
    fun captureClipboardBinding(binding:Deferred<DeliveryBinding?>) {
        if(clipboardBindingCaptured)return
        clipboardBindingCaptured=true;clipboardBinding=binding
    }
    fun clipboard(clip:ClipData?)=launch {
        val binding=try {requireNotNull(clipboardBinding).await()}catch(e:Exception) {
            if(e is CancellationException)throw e
            Log.w("Pneu","manual_entry_rejected: settings")
            return@launch EntryResult(app.context.getString(R.string.toast_settings))
        }
        when {
            !app.connection.connected || binding==null || !binding.matches(shareStep("settings") {app.preferences.read()})-> {
                Log.w("Pneu","manual_entry_rejected: unavailable_binding");EntryResult(app.context.getString(R.string.toast_not_sent))
            }
            clip==null || clip.itemCount==0-> {Log.w("Pneu","manual_entry_rejected: empty");EntryResult(app.context.getString(R.string.toast_clipboard_empty))}
            clip.description.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)==true-> {Log.w("Pneu","manual_entry_rejected: sensitive");EntryResult(app.context.getString(R.string.toast_sensitive))}
            else->try {
                val row=withContext(Dispatchers.IO) {
                    val item=clip.getItemAt(0)
                    val type=if(item.uri!=null)shareStep("metadata_failed") {ProviderReads.call {app.context.contentResolver.getType(item.uri)}} else null
                    if(item.uri!=null && type?.startsWith("image/")==true) {
                        if(!app.connection.supports("clipboard_image"))throw ShareFailure("unsupported_kind")
                        ManualEntries.file(app,item.uri,"clipboard_image",binding,type)
                    }else {
                        val text=item.text?.toString().orEmpty()
                        if(text.isEmpty()) {Log.w("Pneu","manual_entry_rejected: empty");return@withContext null}
                        if(!app.connection.supports("clipboard_text"))throw ShareFailure("unsupported_kind")
                        val hash=sha256(text.toByteArray())
                        if(DeliveryPolicy.ownedEcho(clip.description.extras?.getBoolean(ClipboardDelivery.OWN_CLIP)==true,hash,clip.description.extras?.getString(ClipboardDelivery.OWN_HASH) ?: shareStep("settings") {app.awaitPrefs()}.lastReceivedHash)) {
                            Log.w("Pneu","manual_entry_rejected: owned_echo");return@withContext null
                        }
                        ManualEntries.text(app,text,"clipboard",binding)
                    }
                }
                if(row==null)EntryResult(app.context.getString(R.string.toast_clipboard_empty))else {
                    completed.add(row.mediaId)
                    app.scope.launch {PostCommit.maintainSuspending {
                        withTimeoutOrNull(120000) {app.queue.work().recoverReads().first {rows->rows.none {it.mediaId==row.mediaId}}}
                        val feedback=app.context.getString(ClipboardFeedback.result(app.queue.find(row.mediaId)))
                        withContext(Dispatchers.Main) {Toast.makeText(app.context,feedback,Toast.LENGTH_SHORT).show()}
                    }}
                    EntryResult()
                }
            }catch(e:CancellationException) {throw e}catch(e:Exception) {Log.w("Pneu","manual_entry_rejected: ${(e as? ShareFailure)?.reason ?: "read_failed"}");EntryResult(app.context.getString(R.string.toast_not_sent))}
        }
    }
}
