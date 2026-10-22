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

internal data class ShareRequest(val binding:Deferred<DeliveryBinding?>,val items:List<Uri>,val texts:List<String>,val admission:Deferred<FrozenAdmission>?=null)
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
            catch(e:CancellationException) {warn(processOwned,"cancelled");terminal.value=EntryResult();throw e}
            catch(e:Exception) {warn(processOwned,(e as? ShareFailure)?.reason ?: "read_failed");EntryResult(app.context.getString(R.string.toast_cant_read))}
        }
    }
    private fun warn(share:Boolean,reason:String) {if(share)ShareAdmission.warn(reason) else Log.w("Pneu","manual_entry_rejected: $reason")}
    fun interrupted(message:String) {if(!started) {Log.w("Pneu","manual_entry_rejected: interrupted");started=true;startedSignal.complete(Unit);terminal.value=EntryResult(message)}}
    fun failure(message:String,reason:String="read_failed") {if(!started) {Log.w("Pneu","manual_entry_rejected: $reason");started=true;startedSignal.complete(Unit);terminal.value=EntryResult(message)}}
    val targetSelection=MutableStateFlow<List<Peer>?>(null)
    private val selectedTargets=CompletableDeferred<Set<String>>()
    // Enable only when an activity owns the picker. Persisted opt-ins are inert
    // until then; the deadline still protects a missing/destroyed consumer.
    internal var targetPickerAvailable=false
    internal var targetSelectionTimeoutMillis=120000L
    fun selectTargets(ids:Set<String>) {require(ids.all {id->targetSelection.value?.any {it.deviceId==id}==true});selectedTargets.complete(ids)}
    fun cancelTargetSelection() {selectedTargets.complete(emptySet())}
    private suspend fun awaitTargets(peers:List<Peer>):Set<String> {
        targetSelection.value=peers
        return try {withTimeoutOrNull(targetSelectionTimeoutMillis) {selectedTargets.await()} ?: emptySet()}
        finally {targetSelection.value=null}
    }
    override fun onCleared() {cancelTargetSelection();super.onCleared()}
    fun share(request:ShareRequest)=launch(processOwned=true) {
        // Reserve every item's order before settings, picker or provider work;
        // later items must not originate after a newer clipboard operation.
        val orders=if(request.items.isNotEmpty())app.newTextOrders(request.items.size,apply=false)
            else app.newTextOrders(request.texts.size)
        val binding:DeliveryBinding?
        val p=try {binding=request.binding.await();app.preferences.read()}catch(e:Exception) {
            if(e is CancellationException)throw e
            ShareAdmission.warn("settings")
            return@launch EntryResult(app.context.getString(R.string.toast_settings))
        }
        if(p.devices.isEmpty() || p.pairingLost) {ShareAdmission.warn("not_paired");return@launch EntryResult(pair=true)}
        if(!p.shareSheet) {ShareAdmission.warn("disabled");return@launch EntryResult()}
        val frozen=request.admission?.await()?.targets(if(request.items.isNotEmpty())"file" else "text") ?: listOfNotNull(binding)
        if(!RecipientSelection.hasAuthority(p,frozen,binding)) {ShareAdmission.warn("binding_mismatch");return@launch EntryResult()}
        val selected=if(p.chooseDevicesEachTime && targetPickerAvailable)awaitTargets(p.devices.filter {peer->frozen.any {it.pcId==peer.deviceId}}) else null
        if(selected?.isEmpty()==true) {ShareAdmission.warn("cancelled");return@launch EntryResult()}
        val targets=frozen.filter {selected==null || it.pcId in selected}
        var count=0;var failed=request.items.isEmpty() && request.texts.isEmpty()
        if(failed)ShareAdmission.warn("empty")
        withContext(Dispatchers.IO) {
            suspend fun admit(body:suspend ()->Outgoing) {
                try {val row=body();completed.add(row.mediaId);count++}
                catch(e:CancellationException) {throw e}
                catch(e:Exception) {ShareAdmission.warn((e as? ShareFailure)?.reason ?: "read_failed");completed.add(null);failed=true}
            }
            if(request.items.isNotEmpty())for((index,uri) in request.items.withIndex())admit {ManualEntries.file(app,uri,"file",binding,targets=targets,order=orders[index])}
            else for((index,text) in request.texts.withIndex())admit {ManualEntries.text(app,text,"share",binding,targets,orders[index])}
        }
        return@launch EntryResult(when {
            failed->app.context.getString(R.string.toast_cant_read)
            app.connection.state.value.status=="Waiting for Wi-Fi"->app.context.getString(R.string.toast_queued_wifi)
            targets.any {app.connection.connected(it.pcId)}->UiPresentation.sendingTo(app.context,p.devices.filter {peer->targets.any {it.pcId==peer.deviceId}})
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
    private var clipboardAdmission:Deferred<FrozenAdmission>?=null
    fun captureClipboardAdmission(admission:Deferred<FrozenAdmission>) {
        if(clipboardBindingCaptured)return
        clipboardAdmission=admission
        captureClipboardBinding(app.scope.async(start=CoroutineStart.UNDISPATCHED) {admission.await().binding})
    }
    suspend fun awaitClipboardAdmission() {clipboardAdmission?.await();clipboardBinding?.await()}
    fun clipboard(clip:ClipData?)=launch {
        val order=if(clip!=null && clip.itemCount>0 && clip.description.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)!=true)
            shareStep("settings") {app.newTextOrder()} else null
        val binding=try {requireNotNull(clipboardBinding).await()}catch(e:Exception) {
            if(e is CancellationException)throw e
            Log.w("Pneu","manual_entry_rejected: settings")
            return@launch EntryResult(app.context.getString(R.string.toast_settings))
        }
        val frozen=clipboardAdmission?.await() ?: FrozenAdmission(binding,listOfNotNull(binding),listOfNotNull(binding),emptyList())
        val current=shareStep("settings") {app.preferences.read()}
        val textTargets=clip?.let {ClipboardSendPolicy.targets(it,current,frozen.text)}.orEmpty()
            .filter {app.recipientCurrent(it,current) && app.connection.connected(it.pcId) && app.connection.supports(it.pcId,"clipboard_text")}
        val imageTargets=clip?.let {ClipboardSendPolicy.targets(it,current,frozen.images)}.orEmpty()
            .filter {app.recipientCurrent(it,current) && app.connection.connected(it.pcId) && app.connection.supports(it.pcId,"clipboard_image") && app.connection.supports(it.pcId,"files")}
        when {
            !app.connection.connected || frozen.text.plus(frozen.images).none {it.matches(current)}-> {
                Log.w("Pneu","manual_entry_rejected: unavailable_binding");EntryResult(app.context.getString(R.string.toast_not_sent))
            }
            clip==null || clip.itemCount==0-> {Log.w("Pneu","manual_entry_rejected: empty");EntryResult(app.context.getString(R.string.toast_clipboard_empty))}
            clip.description.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)==true-> {Log.w("Pneu","manual_entry_rejected: sensitive");EntryResult(app.context.getString(R.string.toast_sensitive))}
            else->try {
                val row=withContext(Dispatchers.IO) {
                    val item=clip.getItemAt(0)
                    val type=if(item.uri!=null)shareStep("metadata_failed") {ProviderReads.call {app.context.contentResolver.getType(item.uri)}} else null
                    if(item.uri!=null && type?.startsWith("image/")==true) {
                        if(imageTargets.isEmpty())throw ShareFailure("no_recipient")
                        ManualEntries.file(app,item.uri,"clipboard_image",binding,type,imageTargets,requireNotNull(order))
                    }else {
                        val text=item.text?.toString().orEmpty()
                        if(text.isEmpty()) {Log.w("Pneu","manual_entry_rejected: empty");return@withContext null}
                        if(textTargets.isEmpty())throw ShareFailure("no_recipient")
                        ManualEntries.text(app,text,"clipboard",binding,textTargets,requireNotNull(order))
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
