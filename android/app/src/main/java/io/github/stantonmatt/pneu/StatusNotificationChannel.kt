package io.github.stantonmatt.pneu

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.os.Build

internal object StatusNotificationChannel {
    const val ID="status_v2"
    const val LEGACY_ID="status"
    @Volatile private var resolvedId:String?=null

    fun create(context:Context) {id(context)}

    /** Resolve once per process so posters, health checks and Settings agree.
     * Modified legacy channels retain Android's native settings and user locks. */
    fun id(context:Context,loadLegacy:(NotificationManager)->NotificationChannel?={it.getNotificationChannel(LEGACY_ID)}):String {
        return synchronized(this) {
            val cached=resolvedId
            if(cached!=null && runCatching {context.getSystemService(NotificationManager::class.java).getNotificationChannel(cached)!=null}.getOrDefault(false))cached
            else resolve(context,loadLegacy).also {resolvedId=it}
        }
    }

    /** Never submit a foreground notification to an unregistered channel. */
    fun ensure(context:Context):String?=runCatching {
        val selected=id(context)
        selected.takeIf {context.getSystemService(NotificationManager::class.java).getNotificationChannel(it)!=null}
    }.getOrNull()

    private fun resolve(context:Context,loadLegacy:(NotificationManager)->NotificationChannel?):String = try {
        val manager=context.getSystemService(NotificationManager::class.java)
        migrate(manager,LEGACY_ID,ID,context.getString(R.string.channel_status),loadLegacy={loadLegacy(manager)})
    } catch(_:Exception) {
        android.util.Log.w("Pneu","status_channel_startup_recovery")
        LEGACY_ID
    }

    @androidx.annotation.VisibleForTesting
    @Synchronized
    internal fun resetForTest(selected:String?=null) {resolvedId=selected}

    @androidx.annotation.VisibleForTesting
    @Synchronized
    internal fun selectForTest(context:Context,legacyId:String,id:String,loadLegacy:()->NotificationChannel?):String {
        val manager=context.getSystemService(NotificationManager::class.java)
        return migrate(manager,legacyId,id,context.getString(R.string.channel_status),loadLegacy=loadLegacy).also {resolvedId=it}
    }

    // Explicit IDs/callbacks isolate fixtures and simulate system-only settings.
    internal fun migrate(manager:NotificationManager,legacyId:String,id:String,name:CharSequence,
        loadLegacy:()->NotificationChannel?={manager.getNotificationChannel(legacyId)},
        register:(NotificationChannel)->Unit={manager.createNotificationChannel(it)}):String {
        return try {
            if(manager.getNotificationChannel(id)!=null)return id
            val legacy=loadLegacy()
            if(legacy!=null && !untouched(legacy,name))return legacyId
            // Create before retiring the old channel, so a creation failure
            // leaves an existing foreground channel usable on this startup.
            register(originalSpec(id,name).apply {setShowBadge(false)})
            check(manager.getNotificationChannel(id)!=null) {"Status channel missing"}
            if(legacy!=null)manager.deleteNotificationChannel(legacyId)
            id
        } catch(_:Exception) {
            android.util.Log.w("Pneu","status_channel_migration_recovery")
            runCatching {
                if(manager.getNotificationChannel(id)!=null)id
                else if(manager.getNotificationChannel(legacyId)!=null)legacyId
                else {
                    manager.createNotificationChannel(originalSpec(id,name).apply {setShowBadge(false)})
                    id
                }
            }.getOrDefault(legacyId)
        }
    }

    /** The original Pneu spec: LOW importance, all framework defaults. */
    internal fun originalSpec(id:String,name:CharSequence)=NotificationChannel(id,name,NotificationManager.IMPORTANCE_LOW)

    internal fun untouched(channel:NotificationChannel,name:CharSequence):Boolean {
        val original=originalSpec(LEGACY_ID,name)
        return !channel.hasUserSetImportance() && !channel.hasUserSetSound() &&
            channel.importance==original.importance &&
            channel.lockscreenVisibility==original.lockscreenVisibility &&
            channel.canBypassDnd()==original.canBypassDnd() &&
            channel.sound==original.sound && channel.audioAttributes==original.audioAttributes &&
            channel.shouldVibrate()==original.shouldVibrate() &&
            java.util.Arrays.equals(channel.vibrationPattern,original.vibrationPattern) &&
            (Build.VERSION.SDK_INT<35 || channel.vibrationEffect==original.vibrationEffect) &&
            channel.shouldShowLights()==original.shouldShowLights() && channel.lightColor==original.lightColor &&
            channel.canShowBadge()==original.canShowBadge() && channel.canBubble()==original.canBubble() &&
            channel.isBlockable==original.isBlockable && channel.isImportantConversation==original.isImportantConversation &&
            channel.isDemoted==original.isDemoted && channel.group==original.group &&
            channel.parentChannelId==original.parentChannelId && channel.conversationId==original.conversationId &&
            channel.name.toString()==original.name.toString() && channel.description==original.description
    }
}
