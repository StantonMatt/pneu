package io.github.stantonmatt.pneu.features

import android.app.*
import android.content.*
import android.content.pm.ServiceInfo
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.protocol.randomId
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import java.util.concurrent.ConcurrentHashMap

/** Owns temporary sender grants independently of the translucent entry task.
 * Requests exist only in this process; a restart never replays source intents. */
class ShareAdmissionService:Service() {
    private val scope=CoroutineScope(SupervisorJob()+Dispatchers.Main.immediate)
    private var active=0
    private var latestStart=0
    override fun onCreate() {
        super.onCreate()
        getSystemService(NotificationManager::class.java).createNotificationChannel(NotificationChannel("status","Status",NotificationManager.IMPORTANCE_LOW))
        startForeground(4,NotificationCompat.Builder(this,"status").setSmallIcon(R.drawable.ic_stat_pneu)
            .setContentTitle("Preparing share").setOngoing(true).setSilent(true).setShowWhen(false).build(),ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
    }
    override fun onStartCommand(intent:Intent?,flags:Int,startId:Int):Int {
        latestStart=startId
        val operation=intent?.getStringExtra(OWNER)?.let {pending.remove(it)}
        if(operation!=null) {
            active++
            scope.launch {
                try {operation.result.first {it!=null}}
                finally {active--;if(active==0)stopSelfResult(latestStart)}
            }
        } else if(active==0)stopSelfResult(startId)
        // Android drops START_NOT_STICKY's delivered start item before work
        // finishes, orphaning its grant owner. Retain it until stopSelfResult
        // revokes the grants. After process death pending is empty: stop above,
        // without replaying the redelivered source intent.
        return if(operation==null)START_NOT_STICKY else START_REDELIVER_INTENT
    }
    override fun onBind(intent:Intent?):IBinder?=null
    override fun onDestroy() {scope.cancel();super.onDestroy()}
    companion object {
        private const val OWNER="pneu.share.owner"
        private val pending=ConcurrentHashMap<String,ManualEntryViewModel>()
        internal fun start(activity:Activity,operation:ManualEntryViewModel,request:ShareRequest) {
            val id=randomId();pending[id]=operation
            val owner=Intent(activity,ShareAdmissionService::class.java).putExtra(OWNER,id)
            if(request.items.isNotEmpty()) {
                owner.clipData=ClipData.newRawUri("Shared items",request.items.first()).apply {
                    request.items.drop(1).forEach {addItem(ClipData.Item(it))}
                }
                owner.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            // AMS assigns the service its own temporary grants in this synchronous
            // start call, before this Activity can lose its task and grants.
            try {ContextCompat.startForegroundService(activity,owner)}
            catch(e:Exception) {pending.remove(id);throw e}
            operation.share(request)
            // A failure to start the ongoing connection must not discard admission.
            runCatching {PneuService.start(activity)}.onFailure {android.util.Log.w("Pneu","share_warning: connection_start_failed")}
        }
    }
}
