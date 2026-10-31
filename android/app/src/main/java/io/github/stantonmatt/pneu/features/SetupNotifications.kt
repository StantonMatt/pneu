package io.github.stantonmatt.pneu.features

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.NotificationCompat
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.launch

/** Joins and link requests. With Pneu in front it shows them itself (Home's snackbar, the
 * request dialog); link requests have a separate, prominent channel so warnings cannot bury their actions. */
internal object SetupNotifications {
    private const val CHANNEL="alerts"
    private const val REQUEST_CHANNEL="setup_requests"
    const val REQUEST="setup_request"
    const val ACTION_ALLOW="io.github.stantonmatt.pneu.SETUP_ALLOW"
    const val ACTION_DENY="io.github.stantonmatt.pneu.SETUP_DENY"
    const val ACTION_DISMISS="io.github.stantonmatt.pneu.SETUP_DISMISS"
    const val READMIT_ALLOW="io.github.stantonmatt.pneu.READMIT_ALLOW"
    const val READMIT_KEEP="io.github.stantonmatt.pneu.READMIT_KEEP"
    fun cancelReadmission(context:Context,key:String) {
        context.getSystemService(NotificationManager::class.java).cancel(key,0)
        context.setupUi.notifiedReadmissions.value=context.setupUi.notifiedReadmissions.value-key
    }
    fun joinedTag(id:String)="setup_joined:$id"
    fun channel(context:Context) {
        val nm=context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL,context.getString(R.string.channel_alerts),NotificationManager.IMPORTANCE_DEFAULT))
        nm.createNotificationChannel(NotificationChannel(REQUEST_CHANNEL,context.getString(R.string.channel_setup_requests),NotificationManager.IMPORTANCE_HIGH))
    }
    /** Opens a page in Pneu; remove also asks to remove the device, cancel clears this notification. */
    fun pageIntent(context:Context,page:String,remove:String?=null,cancel:String?=null)=Intent(context,MainActivity::class.java)
        .setIdentifier("setup:$page:${remove.orEmpty()}:${cancel.orEmpty()}").putExtra("page",page).putExtra("remove",remove).putExtra("cancel",cancel)
        .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
    private fun activity(context:Context,intent:Intent)=PendingIntent.getActivity(context,0,intent,PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
    private fun broadcast(context:Context,action:String,keys:List<String>)=PendingIntent.getBroadcast(context,0,
        Intent(context,SetupActionReceiver::class.java).setAction(action).setIdentifier("setup:$action:"+keys.joinToString(",")).putExtra("keys",keys.toTypedArray()),
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
    fun cancelRequest(context:Context) {
        context.getSystemService(NotificationManager::class.java).cancel(REQUEST,0)
        context.setupUi.notifiedPrompts.value=emptySet()
    }

    suspend fun run(app:AppGraph) {
        val context=app.context;val ui=context.setupUi;val nm=context.getSystemService(NotificationManager::class.java)
        channel(context)
        // A slower collector can see a snapshot from before its own dismissal; post each once.
        val posted=mutableSetOf<String>()
        combine(app.prefs,MainActivity.foreground,ui.dismissedPrompts,ui.presentedNotices) {p,front,dismissed,_->Triple(p,front,dismissed)}.collect {(p,front,dismissed)->
            try {show(app,nm,p,front,dismissed,posted)}
            catch(e:Exception) {if(e is kotlinx.coroutines.CancellationException)throw e;android.util.Log.w("PneuSetupNotice",e.javaClass.simpleName)}
        }
    }
    internal suspend fun show(app:AppGraph,nm:NotificationManager,p:Prefs,front:Boolean,dismissed:Set<String>,posted:MutableSet<String>) {
        val context=app.context;val ui=context.setupUi
        if(!p.loaded)return
        val readmissions=SetupPresentation.readmissions(p)
        val readmitKeys=readmissions.map {it.actionKey}.toSet()
        (ui.notifiedReadmissions.value-readmitKeys).forEach {cancelReadmission(context,it)}
        for(prompt in readmissions) {
            if(front)cancelReadmission(context,prompt.actionKey)
            else if(canPost(context,REQUEST_CHANNEL) && prompt.actionKey !in dismissed && prompt.actionKey !in ui.notifiedReadmissions.value) {
                postReadmission(context,p,prompt)
                ui.notifiedReadmissions.value=ui.notifiedReadmissions.value+prompt.actionKey
            }
        }
        val prompts=SetupPresentation.prompts(p);val keys=prompts.map(SetupPresentation::promptKey)
        if(prompts.isEmpty()) {nm.cancel(REQUEST,0);ui.notifiedPrompts.value=emptySet()}
        else if(canPost(context,REQUEST_CHANNEL) && !front && keys.any {it !in dismissed} && keys.toSet()!=ui.notifiedPrompts.value) {
            postRequest(context,p,prompts)
            ui.notifiedPrompts.value=keys.toSet()
        }
        if(front || !canPost(context))return
        // Also linked stays for Home. Already presented snackbars never notify.
        for(notice in SetupPresentation.joined(p).filterNot {it.id in ui.presentedNotices.value}) {
            if(notice.id !in posted) {
                postJoined(context,p,notice)
                posted.add(notice.id)
            }
            app.dismissSetupNotices(setOf(notice.id))
        }
    }
    internal fun canPost(context:Context,channel:String=CHANNEL)=NotificationManagerCompat.from(context).areNotificationsEnabled() &&
        (context.getSystemService(NotificationManager::class.java).getNotificationChannel(channel)?.importance ?: NotificationManager.IMPORTANCE_NONE)>NotificationManager.IMPORTANCE_NONE
    /** {device} wants to link your devices · Allow · Don't allow. Swiping it away leaves it waiting. */
    fun postRequest(context:Context,p:Prefs,prompts:List<SetupPrompt>) {
        val keys=prompts.map(SetupPresentation::promptKey)
        val title=SetupPresentation.requestTitle(context.resources,SetupPresentation.promptNames(p,prompts))
        val text=context.getString(R.string.setup_request_text)
        context.getSystemService(NotificationManager::class.java).notify(REQUEST,0,NotificationCompat.Builder(context,REQUEST_CHANNEL).setSmallIcon(R.drawable.ic_stat_pneu).setColor(0xFF4F5BFF.toInt())
            .setContentTitle(title).setContentText(text).setStyle(NotificationCompat.BigTextStyle().setBigContentTitle(title).bigText(text))
            .setContentIntent(activity(context,Intent(context,MainActivity::class.java).putExtra("home",true)))
            .setPriority(NotificationCompat.PRIORITY_HIGH).setGroup("setup_requests").setGroupAlertBehavior(NotificationCompat.GROUP_ALERT_ALL)
            .setDeleteIntent(broadcast(context,ACTION_DISMISS,keys)).setOnlyAlertOnce(true).setAutoCancel(true)
            .addAction(0,context.getString(R.string.action_allow),broadcast(context,ACTION_ALLOW,keys))
            .addAction(0,context.getString(R.string.action_dont_allow),broadcast(context,ACTION_DENY,keys)).build())
    }
    fun postReadmission(context:Context,p:Prefs,prompt:ReadmissionPrompt) {
        val text=SetupPresentation.readmissionText(context.resources,p,prompt)
        val keys=listOf(prompt.actionKey)
        context.getSystemService(NotificationManager::class.java).notify(prompt.actionKey,0,
            NotificationCompat.Builder(context,REQUEST_CHANNEL).setSmallIcon(R.drawable.ic_stat_pneu).setColor(0xFF4F5BFF.toInt())
                .setContentTitle(text).setStyle(NotificationCompat.BigTextStyle().bigText(text))
                .setContentIntent(activity(context,Intent(context,MainActivity::class.java).putExtra("home",true)))
                .setPriority(NotificationCompat.PRIORITY_HIGH).setGroup("setup_requests").setOnlyAlertOnce(true).setAutoCancel(true)
                .setDeleteIntent(broadcast(context,ACTION_DISMISS,keys))
                .addAction(0,context.getString(R.string.setup_keep_removed),broadcast(context,READMIT_KEEP,keys))
                .addAction(0,context.getString(R.string.action_allow),broadcast(context,READMIT_ALLOW,keys)).build())
    }
    /** {device} joined · Added via {device} · Remove, which opens its page with the confirmation. */
    fun postJoined(context:Context,p:Prefs,notice:SetupNotice) {
        val key=p.devices.find {it.deviceId==notice.peerId}?.let {SetupPresentation.memberKey(p,it)}
        val tag=joinedTag(notice.id)
        context.getSystemService(NotificationManager::class.java).notify(tag,0,NotificationCompat.Builder(context,CHANNEL).setSmallIcon(R.drawable.ic_stat_pneu).setColor(0xFF4F5BFF.toInt())
            .setContentTitle(context.getString(R.string.setup_joined,notice.name)).setContentText(context.getString(R.string.setup_added_via,notice.introducerName))
            .setContentIntent(activity(context,pageIntent(context,DEVICE+notice.peerId,cancel=tag))).setOnlyAlertOnce(true).setAutoCancel(true)
            .apply {if(key!=null)addAction(0,context.getString(R.string.setup_action_remove),activity(context,pageIntent(context,DEVICE+notice.peerId,key,tag)))}.build())
    }
}

/** A joined notice is shown once, then dropped; Added via stays on the device page. */
internal suspend fun AppGraph.dismissSetupNotices(ids:Set<String>)=withBindingLifecycle {preferences.edit {p->
    val state=SetupStorage.read(p);SetupStorage.write(p,state.copy(notices=state.notices.filterNot {it.id in ids}))
}}
/** Answers the requests that were shown. One Allow links this phone's other undecided pairings too. */
internal suspend fun AppGraph.answerSetupRequests(keys:Set<String>,allow:Boolean) {
    if(allow)context.setupUi.update {it.copy(joinedPin="",joinedAt=0)}
    var answered=0
    for(key in keys) {
        val prompt=SetupPresentation.prompts(preferences.read()).find {SetupPresentation.promptKey(it)==key} ?: continue
        answerSetupPrompt(prompt,allow);answered++
    }
    check(answered>0) {"Link request changed"}
}

/** Allow, Don't allow and swipe-away on the request notification. Swiping leaves it waiting. */
class SetupActionReceiver:BroadcastReceiver() {
    override fun onReceive(context:Context,intent:Intent) {
        val keys=intent.getStringArrayExtra("keys")?.toSet().orEmpty()
        if(intent.action==SetupNotifications.ACTION_DISMISS) {context.setupUi.dismissPrompts(keys);return}
        if(intent.action in setOf(SetupNotifications.READMIT_ALLOW,SetupNotifications.READMIT_KEEP)) {
            val allow=intent.action==SetupNotifications.READMIT_ALLOW
            val app=context.graph;val pending=goAsync()
            app.scope.launch {
                try {
                    val prompt=SetupPresentation.readmissions(app.preferences.read()).find {it.actionKey in keys} ?: return@launch
                    if(UiActions.feedback(context).run(context.getString(R.string.setup_readmission_error),errorMessage={readmissionError(context,it)}) {
                        app.answerReadmission(prompt,allow)
                    } && allow)runCatching {PneuService.start(context)}
                }finally {pending.finish()}
            }
            return
        }
        if(intent.action !in setOf(SetupNotifications.ACTION_ALLOW,SetupNotifications.ACTION_DENY) || keys.isEmpty())return
        val allow=intent.action==SetupNotifications.ACTION_ALLOW
        val app=context.graph;val pending=goAsync()
        app.scope.launch {
            try {
                if(UiActions.feedback(context).run(context.getString(R.string.ui_error_link)) {app.answerSetupRequests(keys,allow)} && allow)
                    runCatching {PneuService.start(context)}
            }finally {pending.finish()}
        }
    }
}
