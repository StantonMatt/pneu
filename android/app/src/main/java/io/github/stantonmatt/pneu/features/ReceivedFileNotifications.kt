package io.github.stantonmatt.pneu.features

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.net.Uri
import android.util.Size
import androidx.core.app.NotificationCompat
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.CapturedAuthority
import io.github.stantonmatt.pneu.data.ReceivedFile

/** Notification actions resolve journal identity and MediaStore ownership at tap time. */
object ReceivedFileNotifications {
    fun openIntent(context:Context,row:ReceivedFile)=ReceivedFileActions.intent(context,row)
    fun shareIntent(context:Context,row:ReceivedFile)=ReceivedFileActions.intent(context,row,share=true)
    internal suspend fun post(app:AppGraph,row:ReceivedFile,authority:CapturedAuthority,
        thumbnail:()->Bitmap?={app.context.contentResolver.loadThumbnail(Uri.parse(row.uri),Size(512,512),null)},
        notify:((Notification)->Unit)?=null) {
        if(authority.binding.pcId!=row.peerId || authority.binding.generation!=row.generation)return
        if(app.withAuthority(authority) {ReceivedFileActions.published(ReceivedMediaStore(app.context),row)}!=true)return
        val context=app.context
        val manager=context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(NotificationChannel(CHANNEL,context.getString(R.string.receive_channel),NotificationManager.IMPORTANCE_DEFAULT))
        fun action(intent:Intent)=PendingIntent.getActivity(context,0,intent,PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        val open=action(openIntent(context,row));val share=action(shareIntent(context,row))
        val notification=NotificationCompat.Builder(context,CHANNEL).setSmallIcon(R.drawable.ic_stat_pneu)
            .setContentTitle(row.fileName).setContentText(context.getString(R.string.receive_from,row.senderName))
            .setTicker(context.getString(R.string.receive_received,row.fileName,row.senderName))
            .setGroup("received:${row.peerId}").setAutoCancel(true).setContentIntent(open)
            .addAction(R.drawable.ic_open_in_new,context.getString(R.string.receive_open),open)
            .addAction(R.drawable.ic_description,context.getString(R.string.receive_share),share)
        if(row.mime.startsWith("image/"))runCatching {thumbnail()?.let {notification.setStyle(NotificationCompat.BigPictureStyle().bigPicture(it))}}
        // Thumbnail/provider work may outlive revocation or a newer connection.
        // Admission and this final OS effect serialize on the same lifecycle lock.
        app.withAuthority(authority) {
            if(ReceivedFileActions.published(ReceivedMediaStore(context),row)) {
                if(notify!=null)notify(notification.build()) else manager.notify("received:${row.peerId}:${row.transferId}",0,notification.build())
            }
        }
    }
    private const val CHANNEL="received_files"
}
