package io.github.stantonmatt.pneu.features

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import androidx.core.app.NotificationCompat
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.data.Peer

internal object FolderSentNotifications {
    fun show(context:Context,row:Outgoing,peerId:String,peers:List<Peer>) {
        val summary=row.folderSummary() ?: return
        if(summary.skipped.count==0 && summary.skipped.directories==0)return
        val device=peers.find {it.deviceId==peerId}?.name ?: return
        val manager=context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(NotificationChannel("alerts",context.getString(R.string.channel_alerts),NotificationManager.IMPORTANCE_DEFAULT))
        val body=summary.skipped.notification(context)
        val notification=NotificationCompat.Builder(context,"alerts").setSmallIcon(R.drawable.ic_stat_pneu)
            .setContentTitle(context.getString(R.string.folder_sent_to,row.fileName,device)).setContentText(body).setAutoCancel(true)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body))
        manager.notify("folder:${row.transferId}:$peerId",0,notification.build())
    }
}
