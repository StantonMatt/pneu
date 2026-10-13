package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R

object PermissionPolicy {
    fun settings(asked:Boolean,rationale:Boolean)=asked && !rationale
    fun acknowledgedAt(now:Long,evidence:Long)=maxOf(now,evidence)
    fun photoAction(limited:Boolean,asked:Boolean,rationale:Boolean)=when {limited->R.string.action_allow_all;settings(asked,rationale)->R.string.action_settings;else->R.string.action_allow}
    fun batterySettings(backgroundRestricted:Boolean)=backgroundRestricted
    // One UI hides battery-exempt apps from its Never sleeping apps picker.
    // Keep kill evidence separate: an exemption satisfies this setting, not proof of survival.
    fun sleeping(acknowledged:Boolean,bucketAllowed:Boolean,killed:Boolean,batteryExempt:Boolean)=batteryExempt || (acknowledged && bucketAllowed && !killed)
    fun cameraAction(asked:Boolean,rationale:Boolean)=if(settings(asked,rationale))R.string.action_open_settings else R.string.pair_allow_camera
    fun systemExit(reason:Int)=reason in setOf(2,3,4,5,6,7,9,12,13,14,17,18) // System exits; exclude deliberate user/self stops, package updates and permission changes.
}
