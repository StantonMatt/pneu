package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.connection.ConnectionStatus
import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.stantonmatt.pneu.graph
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.flow.*

/** [item] identifies this one delivery, so a details sheet can stop or retry it alone. */
data class RecipientOutcome(val peerId:String,val name:String,val state:String,val error:String,val sent:Long,val total:Long,val active:Boolean=false,val item:ConfirmedItem?=null,val resumable:Boolean=false) {
    val pending get()=state in setOf("waiting","sending")
    val retryable get()=state=="failed"
    val actionable get()=pending || retryable
}
data class RecentItem(val row:Outgoing,val recipients:List<RecipientOutcome>,val received:ReceivedFile?=null) {
    val sentCount get()=recipients.count {it.state=="sent"}
    val sent get()=recipients.sumOf {it.sent}
    val total get()=recipients.sumOf {it.total}
    val pending get()=recipients.any {it.pending}
    val actionable get()=recipients.any {it.actionable}
    val sending get()=recipients.any {it.state=="sending" && it.active}
    val failed get()=recipients.filter {it.state=="failed"}
    val waiting get()=recipients.filter {it.state=="waiting" || it.state=="sending" && !it.active}
}
internal object RecentProjection {
    fun merged(rows:List<Outgoing>,received:List<ReceivedFile>,p:Prefs,links:Map<String,LinkState>):List<RecentItem> {
        val arrivals=received.map {file->RecentItem(Outgoing().apply {
            mediaId="received:${file.peerId}:${file.transferId}";peerId=file.peerId;transferId=file.transferId
            uri=file.uri;mime=file.mime;fileName=file.fileName;kind=file.kind;size=file.size
            createdAt=file.receivedAt;state="sent";automatic=false
        },emptyList(),file)}
        val (active,history)=items(rows,p,links).partition {it.row.state!="sent"}
        return active+(history+arrivals).sortedWith(compareByDescending<RecentItem> {it.row.createdAt}
            .thenByDescending {it.row.orderId}.thenBy {it.row.mediaId})
    }
    fun items(rows:List<Outgoing>,p:Prefs,links:Map<String,LinkState>):List<RecentItem> =rows.groupBy {it.mediaId}.values.map {deliveries->
        val row=deliveries.minWith(compareBy<Outgoing> {when(it.state) {"sending"->0;"failed"->1;"waiting"->2;"sent"->3;else->4}}.thenBy {it.peerId})
        RecentItem(row,deliveries.map {delivery->
            val link=links[delivery.peerId]?.takeIf {it.connectionStatus==ConnectionStatus.CONNECTED && it.transferMediaId==delivery.mediaId && delivery.state=="sending"}
            val total=maxOf(delivery.size,link?.total ?: 0).coerceAtLeast(0)
            val sent=if(delivery.state=="sent")total else (link?.sent ?: delivery.sentBytes).coerceIn(0,total)
            RecipientOutcome(delivery.peerId,p.devices.find {it.deviceId==delivery.peerId}?.name.orEmpty(),delivery.state,delivery.error,sent,total,link!=null,ConfirmedItem.from(delivery),delivery.resumable)
        })
    }.sortedWith(compareBy<RecentItem> {if(it.row.state=="sent")1 else 0}
        .thenBy {if(it.row.state=="sent")-it.row.orderId else it.row.orderId}.thenBy {it.row.mediaId})
}
/** One artifact row with aggregate bytes and the outcome of every recipient.
 * Home shows a short summary; the details sheet lists each recipient. */
class RecentViewModel(application:Application):AndroidViewModel(application) {
    private val app=application.graph
    val items=combine(app.queue.recentDeliveries().recoverReads(),app.receivedRecent.recoverReads(),app.prefs,app.connection.devices,RecentProjection::merged)
        .stateIn(viewModelScope,SharingStarted.WhileSubscribed(5000),emptyList())
}
