package io.github.stantonmatt.pneu

import android.app.*
import android.content.*
import android.content.pm.ServiceInfo
import android.graphics.Color
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import io.github.stantonmatt.pneu.features.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

/** connectedDevice: an ongoing authenticated network link; CHANGE_NETWORK_STATE is its prerequisite. */
class PneuService : Service() {
    private val alertThrottle=AlertThrottle(System::currentTimeMillis,android.os.SystemClock::elapsedRealtime)
    private val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
    private var observer: ScreenshotObserver?=null
    private var initialization: ServiceInitialization?=null
    @Volatile private var latestStart=0
    @Volatile private var lastNotification:Notification?=null
    private var workStarted=false
    override fun onCreate() {
        super.onCreate()
        val nm=getSystemService(NotificationManager::class.java)
        StatusNotificationChannel.create(this)
        nm.createNotificationChannel(NotificationChannel("alerts",getString(R.string.channel_alerts),NotificationManager.IMPORTANCE_DEFAULT))
        enterForeground()
    }
    // Never consult graph, preferences, identity or Room on the promotion path.
    private fun startingNotification()=NotificationCompat.Builder(this,StatusNotificationChannel.id(this))
        .setSmallIcon(R.drawable.ic_stat_pneu).setColor(0xFF4F5BFF.toInt())
        .setContentTitle(getString(R.string.notif_starting)).setContentIntent(activityIntent(0,MainActivity::class.java))
        .setOngoing(true).setOnlyAlertOnce(true).setShowWhen(false).setSilent(true)
        .setCategory(NotificationCompat.CATEGORY_SERVICE)
        .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE).build()
    private fun enterForeground():Boolean {
        val channel=StatusNotificationChannel.ensure(this) ?: return false
        // A recovered channel may differ from the last notification's channel.
        // Promotion still uses the graph-free startup notification in that case.
        val ready=lastNotification?.takeIf {it.channelId==channel} ?: startingNotification()
        startForeground(1,ready,ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        return true
    }
    private fun updateNotification(nm:NotificationManager) {
        if(StatusNotificationChannel.ensure(this)==null)return
        val ready=notification()
        lastNotification=ready
        nm.notify(1,ready)
    }
    private suspend fun stopForCommand(startId:Int)=withContext(Dispatchers.Main.immediate) {
        // An older idle snapshot must not cancel a newer, possibly queued,
        // foreground start. AMS checks its latest start ID, not just our copy.
        startId>0 && stopSelfResult(startId)
    }
    private fun startWork() {
        workStarted=true
        val nm=getSystemService(NotificationManager::class.java)
        observer=ScreenshotObserver(graph)
        initialization=ServiceInitialization(scope,attempt={
            while(currentCoroutineContext().isActive) {
                val command=latestStart
                val p=graph.preferences.read()
                if((p.peer!=null || p.pending!=null) && !p.pairingLost) {
                    observer?.start(scope);graph.connection.start(scope);break
                }
                if(stopForCommand(command))break
                // A new command was queued while the old read was in flight.
                delay(10)
            }
        },failure={
            graph.connection.state.value=io.github.stantonmatt.pneu.connection.LinkState("Storage unavailable")
            updateNotification(nm)
        })
        scope.launch {
            combine(graph.prefs,graph.connection.state,graph.waitingCount) { p,s,r -> Triple(p,s,r) }.collect { (p,s,_) ->
                val command=latestStart
                if(!p.loaded) {updateNotification(nm);return@collect}
                if(p.pairingLost) {
                    val times=getSharedPreferences("health_alerts",MODE_PRIVATE)
                    if(alertThrottle.eligible("pairing",times.getLong("pairing",0),MainActivity.visible)) {
                        nm.notify(2,NotificationCompat.Builder(this@PneuService,"alerts").setSmallIcon(R.drawable.ic_stat_pneu).setColor(0xFF4F5BFF.toInt())
                            .setContentTitle(getString(R.string.status_pairing_lost)).setContentText(getString(R.string.detail_pairing_lost,p.lostPcName.ifEmpty {p.peer?.name.orEmpty()}))
                            .setContentIntent(activityIntent(3,MainActivity::class.java)).setAutoCancel(true)
                            .addAction(0,getString(R.string.action_pair_again),activityIntent(3,MainActivity::class.java)).build())
                        times.edit().putLong("pairing",alertThrottle.shown("pairing")).apply()
                    }
                    if(p==graph.prefs.value)stopForCommand(command)
                } else if(p.peer == null && p.pending == null && p.deviceId.isNotEmpty()) {
                    if(p==graph.prefs.value)stopForCommand(command)
                } else updateNotification(nm)
            }
        }
        scope.launch {graph.initialized.await();graph.prefs.first {it.deviceId.isNotEmpty()};PostCommit.maintain {health();ServiceEvidence.start(this@PneuService)}; while(isActive) { PostCommit.maintainSuspending {checkHealthAlerts()}; delay(86_400_000) } }
    }
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Every command can carry a fresh foreground deadline, including a
        // sticky null intent, notification action, retry, or queued stop.
        if(!enterForeground()) {stopSelfResult(startId);return START_NOT_STICKY}
        runCatching {onForegroundCommand(this,startId)}
        latestStart=startId
        if(intent?.action==ACTION_STOP) {
            stopSelfResult(startId)
            return START_NOT_STICKY
        }
        if(!workStarted)startWork() else initialization?.kick()
        // Initialization may already have succeeded on an earlier command.
        scope.launch {PostCommit.maintainSuspending {
            val p=graph.preferences.read()
            if((p.peer==null && p.pending==null) || p.pairingLost)stopForCommand(startId)
        }}
        scope.launch { PostCommit.maintainSuspending {checkHealthAlerts()} }
        if(intent?.action == ACTION_PAUSE) scope.launch {io.github.stantonmatt.pneu.features.UiActions.feedback(this@PneuService).run(getString(R.string.toast_pause)) {graph.preferences.toggle("paused",!graph.awaitPrefs().paused)}}
        return START_STICKY
    }
    private fun activityIntent(code: Int, activity: Class<*>) = PendingIntent.getActivity(this,code,Intent(this,activity).apply { if(code==0)putExtra("home",true);if(code == 3) putExtra("pair",true); if(code >= 10) putExtra("permissions",true) },PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
    private fun notification(): Notification {
        val p=graph.prefs.value; val s=graph.connection.state.value
        if(!p.loaded)return NotificationCompat.Builder(this,StatusNotificationChannel.id(this)).setSmallIcon(R.drawable.ic_stat_pneu).setColor(0xFF4F5BFF.toInt())
            .setContentTitle(if(s.status=="Storage unavailable")getString(R.string.load_failed) else getString(R.string.load_pending))
            .setContentIntent(activityIntent(0,MainActivity::class.java)).setOngoing(true).setOnlyAlertOnce(true).setShowWhen(false).setSilent(true).build()
        val title=if(p.pairingLost) "Pairing lost" else if(p.paused) "Paused" else UiPresentation.sending(this,s) ?: s.status
        val count=graph.waitingCount.value
        val detail=when {
            p.pairingLost -> getString(R.string.detail_pairing_lost,p.lostPcName)
            p.paused -> getString(R.string.detail_paused)
            s.transferMediaId.isNotEmpty() -> getString(R.string.notif_progress,android.text.format.Formatter.formatShortFileSize(this,s.sent),android.text.format.Formatter.formatShortFileSize(this,s.total))
            s.status == "Connected" -> getString(R.string.detail_route,p.peer?.name.orEmpty(),UiPresentation.status(this,s.route))
            s.status == "Connecting…" -> p.pending?.name ?: p.peer?.name.orEmpty()
            count > 0 -> resources.getQuantityString(R.plurals.waiting,count,count)
            s.status == "Waiting for Wi-Fi" -> getString(R.string.detail_mobile_data_off)
            else -> if(p.lastConnected>0) getString(R.string.detail_last_connected,lastConnectedLabel(this,p.lastConnected)) else null
        }
        val pause=PendingIntent.getService(this,2,Intent(this,PneuService::class.java).setAction(ACTION_PAUSE),PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        return NotificationCompat.Builder(this,StatusNotificationChannel.id(this)).setSmallIcon(R.drawable.ic_stat_pneu).setColor(0xFF4F5BFF.toInt())
            .setContentTitle(UiPresentation.status(this,title)).setContentText(detail).setContentIntent(activityIntent(0,MainActivity::class.java))
            .setOngoing(true).setOnlyAlertOnce(true).setShowWhen(false).setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE).setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .apply {
                if(p.pairingLost)addAction(0,getString(R.string.action_pair_again),activityIntent(3,MainActivity::class.java))
                else {
                    if(p.clipboardButton) addAction(0,getString(R.string.tile_send_clipboard),PendingIntent.getActivity(this@PneuService,1,ManualEntries.clipboardIntent(this@PneuService,p),PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE))
                    addAction(0,if(p.paused) getString(R.string.action_resume) else getString(R.string.action_pause),pause)
                    if(s.total > 0) setProgress(100,(s.sent*100/s.total).toInt(),false)
                }
            }.build()
    }
    private suspend fun checkHealthAlerts() {
        val p=graph.awaitPrefs(); val h=health(p)
        if(!h.sleeping && p.neverSleeping) graph.invalidateSleeping()
        val nm=getSystemService(NotificationManager::class.java)
        val alerts=listOf(
            Triple(!h.photos,getString(R.string.alert_photos_title),getString(R.string.alert_photos_text)),
            Triple(h.killed,getString(R.string.alert_killed_title),if(android.os.Build.MANUFACTURER.equals("samsung",true) && !h.sleeping)getString(R.string.alert_killed_text) else getString(R.string.alert_killed_generic_text)),
            Triple(!h.battery,getString(R.string.alert_battery_title),getString(R.string.alert_battery_text)),
            Triple(!h.localNetwork,getString(R.string.banner_local_network),getString(R.string.alert_local_network_text))
        )
        val times=getSharedPreferences("health_alerts",MODE_PRIVATE)
        alerts.forEachIndexed { i,(active,title,text) ->
            val key="alert_$i"
            if(!active) nm.cancel(10+i)
            else if(alertThrottle.eligible(key,times.getLong(key,0),MainActivity.visible)) {
                nm.notify(10+i,NotificationCompat.Builder(this,"alerts").setSmallIcon(R.drawable.ic_stat_pneu).setColor(0xFF4F5BFF.toInt()).setContentTitle(UiPresentation.status(this,title)).setContentText(text)
                    .setContentIntent(activityIntent(10+i,MainActivity::class.java)).addAction(0,getString(R.string.action_fix),activityIntent(10+i,MainActivity::class.java)).build())
                times.edit().putLong(key,alertThrottle.shown(key)).apply()
            }
        }
    }
    override fun onDestroy() {scope.cancel();if(workStarted) {ServiceEvidence.stop(this);observer?.stop();graph.connection.stop()};super.onDestroy() }
    override fun onBind(intent: Intent?): IBinder? = null
    companion object {
        const val ACTION_PAUSE="io.github.stantonmatt.pneu.PAUSE"
        private const val ACTION_STOP="io.github.stantonmatt.pneu.STOP"
        @Volatile internal var onForegroundCommand:(PneuService,Int)->Unit={_,_->}
        fun start(context: Context) { ContextCompat.startForegroundService(context,Intent(context,PneuService::class.java)) }
        /** Deliver shutdown after any queued start, rather than cancelling that
         * start before Android can acknowledge its foreground requirement. */
        fun stop(context:Context) {context.startService(Intent(context,PneuService::class.java).setAction(ACTION_STOP))}
    }
}
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if(intent.action !in setOf(Intent.ACTION_BOOT_COMPLETED,Intent.ACTION_MY_PACKAGE_REPLACED))return
        try {PneuService.start(context)}catch(_:IllegalStateException) {}catch(_:SecurityException) {}

    }
}
