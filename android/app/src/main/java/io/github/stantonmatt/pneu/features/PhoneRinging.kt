package io.github.stantonmatt.pneu.features

import android.app.*
import android.content.*
import android.media.*
import android.os.*
import androidx.core.app.NotificationCompat
import io.github.stantonmatt.pneu.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.withLock

/** Shared alarm volume survives process death only as a restoration intent, never a ring request. */
internal class RingAudio(private val context:Context) {
    private val audio=context.getSystemService(AudioManager::class.java)
    private val journal=context.getSharedPreferences("ring-volume",Context.MODE_PRIVATE)
    private var sound:Ringtone?=null
    private var wake:PowerManager.WakeLock?=null
    private val vibrator=context.getSystemService(VibratorManager::class.java).defaultVibrator
    val playing get()=sound?.isPlaying==true
    val attributes get()=sound?.audioAttributes
    fun start() {
        check(restorePending(context)) {"Alarm volume restoration failed"}
        check(journal.edit().putInt("previous",audio.getStreamVolume(AudioManager.STREAM_ALARM)).commit())
        audio.setStreamVolume(AudioManager.STREAM_ALARM,audio.getStreamMaxVolume(AudioManager.STREAM_ALARM),0)
        val attributes=AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_ALARM).setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION).build()
        sound=requireNotNull(RingtoneManager.getRingtone(context,RingtoneManager.getDefaultUri(RingtoneManager.TYPE_ALARM))).apply {
            audioAttributes=attributes;isLooping=true;volume=1f;play()
        }
        check(playing) {"Alarm playback unavailable"}
        vibrator.vibrate(VibrationEffect.createWaveform(longArrayOf(0,700,400),0),VibrationAttributes.Builder().setUsage(VibrationAttributes.USAGE_ALARM).build())
        renewWake()
    }
    fun renewWake() {
        wake?.takeIf {it.isHeld}?.release()
        wake=context.getSystemService(PowerManager::class.java).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK,"pneu:ring").apply {acquire(31_000)}
    }
    fun stop() {
        runCatching {sound?.stop()};sound=null
        runCatching {vibrator.cancel()}
        runCatching {wake?.takeIf {it.isHeld}?.release()};wake=null
        restorePending(context)
    }
    companion object {
        fun restorePending(context:Context):Boolean=runCatching {
            context.getSystemService(NotificationManager::class.java).cancel(PhoneRinging.NOTIFICATION_ID)
            val journal=context.getSharedPreferences("ring-volume",Context.MODE_PRIVATE)
            if(journal.contains("previous")) {
                context.getSystemService(AudioManager::class.java).setStreamVolume(AudioManager.STREAM_ALARM,journal.getInt("previous",0),0)
                check(journal.edit().remove("previous").commit())
            }
            true
        }.getOrDefault(false)
    }
}

data class PhoneRingState(val ringing:Boolean=false,val computer:String="")
/** Owned by the existing connectedDevice foreground service, which already holds the live link.
 * Audio is an immediate, bounded command from that device; no background mediaPlayback FGS is started. */
