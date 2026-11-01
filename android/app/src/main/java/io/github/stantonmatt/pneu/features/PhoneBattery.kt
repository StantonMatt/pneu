package io.github.stantonmatt.pneu.features

import android.content.*
import android.os.BatteryManager
import io.github.stantonmatt.pneu.protocol.BatteryState
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.callbackFlow

/** Sticky sample is read again for every new session; nothing is persisted. */
internal fun phoneBattery(context:Context)=callbackFlow {
    val receiver=object:BroadcastReceiver() {
        override fun onReceive(context:Context,intent:Intent) {trySend(batterySample(intent))}
    }
    context.registerReceiver(receiver,IntentFilter(Intent.ACTION_BATTERY_CHANGED),Context.RECEIVER_NOT_EXPORTED)
        ?.let {trySend(batterySample(it))}
    awaitClose {context.unregisterReceiver(receiver)}
}
internal fun batterySample(intent:Intent):BatteryState {
    val level=intent.getIntExtra(BatteryManager.EXTRA_LEVEL,-1)
    val scale=intent.getIntExtra(BatteryManager.EXTRA_SCALE,-1)
    if(level<0 || scale<=0 || level>scale)return BatteryState()
    val status=intent.getIntExtra(BatteryManager.EXTRA_STATUS,-1)
    if(status !in setOf(BatteryManager.BATTERY_STATUS_CHARGING,BatteryManager.BATTERY_STATUS_FULL,
            BatteryManager.BATTERY_STATUS_DISCHARGING,BatteryManager.BATTERY_STATUS_NOT_CHARGING))return BatteryState()
    return BatteryState((level.toLong()*100/scale).toInt(),intent.getIntExtra(BatteryManager.EXTRA_PLUGGED,0)!=0 &&
        status in setOf(BatteryManager.BATTERY_STATUS_CHARGING,BatteryManager.BATTERY_STATUS_FULL))
}
