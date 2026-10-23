package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.connection.ConnectionStatus
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

internal data class ShareRequest(val binding:Deferred<DeliveryBinding?>,val items:List<Uri>,val texts:List<String>,val admission:Deferred<FrozenAdmission>?=null,val explicitTargets:List<CapturedAuthority>?=null)
internal data class EntryResult(val message:String?=null,val pair:Boolean=false,val id:String=java.util.UUID.randomUUID().toString()) {
    fun show(context:Context) {
        if(pair)context.startActivity(Intent(context,MainActivity::class.java).putExtra("pair",true).apply {
            if(context !is android.app.Activity)addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        })
        message?.let {Toast.makeText(context,it,Toast.LENGTH_LONG).show()}
    }
}

/** Retains one admission across rotation. Shares run in the process scope with
 * service-owned grants, so clearing the entry task cannot cancel them. */
internal class ManualEntryViewModel(application:Application):AndroidViewModel(application) {
    private val app=application.graph
    private val terminal=MutableStateFlow<EntryResult?>(null)
    val result=terminal.asStateFlow()
    internal suspend fun showResult(context:Context) {result.filterNotNull().first().show(context)}
    internal val startedSignal=CompletableDeferred<Unit>()
    var started=false
        private set
    // Each element is terminal: an admitted media ID, or null for a failed item.
    // Recreation joins this same operation; it never replays completed items.
    val completed=mutableListOf<String?>()
    private fun launch(processOwned:Boolean=false,body:suspend ()->EntryResult) {
        if(started)return
        started=true;startedSignal.complete(Unit)
        (if(processOwned)app.scope else viewModelScope).launch(Dispatchers.Main.immediate+CoroutineName("manual-entry")) {
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
    // ShareActivity owns the picker and enables it. Other owners never wait for
    // a choice; the deadline still protects a missing/destroyed consumer.
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
        if(request.explicitTargets==null && !p.shareSheet) {ShareAdmission.warn("disabled");return@launch EntryResult()}
        val admission=request.admission?.await()
        val frozen=request.explicitTargets?.map {it.binding} ?: admission?.targets(if(request.items.isNotEmpty())"file" else "text") ?: listOfNotNull(binding)
        // Files may go to a computer that is offline now and wait for it. Shared
        // text never waits, so the picker offers only its frozen (connected) targets.
        val files=request.items.isNotEmpty()
        val choices=(request.explicitTargets ?: (if(files)admission?.fileChoices ?: frozen else frozen).map(::CapturedAuthority))
        val picking=request.explicitTargets==null && p.chooseDevicesEachTime && targetPickerAvailable
        val authority=if(picking && files)choices.map {it.binding} else frozen
        if(authority.isEmpty()) {ShareAdmission.warn("no_recipient");return@launch EntryResult(app.context.getString(R.string.toast_not_sent))}
        if(!RecipientSelection.hasAuthority(p,authority)) {ShareAdmission.warn("binding_mismatch");return@launch EntryResult(app.context.getString(R.string.toast_cant_read))}
        val eligible=app.recipients(p,if(files)"file" else "text",selected=if(picking && files || request.explicitTargets!=null)choices.map {it.binding.pcId}.toSet() else null)
        val offered=p.devices.filter {peer->eligible.any {it.pcId==peer.deviceId} && choices.any {
            it.binding.pcId==peer.deviceId && it.stillValid(p,app.connection.generation(it.binding)) && app.recipientCurrent(it.binding,p)
        }}
        if(offered.isEmpty()) {ShareAdmission.warn("no_recipient");return@launch EntryResult(app.context.getString(R.string.toast_not_sent))}
        val selected=if(picking)awaitTargets(offered) else null
        if(selected?.isEmpty()==true) {ShareAdmission.warn("cancelled");return@launch EntryResult()}
        val targets=when {
            selected==null->frozen.filter {target->offered.any {it.deviceId==target.pcId}}
            files->choices.filter {it.binding.pcId in selected}.map {it.binding}
            else->frozen.filter {it.pcId in selected}
        }
        val current=shareStep("settings") {app.preferences.read()}
        val surviving=targets.filter {target->choices.find {it.binding==target}?.stillValid(current,app.connection.generation(target))!=false && app.recipientCurrent(target,current)}
        if(surviving.isEmpty()) {ShareAdmission.warn("no_recipient");return@launch EntryResult(app.context.getString(R.string.toast_not_sent))}
        var count=0;var failed=request.items.isEmpty() && request.texts.isEmpty()
        if(failed)ShareAdmission.warn("empty")
        withContext(Dispatchers.IO) {
            suspend fun admit(body:suspend ()->Outgoing) {
                try {val row=body();completed.add(row.mediaId);count++}
                catch(e:CancellationException) {throw e}
                catch(e:Exception) {ShareAdmission.warn((e as? ShareFailure)?.reason ?: "read_failed");completed.add(null);failed=true}
            }
            if(request.items.isNotEmpty())for((index,uri) in request.items.withIndex())admit {ManualEntries.file(app,uri,"file",binding,targets=surviving,order=orders[index])}
            else for((index,text) in request.texts.withIndex())admit {ManualEntries.text(app,text,"share",binding,surviving,orders[index])}
        }
        return@launch EntryResult(when {
            failed->app.context.getString(R.string.toast_cant_read)
            app.connection.state.value.connectionStatus==ConnectionStatus.WAITING_WIFI->app.context.getString(R.string.toast_queued_wifi)
            targets.any {app.connection.connected(it.pcId)}->UiPresentation.sendingTo(app.context,p.devices.filter {peer->targets.any {it.pcId==peer.deviceId}},count)
            selected!=null && targets.size==1->app.context.getString(R.string.item_waiting_for,p.devices.find {it.deviceId==targets.single().pcId}?.name.orEmpty())
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
        val textEligible=app.recipients(current,"text").map {it.pcId}.toSet()
        val imageEligible=app.recipients(current,"clipboard_image").map {it.pcId}.toSet()
        val textTargets=clip?.let {ClipboardSendPolicy.targets(it,current,frozen.text)}.orEmpty()
            .filter {app.recipientCurrent(it,current) && it.pcId in textEligible}
        val imageTargets=clip?.let {ClipboardSendPolicy.targets(it,current,frozen.images)}.orEmpty()
            .filter {app.recipientCurrent(it,current) && it.pcId in imageEligible}
        when {
            !app.connection.connected || frozen.text.plus(frozen.images).none {it.matches(current)}-> {
                Log.w("Pneu","manual_entry_rejected: unavailable_binding");EntryResult(app.context.getString(R.string.toast_not_sent))
            }
            clip==null || clip.itemCount==0-> {Log.w("Pneu","manual_entry_rejected: empty");EntryResult(app.context.getString(R.string.toast_clipboard_empty))}
            clip.description.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)==true-> {Log.w("Pneu","manual_entry_rejected: sensitive");EntryResult(app.context.getString(R.string.toast_sensitive))}
            textTargets.isEmpty() && imageTargets.isEmpty()-> {
                Log.w("Pneu","manual_entry_rejected: no_recipient");EntryResult(app.context.getString(R.string.toast_not_sent))
            }
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
