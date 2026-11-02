package io.github.stantonmatt.pneu.features

import android.app.Notification
import android.app.NotificationManager
import android.content.ComponentName
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.Process
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import io.github.stantonmatt.pneu.graph
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

fun Context.notificationAccess()=getSystemService(NotificationManager::class.java)
    .isNotificationListenerAccessGranted(ComponentName(this,PhoneNotificationListener::class.java))

/** Shared RAM snapshot. Only the listener owns Android keys; only the session mapper exposes wire IDs. */
class PhoneNotifications {
    private val active=linkedMapOf<String,MirrorNotice>()
    private var postSequence=0L
    @Volatile var connected=false
        private set
    @Synchronized fun snapshot()=if(connected)active.values.toList() else emptyList()
    @Synchronized fun postPosition()=postSequence
    @Synchronized fun replace(notices:List<MirrorNotice>) {
        // Refreshes discover eligibility/content, but only a live post advances the connection fence.
        val current=notices.sortedWith(compareByDescending<MirrorNotice> {it.time}.thenBy {it.localKey}).take(128)
            .map {it.copy(postSequence=active[it.localKey]?.postSequence ?: 0)}
        active.clear();current.forEach {active[it.localKey]=it};connected=true
    }
    @Synchronized fun post(notice:MirrorNotice) {
        // Snapshot presence is not delivery. Only changed visible content is a live event;
        // timestamp-only reposts must not cross a reconnect/consent fence.
        val previous=active[notice.localKey]
        if(previous?.copy(time=notice.time,postSequence=notice.postSequence)==notice)return
        active[notice.localKey]=notice.copy(postSequence=++postSequence)
        if(active.size>128)active.values.minWithOrNull(compareBy<MirrorNotice> {it.time}.thenByDescending {it.localKey})?.let {active.remove(it.localKey)}
    }
    @Synchronized fun remove(key:String) {active.remove(key)}
    @Synchronized fun disconnected() {connected=false;active.clear()}
    val rate=NotificationRateLimit(android.os.SystemClock::elapsedRealtime)

}

class PhoneNotificationListener:NotificationListenerService() {
    private val app get()=graph
    private val worker=CoroutineScope(SupervisorJob()+Dispatchers.Default)
    private var isConnected=false
    private var snapshots:Job?=null
    override fun onListenerConnected() {
        isConnected=true;snapshots?.cancel()
        snapshots=worker.launch {
            app.initialized.await()
            app.prefs.map {it.notificationConsent}.distinctUntilChanged().collect {consent->withContext(Dispatchers.Main) {
                if(isConnected)refresh(consent)
            }}
        }
    }
    override fun onListenerDisconnected() {isConnected=false;snapshots?.cancel();app.phoneNotifications.disconnected()}
    override fun onDestroy() {app.phoneNotifications.disconnected();worker.cancel();super.onDestroy()}
    private fun refresh(consent:NotificationConsent,exclude:String?=null,preserveExisting:Boolean=false) {
        val active=runCatching {activeNotifications.orEmpty()}.getOrElse {app.phoneNotifications.disconnected();return}
        val previous=if(preserveExisting)app.phoneNotifications.snapshot().associateBy {it.localKey} else emptyMap()
        app.phoneNotifications.replace(active.asSequence().filter {it.key!=exclude}
            .sortedWith(compareByDescending<StatusBarNotification> {it.postTime}.thenBy {it.key})
            .mapNotNull {capture(it,consent,previous[it.key])}.take(128).toList())
    }
    override fun onNotificationRankingUpdate(rankingMap:RankingMap) {
        if(isConnected)refresh(app.prefs.value.notificationConsent,preserveExisting=true)
    }
    // Framework callbacks arrive in order on main. Capture/reconcile there to avoid snapshot resurrection.
    override fun onNotificationPosted(sbn:StatusBarNotification) {
        val consent=app.prefs.value.notificationConsent
        val notice=capture(sbn,consent)
        if(notice==null) {
            if(app.phoneNotifications.snapshot().any {it.localKey==sbn.key})refresh(consent,preserveExisting=true)
        } else app.phoneNotifications.post(notice)
    }
    override fun onNotificationRemoved(sbn:StatusBarNotification) {
        if(app.phoneNotifications.snapshot().any {it.localKey==sbn.key})refresh(app.prefs.value.notificationConsent,exclude=sbn.key,preserveExisting=true)
    }
    private fun capture(sbn:StatusBarNotification,consent:NotificationConsent,existing:MirrorNotice?=null):MirrorNotice?=runCatching {
        val n=sbn.notification
        // Work/other profiles are never inferred from a package name in the personal profile picker.
        if(sbn.user!=Process.myUserHandle() || sbn.packageName==packageName || !consent.enabled ||
            NotificationConsent.app(sbn.user.hashCode(),sbn.packageName) !in consent.apps ||
            n.visibility==Notification.VISIBILITY_SECRET || n.flags and (Notification.FLAG_ONGOING_EVENT or Notification.FLAG_FOREGROUND_SERVICE or Notification.FLAG_GROUP_SUMMARY)!=0)return@runCatching null
        val ranking=Ranking();if(!currentRanking.getRanking(sbn.key,ranking) || ranking.importance<NotificationManager.IMPORTANCE_DEFAULT || ranking.isSuspended)return@runCatching null
        if(existing!=null)return@runCatching existing
        val info=runCatching {packageManager.getApplicationInfo(sbn.packageName,0)}.getOrNull() ?: return@runCatching null
        val label=packageManager.getApplicationLabel(info).toString()
        val messages=androidx.core.app.NotificationCompat.MessagingStyle.extractMessagingStyleFromNotification(n)?.messages
        val message=messages?.lastOrNull()
        val title=n.extras.getCharSequence(Notification.EXTRA_TITLE)?.toString()?.takeUnless {it.isBlank()} ?: message?.person?.name?.toString()
        val text=n.extras.getCharSequence(Notification.EXTRA_TEXT)?.toString()?.takeUnless {it.isBlank()} ?: message?.text?.toString()
        MirrorNotice(sbn.key,sbn.user.hashCode(),sbn.packageName,label,sbn.postTime,
            notificationParagraph(title,1024),notificationParagraph(text,8192),icon(sbn.packageName))
    }.getOrNull()
    private fun icon(pkg:String):String?=runCatching {
        val drawable=packageManager.getApplicationIcon(pkg)
        val bitmap=Bitmap.createBitmap(64,64,Bitmap.Config.ARGB_8888)
        try {
            drawable.setBounds(0,0,64,64);drawable.draw(Canvas(bitmap))
            java.io.ByteArrayOutputStream().use {out->
                bitmap.compress(Bitmap.CompressFormat.PNG,100,out)
                out.toByteArray().takeIf {it.size in 1..8192}?.hex()
            }
        }finally {bitmap.recycle()}
    }.getOrNull()
}
