package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.ClipDescription
import android.net.Uri
import kotlinx.coroutines.CancellationException
import android.content.ClipboardManager
import androidx.compose.runtime.*
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalWindowInfo
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.Prefs

private data class ClipboardIdentity(val uri:Uri,val timestamp:Long,val label:String?)
private data class ClipboardMime(val type:String?)

/** Android permits foreground clipboard reads. Refresh on focus/resume and changes. */
@Composable internal fun rememberClipboardAvailable(p:Prefs,links:Map<String,LinkState>):Boolean {
    val context=LocalContext.current
    val lifecycle=LocalLifecycleOwner.current.lifecycle
    val clipboard=remember {context.getSystemService(ClipboardManager::class.java)}
    var clip by remember {mutableStateOf(runCatching {clipboard.primaryClip}.getOrNull())}
    val focused=LocalWindowInfo.current.isWindowFocused
    LaunchedEffect(focused) {if(focused)clip=runCatching {clipboard.primaryClip}.getOrNull()}
    DisposableEffect(clipboard,lifecycle) {
        fun refresh() {clip=runCatching {clipboard.primaryClip}.getOrNull()}
        val listener=ClipboardManager.OnPrimaryClipChangedListener {refresh()}
        val observer=LifecycleEventObserver {_,event->if(event==Lifecycle.Event.ON_RESUME)refresh()}
        clipboard.addPrimaryClipChangedListener(listener);lifecycle.addObserver(observer)
        refresh()
        onDispose {clipboard.removePrimaryClipChangedListener(listener);lifecycle.removeObserver(observer)}
    }
    return clipboardAvailable(clip,p,links) {context.contentResolver.getType(it)}
}

/** Provider work belongs to the clipboard identity. Progress, settings and peer
 * eligibility can change while it runs, without cancelling or repeating it. */
@Composable internal fun clipboardAvailable(clip:ClipData?,p:Prefs,links:Map<String,LinkState>,resolve:(Uri)->String?):Boolean {
    val uri=clip?.takeIf {it.itemCount>0}?.getItemAt(0)?.uri
    val identity=uri?.let {ClipboardIdentity(it,requireNotNull(clip).description.timestamp,clip.description.label?.toString())}
    val sensitive=clip?.description?.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE)==true
    val mime by key(identity,sensitive) {
        produceState<ClipboardMime?>(null) {
            if(!sensitive)value=try {ClipboardMime(uri?.let {source->ProviderReads.call {resolve(source)}})}
                catch(e:CancellationException) {throw e}
                catch(_:Exception) {null}
        }
    }
    return !sensitive && mime!=null && ClipboardSendPolicy.available(clip,p,links,mime?.type)
}
