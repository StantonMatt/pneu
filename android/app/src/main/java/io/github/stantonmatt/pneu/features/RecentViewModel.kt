package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.github.stantonmatt.pneu.graph
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.flow.*

data class RecipientOutcome(val peerId:String,val name:String,val state:String,val error:String,val sent:Long,val total:Long,val active:Boolean=false)
data class RecentItem(val row:Outgoing,val recipients:List<RecipientOutcome>) {
    val sentCount get()=recipients.count {it.state=="sent"}
    val sent get()=recipients.sumOf {it.sent}
    val total get()=recipients.sumOf {it.total}
    val sending get()=recipients.any {it.state=="sending" && it.active}
    val failed get()=recipients.filter {it.state=="failed"}
}
internal object RecentProjection {
    fun items(rows:List<Outgoing>,p:Prefs,links:Map<String,LinkState>):List<RecentItem> =rows.groupBy {it.mediaId}.values.map {deliveries->
        val row=deliveries.minWith(compareBy<Outgoing> {when(it.state) {"sending"->0;"failed"->1;"waiting"->2;"sent"->3;else->4}}.thenBy {it.peerId})
        RecentItem(row,deliveries.map {delivery->
            val link=links[delivery.peerId]?.takeIf {it.status=="Connected" && it.transferMediaId==delivery.mediaId && delivery.state=="sending"}
            val total=maxOf(delivery.size,link?.total ?: 0).coerceAtLeast(0)
            val sent=if(delivery.state=="sent")total else (link?.sent ?: 0).coerceIn(0,total)
            RecipientOutcome(delivery.peerId,p.devices.find {it.deviceId==delivery.peerId}?.name.orEmpty(),delivery.state,delivery.error,sent,total,link!=null)
        })
    }.sortedWith(compareBy<RecentItem> {if(it.row.state=="sent")1 else 0}
        .thenBy {if(it.row.state=="sent")-it.row.orderId else it.row.orderId}.thenBy {it.row.mediaId})
}
/** One artifact row with aggregate bytes and the outcome of every recipient.
 * The current UI uses a short summary; future details can use the same state. */
class RecentViewModel(application:Application):AndroidViewModel(application) {
    private val app=application.graph
    val items=combine(app.queue.recentDeliveries().recoverReads(),app.prefs,app.connection.devices,RecentProjection::items)
        .stateIn(viewModelScope,SharingStarted.WhileSubscribed(5000),emptyList())
}
