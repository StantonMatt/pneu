package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/** Only the receiving phone advertises ring willingness. Keep the legacy list in order. */
fun phoneComputerCapabilities(allowRinging:Boolean):List<String> =
    LEGACY_PHONE_CAPABILITIES + CAP_FILE_RESUME + if(allowRinging)listOf(CAP_RING) else emptyList()

/** One RAM-only coalescer per authenticated connection. Clear bypasses the sample throttle. */
internal class BatteryCoalescer {
    private var latest=BatteryState()
    private var sent:BatteryState?=null
    private var sentAt:Long?=null
    fun offer(value:BatteryState) {latest=value}
    fun remaining(now:Long):Long {
        val at=sentAt ?: return 0
        if(latest!=sent)return if(latest.level==null)0 else (at+1000-now).coerceAtLeast(0)
        return if(latest.level==null)Long.MAX_VALUE else (at+300_000-now).coerceAtLeast(0)
    }
    fun take(now:Long):BatteryState? {
        if(remaining(now)>0)return null
        return latest.also {sent=it;sentAt=now}
    }
}
internal suspend fun publishBattery(source:Flow<BatteryState>,now:()->Long,send:suspend (BatteryState)->Unit)=coroutineScope {
    val input=Channel<BatteryState>(Channel.CONFLATED)
    val producer=launch {source.collect {input.send(it)}}
    val coalescer=BatteryCoalescer()
    try {
        coalescer.offer(input.receive())
        while(true) {
            // Drain changes before choosing a sample, including a policy clear.
            while(true) {val value=input.tryReceive().getOrNull() ?: break;coalescer.offer(value)}
            coalescer.take(now())?.let {send(it)}
            val wait=coalescer.remaining(now())
            val value=if(wait==Long.MAX_VALUE)input.receive() else withTimeoutOrNull(wait.coerceAtLeast(1)) {input.receive()}
            if(value!=null)coalescer.offer(value)
        }
    } finally {producer.cancel();input.cancel()}
}

enum class RingStopSource { SURFACE, NOTIFICATION, SCREEN_OFF, COMPUTER, TIMEOUT, DISABLED, PAUSED, REVOKED, SERVICE_STOP, FAILURE }
internal class RingMachine {
    var deadline:Long?=null;private set
    val ringing get()=deadline!=null
    fun start(now:Long):Boolean {val changed=!ringing;deadline=now+30_000;return changed}
    fun stop(@Suppress("UNUSED_PARAMETER") source:RingStopSource):Boolean {val changed=ringing;deadline=null;return changed}
    fun expire(now:Long):Boolean = if(deadline?.let {now>=it}==true)stop(RingStopSource.TIMEOUT) else false
}
