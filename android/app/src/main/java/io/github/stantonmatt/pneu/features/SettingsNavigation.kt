package io.github.stantonmatt.pneu.features

import android.content.ComponentName
import android.content.Intent
import android.net.Uri
import android.provider.Settings

object SettingsNavigation {
    /** A resolved OEM activity may still reject the launch (for example, if not exported). */
    internal fun <T> launchFirstAvailable(candidates:List<T>,launch:(T)->Unit):Boolean =
        candidates.any {runCatching {launch(it)}.isSuccess}

    fun openSamsungBattery(packageName:String,launch:(Intent)->Unit):Boolean = launchFirstAvailable(listOf(
        Intent().setComponent(ComponentName("com.samsung.android.lool","com.samsung.android.sm.battery.ui.BatteryNeverSleepingAppsActivity")),
        Intent().setComponent(ComponentName("com.samsung.android.lool","com.samsung.android.sm.battery.ui.BatteryActivity")),
        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,Uri.parse("package:$packageName"))
    ),launch)
}
