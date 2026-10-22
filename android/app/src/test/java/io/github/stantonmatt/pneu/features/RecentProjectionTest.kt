package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.room.Room
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.flow.first
import org.junit.Test
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class RecentProjectionTest {
    private val a=Peer("Desktop","a".repeat(64),"a".repeat(32),emptyList())
    private val b=Peer("MacBook","b".repeat(64),"b".repeat(32),emptyList())
    private val p=Prefs(peers=listOf(a,b))
    private fun delivery(peer:Peer,state:String,id:String="artifact",order:Long=1)=Outgoing().apply {
        mediaId=id;peerId=peer.deviceId;transferId=id;this.state=state;size=100;orderId=order;error=if(state=="failed")"disk_full" else ""
    }
    @Test fun progressCombinesCompletedAndConcurrentRecipientsAndKeepsFailures() {
        val context=RuntimeEnvironment.getApplication()
        val unmatched=RecentProjection.items(listOf(delivery(a,"sending"),delivery(b,"sending")),p,emptyMap()).single()
        assertFalse(unmatched.sending)
        assertEquals("Waiting",UiPresentation.recentOutcome(context,unmatched))
        val rows=listOf(delivery(a,"sent"),delivery(b,"sending"))
        val sending=RecentProjection.items(rows,p,mapOf(b.deviceId to LinkState("Connected",sent=25,total=100,transferMediaId="artifact"))).single()
        assertEquals(125L,sending.sent);assertEquals(200L,sending.total);assertEquals(1,sending.sentCount)
        assertEquals(2,sending.recipients.size)
        val failed=RecentProjection.items(listOf(delivery(a,"sent"),delivery(b,"failed")),p,emptyMap()).single()
        assertEquals("failed",failed.row.state);assertEquals("disk_full",failed.failed.single().error)
        assertEquals("Failed on MacBook",UiPresentation.recentOutcome(context,failed))
        val partial=RecentProjection.items(listOf(delivery(a,"sending"),delivery(b,"failed")),p,mapOf(a.deviceId to LinkState("Connected",sent=25,total=100,transferMediaId="artifact"))).single()
        assertTrue(partial.sending);assertEquals(25L,partial.sent);assertEquals(200L,partial.total)
        assertEquals("Failed on MacBook",UiPresentation.recentOutcome(context,partial))
        val waiting=RecentProjection.items(listOf(delivery(a,"sent"),delivery(b,"waiting")),p,emptyMap()).single()
        assertEquals("Sent to 1 of 2",UiPresentation.recentOutcome(context,waiting))
        val both=RecentProjection.items(listOf(delivery(a,"sending"),delivery(b,"sending")),p,mapOf(
            a.deviceId to LinkState("Connected",sent=20,total=100,transferMediaId="artifact"),
            b.deviceId to LinkState("Connected",sent=40,total=100,transferMediaId="artifact"))).single()
        assertEquals(60L,both.sent);assertEquals(200L,both.total)
        assertEquals("Sending to MacBook",UiPresentation.sendingTo(context,listOf(b)))
        assertEquals("Sending to 2 computers",UiPresentation.sendingTo(context,listOf(a,b)))
    }
    @Test fun queryLimitsArtifactsWhileRetainingEveryRecipientAndActivePartialFailures()=runBlocking {
        val room=Room.inMemoryDatabaseBuilder(RuntimeEnvironment.getApplication(),QueueDatabase::class.java).allowMainThreadQueries().build()
        try {
            val queue=room.queue()
            repeat(60) {n->for(peer in listOf(a,b))queue.insert(delivery(peer,"sent","old$n",n.toLong()))}
            queue.insert(delivery(a,"sent","active",100));queue.insert(delivery(b,"failed","active",100))
            val rows=queue.recentDeliveries().first()
            assertEquals(100,rows.size);assertEquals(50,rows.map {it.mediaId}.toSet().size)
            assertTrue(rows.groupBy {it.mediaId}.values.all {it.size==2})
            val items=RecentProjection.items(rows,p,emptyMap())
            assertEquals("active",items.first().row.mediaId);assertEquals(2,items.first().recipients.size)
        }finally {room.close()}
    }
}
