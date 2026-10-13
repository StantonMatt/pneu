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
    private lateinit var observer: ScreenshotObserver
    private lateinit var initialization: ServiceInitialization
    override fun onCreate() {
        super.onCreate()
        val nm=getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel("status","Status",NotificationManager.IMPORTANCE_LOW))
        nm.createNotificationChannel(NotificationChannel("alerts","Alerts",NotificationManager.IMPORTANCE_DEFAULT))
        startForeground(1,notification(),ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        observer=ScreenshotObserver(graph)
        initialization=ServiceInitialization(scope,attempt={
            val p=graph.preferences.read()
            if(p.peer==null && p.pending==null)stopSelf()
            else {observer.start(scope);graph.connection.start(scope)}
        },failure={
            graph.connection.state.value=io.github.stantonmatt.pneu.connection.LinkState("Storage unavailable")
            nm.notify(1,notification())
        })
        scope.launch {
            combine(graph.prefs,graph.connection.state,graph.waitingCount) { p,s,r -> Triple(p,s,r) }.collect { (p,s,_) ->
                if(!p.loaded) {nm.notify(1,notification());return@collect}
                if(p.pairingLost) {
                    val times=getSharedPreferences("health_alerts",MODE_PRIVATE)
                    if(alertThrottle.eligible("pairing",times.getLong("pairing",0),MainActivity.visible)) {
                        nm.notify(2,NotificationCompat.Builder(this@PneuService,"alerts").setSmallIcon(R.drawable.ic_stat_pneu)
                            .setContentTitle("Pairing lost").setContentText("${p.lostPcName.ifEmpty {p.peer?.name.orEmpty()}} doesn't recognize this phone")
                            .setContentIntent(activityIntent(3,MainActivity::class.java)).setAutoCancel(true)
                            .addAction(0,"Pair again",activityIntent(3,MainActivity::class.java)).build())
                        times.edit().putLong("pairing",alertThrottle.shown("pairing")).apply()
                    }
                    stopSelf()
                } else if(p.peer == null && p.pending == null && p.deviceId.isNotEmpty()) stopSelf() else nm.notify(1,notification())
            }
        }
        scope.launch {graph.initialized.await();graph.prefs.first {it.deviceId.isNotEmpty()};PostCommit.maintain {health();ServiceEvidence.start(this@PneuService)}; while(isActive) { PostCommit.maintainSuspending {checkHealthAlerts()}; delay(86_400_000) } }
    }
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        initialization.kick()
        scope.launch { PostCommit.maintainSuspending {checkHealthAlerts()} }
        if(intent?.action == ACTION_PAUSE) scope.launch {io.github.stantonmatt.pneu.features.UiActions.feedback(this@PneuService).run("Couldn't change pause. Open Pneu and try again.") {graph.preferences.toggle("paused",!graph.awaitPrefs().paused)}}
        return START_STICKY
    }
    private fun activityIntent(code: Int, activity: Class<*>) = PendingIntent.getActivity(this,code,Intent(this,activity).apply { if(code==0)putExtra("home",true);if(code == 3) putExtra("pair",true); if(code >= 10) putExtra("permissions",true) },PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
    private fun notification(): Notification {
        val p=graph.prefs.value; val s=graph.connection.state.value
        if(!p.loaded)return NotificationCompat.Builder(this,"status").setSmallIcon(R.drawable.ic_stat_pneu)
            .setContentTitle(if(s.status=="Storage unavailable")"Couldn't load Pneu settings" else "Loading Pneu settings")
            .setContentIntent(activityIntent(0,MainActivity::class.java)).setOngoing(true).setOnlyAlertOnce(true).setShowWhen(false).setSilent(true).build()
        val title=if(p.pairingLost) "Pairing lost" else if(p.paused) "Paused" else s.sendingTitle() ?: s.status
        val count=graph.waitingCount.value
        val detail=when {
            p.pairingLost -> "${p.lostPcName} doesn't recognize this phone"
            p.paused -> "Automatic sending is off"
            s.transferMediaId.isNotEmpty() -> "${android.text.format.Formatter.formatShortFileSize(this,s.sent)} of ${android.text.format.Formatter.formatShortFileSize(this,s.total)}"
            s.status == "Connected" -> "${p.peer?.name.orEmpty()} · ${s.route}"
            s.status == "Connecting…" -> p.pending?.name ?: p.peer?.name.orEmpty()
            count > 0 -> resources.getQuantityString(R.plurals.waiting,count,count)
            s.status == "Waiting for Wi-Fi" -> "Mobile data is off in settings"
            else -> if(p.lastConnected>0) "Last connected ${lastConnectedLabel(this,p.lastConnected)}" else null
        }
        val pause=PendingIntent.getService(this,2,Intent(this,PneuService::class.java).setAction(ACTION_PAUSE),PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        return NotificationCompat.Builder(this,"status").setSmallIcon(R.drawable.ic_stat_pneu).setColor(Color.rgb(15,110,104))
            .setContentTitle(title).setContentText(detail).setContentIntent(activityIntent(0,MainActivity::class.java))
            .setOngoing(true).setOnlyAlertOnce(true).setShowWhen(false).setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE).setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .apply {
                if(p.pairingLost)addAction(0,"Pair again",activityIntent(3,MainActivity::class.java))
                else {
                    if(p.clipboardButton) addAction(0,"Send clipboard",PendingIntent.getActivity(this@PneuService,1,ManualEntries.clipboardIntent(this@PneuService,p),PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE))
                    addAction(0,if(p.paused) "Resume" else "Pause",pause)
                    if(s.total > 0) setProgress(100,(s.sent*100/s.total).toInt(),false)
                }
            }.build()
    }
    private suspend fun checkHealthAlerts() {
        val p=graph.awaitPrefs(); val h=health(p)
        if(!h.sleeping && p.neverSleeping) graph.invalidateSleeping()
        val nm=getSystemService(NotificationManager::class.java)
        val alerts=listOf(
            Triple(!h.photos,"Pneu can't see new screenshots","Photo access was turned off"),
            Triple(h.killed,"Pneu was stopped in the background",if(android.os.Build.MANUFACTURER.equals("samsung",true) && !h.sleeping)"Add Pneu to Never sleeping apps" else "Check permissions and battery settings"),
            Triple(!h.battery,"Pneu may stop in the background","Battery use is set to Optimized"),
            Triple(!h.localNetwork,"Pneu can't connect to your PC","Local network access is off")
        )
        val times=getSharedPreferences("health_alerts",MODE_PRIVATE)
        alerts.forEachIndexed { i,(active,title,text) ->
            val key="alert_$i"
            if(!active) nm.cancel(10+i)
            else if(alertThrottle.eligible(key,times.getLong(key,0),MainActivity.visible)) {
                nm.notify(10+i,NotificationCompat.Builder(this,"alerts").setSmallIcon(R.drawable.ic_stat_pneu).setContentTitle(title).setContentText(text)
                    .setContentIntent(activityIntent(10+i,MainActivity::class.java)).addAction(0,"Fix",activityIntent(10+i,MainActivity::class.java)).build())
                times.edit().putLong(key,alertThrottle.shown(key)).apply()
            }
        }
    }
    override fun onDestroy() {ServiceEvidence.stop(this); observer.stop(); graph.connection.stop(); scope.cancel(); super.onDestroy() }
    override fun onBind(intent: Intent?): IBinder? = null
    companion object {
        const val ACTION_PAUSE="io.github.stantonmatt.pneu.PAUSE"
        fun start(context: Context) { ContextCompat.startForegroundService(context,Intent(context,PneuService::class.java)) }
    }
}
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if(intent.action !in setOf(Intent.ACTION_BOOT_COMPLETED,Intent.ACTION_MY_PACKAGE_REPLACED))return
        try {PneuService.start(context)}catch(_:IllegalStateException) {}catch(_:SecurityException) {}

    }
}
