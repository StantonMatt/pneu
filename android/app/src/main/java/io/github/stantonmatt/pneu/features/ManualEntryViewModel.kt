package io.github.stantonmatt.pneu.features

import android.app.Application
import android.content.*
import android.net.Uri
import android.widget.Toast
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.sha256
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

internal data class ShareRequest(val binding:Deferred<DeliveryBinding?>,val items:List<Uri>,val texts:List<String>,val grants:Boolean)
internal data class EntryResult(val message:String?=null,val pair:Boolean=false)

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
            catch(e:CancellationException) {throw e}
            catch(_:Exception) {EntryResult("Couldn't read this item")}
        }
    }
    fun interrupted(message:String) {if(!started) {started=true;startedSignal.complete(Unit);terminal.value=EntryResult(message)}}
    fun failure(message:String) {if(!started) {started=true;startedSignal.complete(Unit);terminal.value=EntryResult(message)}}
    fun share(request:ShareRequest)=launch(processOwned=true) {
        val binding:DeliveryBinding?
        val p=try {binding=request.binding.await();app.preferences.read()}catch(e:Exception) {
            if(e is CancellationException)throw e
            return@launch EntryResult("Couldn't load Pneu settings. Open Pneu and try again.")
        }
        if(p.peer==null || p.pairingLost)return@launch EntryResult(pair=true)
        if(!p.shareSheet || binding==null || !binding.matches(p))return@launch EntryResult()
        if(!request.grants)return@launch EntryResult("Couldn't read this item")
        var count=0;var failed=request.items.isEmpty() && request.texts.isEmpty()
        withContext(Dispatchers.IO) {
            suspend fun admit(body:suspend ()->Outgoing) {
                try {val row=body();completed.add(row.mediaId);count++}
                catch(e:CancellationException) {throw e}
                catch(_:Exception) {completed.add(null);failed=true}
            }
            if(request.items.isNotEmpty())for(uri in request.items)admit {ManualEntries.file(app,uri,"file",binding)}
            else for(text in request.texts)admit {ManualEntries.text(app,text,"share",binding)}
        }
        return@launch EntryResult(when {
            failed->"Couldn't read this item"
            app.connection.state.value.status=="Waiting for Wi-Fi"->"Waiting for Wi-Fi. Queued."
            app.connection.connected->if(count>1)"Sending $count items to PC" else "Sending to PC"
            else->"PC unreachable. Queued."
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
            return@launch EntryResult("Couldn't load Pneu settings. Open Pneu and try again.")
        }
        when {
            !app.connection.connected || binding==null || !binding.matches(app.preferences.read())->EntryResult("PC unreachable. Not sent.")
            clip==null || clip.itemCount==0->EntryResult("Clipboard is empty")
            clip.description.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)==true->EntryResult("Sensitive clipboard. Not sent.")
            else->try {
                val row=withContext(Dispatchers.IO) {
                    val item=clip.getItemAt(0)
                    val type=if(item.uri!=null)ProviderReads.call {app.context.contentResolver.getType(item.uri)} else null
                    if(item.uri!=null && type?.startsWith("image/")==true) {
                        check(app.connection.supports("clipboard_image"));ManualEntries.file(app,item.uri,"clipboard_image",binding,type)
                    }else {
                        val text=item.text?.toString().orEmpty()
                        if(text.isEmpty())return@withContext null
                        check(app.connection.supports("clipboard_text"))
                        val hash=sha256(text.toByteArray())
                        if(DeliveryPolicy.ownedEcho(clip.description.extras?.getBoolean(ClipboardDelivery.OWN_CLIP)==true,hash,clip.description.extras?.getString(ClipboardDelivery.OWN_HASH) ?: app.awaitPrefs().lastReceivedHash))return@withContext null
                        ManualEntries.text(app,text,"clipboard",binding)
                    }
                }
                if(row==null)EntryResult("Clipboard is empty")else {
                    completed.add(row.mediaId)
                    app.scope.launch {PostCommit.maintainSuspending {
                        withTimeoutOrNull(120000) {app.queue.work().recoverReads().first {rows->rows.none {it.mediaId==row.mediaId}}}
                        val feedback=ClipboardFeedback.result(app.queue.find(row.mediaId))
                        withContext(Dispatchers.Main) {Toast.makeText(app.context,feedback,Toast.LENGTH_SHORT).show()}
                    }}
                    EntryResult()
                }
            }catch(e:CancellationException) {throw e}catch(_:Exception) {EntryResult("PC unreachable. Not sent.")}
        }
    }
}
