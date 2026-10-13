package io.github.stantonmatt.pneu.features

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.util.Log
import kotlinx.coroutines.CancellationException
import java.io.IOException

object ShareAdmission {
    fun supported(action:String?)=action==Intent.ACTION_SEND || action==Intent.ACTION_SEND_MULTIPLE
    fun publicContent(uri:Uri,packageName:String)=rejection(uri.scheme,uri.authority,uri.path.orEmpty(),uri.pathSegments,packageName)==null
    internal fun rejection(scheme:String?,authority:String?,path:String,segments:List<String>,packageName:String,providerPackageName:String?=null):String?=when {
        scheme!="content" || authority.isNullOrEmpty()->"not_content"
        authority==packageName || authority=="$packageName.files" || providerPackageName==packageName->"own_provider"
        segments.any {it==".."} || path.startsWith("/data/") || path.startsWith("/proc/") || path.startsWith("/sys/")->"unsafe_content"
        else->null
    }
    fun rejection(context:Context,uri:Uri):String? {
        rejection(uri.scheme,uri.authority,uri.path.orEmpty(),uri.pathSegments,context.packageName)?.let {return it}
        val provider=context.packageManager.resolveContentProvider(uri.authority!!,0)
        return if(provider?.packageName==context.packageName)"own_provider" else null
    }
    // Never log source URIs, text, filenames or provider exception messages.
    fun warn(reason:String) {Log.w("Pneu","share_rejected: $reason")}
}

internal class ShareFailure(val reason:String,cause:Exception?=null):IOException(reason,cause)
internal suspend fun <T> shareStep(reason:String,body:suspend ()->T):T=try {body()}
catch(e:CancellationException) {throw e}
catch(e:ShareFailure) {throw e}
catch(e:BindingChangedException) {throw ShareFailure("binding_mismatch",e)}
catch(e:Exception) {throw ShareFailure(reason,e)}
