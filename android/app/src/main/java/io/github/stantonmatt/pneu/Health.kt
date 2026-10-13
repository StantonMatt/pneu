package io.github.stantonmatt.pneu

import io.github.stantonmatt.pneu.features.PermissionPolicy
import android.Manifest
import android.app.*
import android.app.usage.UsageStatsManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.PowerManager
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat

/** These checks also run after returning from OEM settings. Samsung does not expose its sleeping list. */
data class Health(val photos: Boolean, val notifications: Boolean, val battery: Boolean, val sleeping: Boolean, val killed: Boolean = false, val channelBlocked:Boolean=false,val backgroundRestricted:Boolean=false,val localNetwork:Boolean=true) {
    val issues: List<String> get() = buildList {
        if(!localNetwork) add("Pneu can't connect to your PC")
        if(!photos) add("Pneu can't see new screenshots")
        if(killed) add("Pneu was stopped in the background")
        if(!battery) add("Battery optimization can stop Pneu")
        if(!sleeping) add("Pneu isn't in Never sleeping apps")
        if(!notifications) add("Notifications are off")
    }
}
fun Context.granted(permission: String) = ContextCompat.checkSelfPermission(this,permission) == PackageManager.PERMISSION_GRANTED
fun Context.health(p:io.github.stantonmatt.pneu.data.Prefs=graph.prefs.value): Health {
    check(p.loaded) {"Settings not loaded"}
    val photos=granted(Manifest.permission.READ_MEDIA_IMAGES) && (!p.recordings || granted(Manifest.permission.READ_MEDIA_VIDEO))
    val nm=getSystemService(NotificationManager::class.java)
    val notifications=NotificationManagerCompat.from(this).areNotificationsEnabled() && nm.getNotificationChannel("status")?.importance != NotificationManager.IMPORTANCE_NONE
    val am=getSystemService(ActivityManager::class.java)
    val batteryExempt=getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(packageName)
    val battery=batteryExempt && !am.isBackgroundRestricted
    val bucket=getSystemService(UsageStatsManager::class.java).appStandbyBucket
    val killed=p.peer!=null && ServiceEvidence.killed(this)
    if(killed && p.neverSleeping)graph.invalidateSleeping()
    val sleeping=!Build.MANUFACTURER.equals("samsung",true) || PermissionPolicy.sleeping(p.neverSleeping,bucket<UsageStatsManager.STANDBY_BUCKET_RARE,killed,batteryExempt)
    return Health(photos,notifications,battery,sleeping,killed,nm.getNotificationChannel("status")?.importance==NotificationManager.IMPORTANCE_NONE,am.isBackgroundRestricted,Build.VERSION.SDK_INT<37 || granted(Manifest.permission.ACCESS_LOCAL_NETWORK))
}
