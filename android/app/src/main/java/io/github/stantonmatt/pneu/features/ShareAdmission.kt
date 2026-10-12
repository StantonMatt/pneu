package io.github.stantonmatt.pneu.features

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build

object ShareAdmission {
    fun supported(action:String?)=action==Intent.ACTION_SEND || action==Intent.ACTION_SEND_MULTIPLE
    fun publicContent(uri:Uri,packageName:String):Boolean {
        val path=uri.path.orEmpty()
        return uri.scheme=="content" && !uri.authority.isNullOrEmpty() && uri.authority!=packageName && uri.authority!="$packageName.files" &&
            uri.pathSegments.none {it==".."} && !path.startsWith("/data/") && !path.startsWith("/proc/") && !path.startsWith("/sys/")
    }
    fun granted(activity:Activity,intent:Intent,uri:Uri):Boolean {
        if(!publicContent(uri,activity.packageName) || intent.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION==0)return false
        val provider=activity.packageManager.resolveContentProvider(uri.authority!!,0)
        if(provider?.packageName==activity.packageName)return false
        // API 35's OS-recorded caller distinguishes a sender grant from our broad MediaStore permission.
        // API 34 cannot establish that proof for a normal startActivity; fail closed for URI shares.
        return Build.VERSION.SDK_INT>=35 && runCatching {activity.initialCaller.checkContentUriPermission(uri,Intent.FLAG_GRANT_READ_URI_PERMISSION)==PackageManager.PERMISSION_GRANTED}.getOrDefault(false)
    }
}