class PhoneRinging(private val context:Context) {
    private val machine=RingMachine()
    private val audio=RingAudio(context)
    private val mutable=MutableStateFlow(PhoneRingState())
    val state=mutable.asStateFlow()
    private var owner:CoroutineScope?=null
    private var timer:Job?=null
    private var policy:Job?=null
    private var registered=false
    private var stopRevision=0L
    private val screen=object:BroadcastReceiver() {
        override fun onReceive(context:Context,intent:Intent) {if(intent.action==Intent.ACTION_SCREEN_OFF)stop(RingStopSource.SCREEN_OFF)}
    }
    internal val serviceAttached get()=owner!=null
    internal val playing get()=audio.playing
    internal val audioAttributes get()=audio.attributes
    fun attach(scope:CoroutineScope) {
        if(owner!=null)return
        RingAudio.restorePending(context)
        owner=scope
        context.registerReceiver(screen,IntentFilter(Intent.ACTION_SCREEN_OFF),Context.RECEIVER_NOT_EXPORTED);registered=true
        policy=scope.launch(Dispatchers.Main.immediate) {
            var bindings=context.graph.prefs.value.devices.map {Triple(it.deviceId,it.pin,it.generation)}.toSet()
            context.graph.prefs.filter {it.loaded}.collect {p->
                val next=p.devices.filter {!it.pairingLost && it.deviceId !in p.resetPeers}.map {Triple(it.deviceId,it.pin,it.generation)}.toSet()
                when {
                    !p.allowRinging->stop(RingStopSource.DISABLED)
                    p.paused->stop(RingStopSource.PAUSED)
                    (bindings-next).isNotEmpty()->stop(RingStopSource.REVOKED)
                }
                bindings=next
            }
        }
    }
    suspend fun command(start:Boolean,computer:String,allowed:()->Boolean)=withContext(Dispatchers.Main.immediate) {
        val revision=stopRevision
        context.graph.preferences.computerPolicy.withLock {
            val p=context.graph.preferences.read()
            if(start && revision!=stopRevision)return@withContext mutable.value.ringing
            if(!allowed())return@withContext mutable.value.ringing
            if(!start)stop(RingStopSource.COMPUTER)
            else if(owner!=null && p.allowRinging && !p.paused) {
                try {
                    if(!machine.ringing)audio.start() else audio.renewWake()
                    machine.start(SystemClock.elapsedRealtime())
                    showNotification(computer)
                    mutable.value=PhoneRingState(true,computer)
                    timer?.cancel()
                    timer=owner!!.launch(Dispatchers.Main.immediate) {
                        while(isActive) {
                            val remaining=(machine.deadline ?: break)-SystemClock.elapsedRealtime()
                            if(remaining<=0) {if(machine.expire(SystemClock.elapsedRealtime()))finishStop();break}
                            delay(remaining)
                        }
                    }
                } catch(e:CancellationException) {stop(RingStopSource.FAILURE);throw e}
                catch(_:Exception) {stop(RingStopSource.FAILURE);audio.stop()}
            }
            mutable.value.ringing
        }
    }
    /** UI, receiver and Service lifecycle all run on Main. */
    fun stop(source:RingStopSource) {
        stopRevision++;machine.stop(source);finishStop()
    }
    private fun finishStop() {
        timer?.cancel();timer=null;audio.stop()
        context.getSystemService(NotificationManager::class.java).cancel(NOTIFICATION_ID)
        mutable.value=PhoneRingState()
    }
    fun detach() {
        stop(RingStopSource.SERVICE_STOP)
        policy?.cancel();policy=null;owner=null
        if(registered) {context.unregisterReceiver(screen);registered=false}
    }
    private fun showNotification(computer:String) {
        val manager=context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(NotificationChannel(CHANNEL,context.getString(R.string.channel_ringing),NotificationManager.IMPORTANCE_HIGH).apply {
            setSound(null,null);enableVibration(false);setShowBadge(false)
        })
        val stop=PendingIntent.getBroadcast(context,0,Intent(context,RingStopReceiver::class.java),PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val full=PendingIntent.getActivity(context,0,Intent(context,RingingActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        manager.notify(NOTIFICATION_ID,NotificationCompat.Builder(context,CHANNEL)
            .setSmallIcon(R.drawable.ic_ring).setContentTitle(context.getString(R.string.ring_title))
            .setContentText(context.getString(R.string.ring_from,computer)).setCategory(NotificationCompat.CATEGORY_ALARM)
            .setPriority(NotificationCompat.PRIORITY_MAX).setOngoing(true).setShowWhen(false)
            .setContentIntent(full).addAction(R.drawable.ic_stop,context.getString(R.string.ring_stop),stop)
            .apply {if(manager.canUseFullScreenIntent())setFullScreenIntent(full,true)}.build())
    }
    companion object {const val CHANNEL="ringing";const val NOTIFICATION_ID=70}
}
class RingStopReceiver:BroadcastReceiver() {
    override fun onReceive(context:Context,intent:Intent) {context.graph.ringing.stop(RingStopSource.NOTIFICATION)}
}
