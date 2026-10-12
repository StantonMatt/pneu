package io.github.stantonmatt.pneu.features

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.Settings

/** Shared, durable request history: rationale=false alone also means never requested. */
object PermissionRequests {
    private fun prefs(context:Context)=context.getSharedPreferences("permission_attempts",Context.MODE_PRIVATE)
    fun asked(context:Context,permission:String):Boolean {
        val legacy=when(permission) {
            Manifest.permission.POST_NOTIFICATIONS->"notificationsAsked"
            Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO->"photosAsked"
            Manifest.permission.CAMERA->"cameraAsked"
            else->"localNetworkAsked"
        }
        return prefs(context).getBoolean(permission,false) || prefs(context).getBoolean(legacy,false)
    }
    fun mark(context:Context,vararg permissions:String) {prefs(context).edit().apply {permissions.forEach {putBoolean(it,true)}}.apply()}
    fun settings(activity:Activity,permission:String)=PermissionPolicy.settings(asked(activity,permission),activity.shouldShowRequestPermissionRationale(permission))
    fun openSettings(context:Context) {context.startActivity(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,Uri.parse("package:${context.packageName}")))}
}
