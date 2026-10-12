package io.github.stantonmatt.pneu

import android.app.ActivityManager
import android.content.Context
import io.github.stantonmatt.pneu.features.PermissionPolicy

object ServiceEvidence {
    private fun prefs(context:Context)=context.getSharedPreferences("service_evidence",Context.MODE_PRIVATE)
    fun start(context:Context) {prefs(context).edit().putLong("started",System.currentTimeMillis()).putBoolean("running",true).commit()}
    fun stop(context:Context) {prefs(context).edit().putBoolean("running",false).commit()}
    fun acknowledge(context:Context) {
        killed(context) // include the latest observed system exit, even after a wall-clock rollback
        val p=prefs(context);p.edit().putLong("acknowledged",PermissionPolicy.acknowledgedAt(System.currentTimeMillis(),p.getLong("kill",0))).commit()
        context.getSystemService(android.app.NotificationManager::class.java).cancel(11)
    }
    fun killed(context:Context):Boolean {
        val p=prefs(context)
        if(p.getBoolean("running",false)) {
            val start=p.getLong("started",Long.MAX_VALUE)
            val previous=p.getLong("kill",0)
            val latest=context.getSystemService(ActivityManager::class.java).getHistoricalProcessExitReasons(context.packageName,0,20)
                .filter {it.timestamp>=start && PermissionPolicy.systemExit(it.reason)}.maxOfOrNull {it.timestamp} ?: 0
            if(latest>previous)p.edit().putLong("kill",latest).commit()
        }
        return p.getLong("kill",0)>p.getLong("acknowledged",0)
    }
}
