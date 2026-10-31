package io.github.stantonmatt.pneu

import android.app.Notification
import android.app.PendingIntent
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel

/** The foreground promotion stays immediate. Subsequent status/progress bursts
 * publish their latest visible content at most twice a second. */
internal class StatusNotificationUpdates(scope:CoroutineScope,build:()->Notification?,post:(Notification)->Unit) {
    private data class Content(val channel:String?,val title:String?,val text:String?,val max:Int,val progress:Int,
        val indeterminate:Boolean,val actions:List<Pair<String,PendingIntent>>)
    private fun content(n:Notification)=Content(n.channelId,n.extras.getCharSequence(Notification.EXTRA_TITLE)?.toString(),
        n.extras.getCharSequence(Notification.EXTRA_TEXT)?.toString(),n.extras.getInt(Notification.EXTRA_PROGRESS_MAX),
        n.extras.getInt(Notification.EXTRA_PROGRESS),n.extras.getBoolean(Notification.EXTRA_PROGRESS_INDETERMINATE),
        n.actions.orEmpty().map {it.title.toString() to it.actionIntent})
    private val requests=Channel<Unit>(Channel.CONFLATED)
    init {scope.launch {
        var previous:Content?=null
        for(ignored in requests) {
            delay(500)
            // Consume the whole burst before reading the latest presentation.
            while(requests.tryReceive().isSuccess) {}
            val notification=build() ?: continue
            val next=content(notification)
            if(next!=previous) {post(notification);previous=next}
        }
    }}
    fun request() {requests.trySend(Unit)}
}
