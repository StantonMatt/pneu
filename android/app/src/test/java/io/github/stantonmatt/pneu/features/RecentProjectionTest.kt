package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.room.Room
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.*
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
    @Test fun compactConnectionLostUsesOneSharedUnit() {
        val context=RuntimeEnvironment.getApplication()
        assertEquals("Connection lost · 265 of 432 MB sent",UiPresentation.connectionLost(context,true,265_000_000,432_000_000))
        assertEquals("Connection lost · 0.6 of 1.8 GB sent",UiPresentation.connectionLost(context,true,600_000_000,1_800_000_000))
    }
    @Test fun waitingHeaderExcludesSendingAndFailedRetry()=runBlocking<Unit>(Dispatchers.IO) {
        val db=Room.inMemoryDatabaseBuilder(RuntimeEnvironment.getApplication(),QueueDatabase::class.java).build()
        try {
            val q=db.queue()
            q.insert(delivery(a,"sending","sending"));q.insert(delivery(a,"failed","failed"))
            assertEquals(0,q.waitingCount().first())
            q.insert(delivery(a,"waiting","waiting"));q.insert(delivery(b,"waiting","waiting"))
            assertEquals(1,q.waitingCount().first())
        } finally {db.close()}
    }
    @Test fun automaticConnectionLostRetryKeepsWaitingCopyInHomeAndRecipientRows() {
        val context=RuntimeEnvironment.getApplication()
        val delivery=delivery(a,"waiting").apply {error="connection_lost";resumable=true;sentBytes=40}
        val item=RecentProjection.items(listOf(delivery),p,emptyMap()).single()
        val recipient=item.recipients.single()
        assertEquals("Waiting",UiPresentation.recentOutcome(context,item))
        assertNull(UiPresentation.connectionLostFailure(context,delivery.state,delivery.error,delivery.resumable,delivery.sentBytes,delivery.size))
        assertNull(UiPresentation.connectionLostFailure(context,recipient.state,recipient.error,recipient.resumable,recipient.sent,recipient.total))
        assertNotNull(UiPresentation.connectionLostFailure(context,"failed",recipient.error,true,40,100))
    }
    @Test fun multiDeviceHomeNamesTheConnectionLostRecipientAndRetainedProgress() {
        val context=RuntimeEnvironment.getApplication()
        val failed=delivery(b,"failed").apply {error="connection_lost";resumable=true;sentBytes=40}
        val item=RecentProjection.items(listOf(delivery(a,"sent"),failed),p,emptyMap()).single()
        assertEquals("Failed on MacBook · "+UiPresentation.connectionLost(context,true,40,100),UiPresentation.recentOutcome(context,item))
        failed.resumable=false
        assertEquals("Failed on MacBook · "+context.getString(io.github.stantonmatt.pneu.R.string.item_connection_lost),
            UiPresentation.recentOutcome(context,RecentProjection.items(listOf(failed),p,emptyMap()).single()))
    }
    @Test fun progressCombinesCompletedAndConcurrentRecipientsAndKeepsFailures()=runBlocking<Unit>(Dispatchers.IO) {
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
        assertEquals("Failed on MacBook · Disk is full",UiPresentation.recentOutcome(context,failed))
        val partial=RecentProjection.items(listOf(delivery(a,"sending"),delivery(b,"failed")),p,mapOf(a.deviceId to LinkState("Connected",sent=25,total=100,transferMediaId="artifact"))).single()
        assertTrue(partial.sending);assertEquals(25L,partial.sent);assertEquals(200L,partial.total)
        assertEquals("Failed on MacBook · Disk is full",UiPresentation.recentOutcome(context,partial))
        val waiting=RecentProjection.items(listOf(delivery(a,"sent"),delivery(b,"waiting")),p,emptyMap()).single()
        assertEquals("Waiting for MacBook",UiPresentation.recentOutcome(context,waiting))
        assertEquals("To Desktop · 9:41",UiPresentation.recentSent(context,waiting,clipboard=false,time="9:41"))
        val sent=RecentProjection.items(listOf(delivery(a,"sent"),delivery(b,"sent")),p,emptyMap()).single()
        assertEquals("Clipboard · To 2 devices · 9:41",UiPresentation.recentSent(context,sent,clipboard=true,time="9:41"))
        val generic=RecentProjection.items(listOf(delivery(a,"sent"),delivery(b,"failed").apply {error="timeout"}),p,emptyMap()).single()
        assertEquals("Failed on MacBook",UiPresentation.recentOutcome(context,generic))
        val both=RecentProjection.items(listOf(delivery(a,"sending"),delivery(b,"sending")),p,mapOf(
            a.deviceId to LinkState("Connected",sent=20,total=100,transferMediaId="artifact"),
            b.deviceId to LinkState("Connected",sent=40,total=100,transferMediaId="artifact"))).single()
        assertEquals(60L,both.sent);assertEquals(200L,both.total)
        assertTrue(UiPresentation.recentOutcome(context,both).startsWith("Sending to 2 devices · "))
        assertEquals(listOf(a.deviceId,b.deviceId),both.recipients.map {it.item!!.peerId})
        assertEquals("Sending to MacBook",UiPresentation.sendingTo(context,listOf(b)))
        assertEquals("Sending to 2 computers",UiPresentation.sendingTo(context,listOf(a,b)))
    }
    @Test fun receivedAndSentMergeByTimeAndKeepCapturedSender()=runBlocking<Unit>(Dispatchers.IO) {
        val context=RuntimeEnvironment.getApplication()
        val received=ReceivedFile().apply {peerId=b.deviceId;transferId="receipt";senderName="Original MacBook";receivedAt=200;fileName="report.pdf";uri="content://received/1";mime="application/pdf";kind="file";state="complete"}
        val rows=listOf(delivery(a,"sent","older").apply {createdAt=100},delivery(b,"sent","newer").apply {createdAt=300},delivery(a,"waiting","pending").apply {createdAt=50})
        val merged=RecentProjection.merged(rows,listOf(received),p,emptyMap())
        assertEquals(listOf("pending","newer","received:${b.deviceId}:receipt","older"),merged.map {it.row.mediaId})
        val item=merged[2]
        assertSame(received,item.received);assertTrue(item.recipients.isEmpty())
        assertEquals("From Original MacBook · 9:41",UiPresentation.recentReceived(context,item,"9:41",2))
        assertEquals("9:41",UiPresentation.recentReceived(context,item,"9:41",1))
        val intent=ReceivedFileNotifications.openIntent(context,received)
        assertEquals(ReceivedFileActionActivity::class.java.name,intent.component!!.className)
        assertEquals(received.uri,ReceivedFileActions.expected(intent)!!.uri)
        assertEquals(0,intent.flags and android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
    @Test fun mergedKeepsActiveSendOrderRegardlessOfTimestampsAndRecipientProgress()=runBlocking<Unit>(Dispatchers.IO) {
        val rows=listOf(delivery(a,"waiting","later",20).apply {createdAt=500},
            delivery(a,"sending","first",10).apply {createdAt=900},
            delivery(b,"failed","first",10).apply {createdAt=900},
            delivery(b,"sent","old",99).apply {createdAt=100},
            delivery(a,"sent","new",1).apply {createdAt=300})
        val received=ReceivedFile().apply {peerId=b.deviceId;transferId="receipt";receivedAt=200;state="complete"}
        for(links in listOf(emptyMap(),mapOf(a.deviceId to LinkState("Connected",sent=40,total=100,transferMediaId="first")))) {
            assertEquals(listOf("first","later","new","received:${b.deviceId}:receipt","old"),
                RecentProjection.merged(rows,listOf(received),p,links).map {it.row.mediaId})
        }
    }
    @Test fun queryLimitsArtifactsWhileRetainingEveryRecipientAndActivePartialFailures()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(RuntimeEnvironment.getApplication(),QueueDatabase::class.java).build()
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
    @Test fun terminalSiblingsAreNotPendingEvenWhenTheirArtifactStillHasActiveRecipients()=runBlocking<Unit>(Dispatchers.IO) {
        for(state in listOf("sent","discarded","superseded","skipped")) {
            val item=RecentProjection.items(listOf(delivery(a,state)),p,emptyMap()).single()
            assertFalse(state,item.pending);assertFalse(state,item.actionable)
            assertFalse(state,item.recipients.single().pending);assertFalse(state,item.recipients.single().retryable)
        }
        for(state in listOf("waiting","sending")) {
            val item=RecentProjection.items(listOf(delivery(a,"discarded"),delivery(b,state)),p,emptyMap()).single()
            assertTrue(state,item.pending);assertEquals(listOf(b.deviceId),item.waiting.map {it.peerId})
        }
        val failed=RecentProjection.items(listOf(delivery(a,"failed"),delivery(b,"discarded")),p,emptyMap()).single()
        assertFalse(failed.pending);assertTrue(failed.actionable);assertTrue(failed.recipients.first().retryable)
    }

    @Test fun lostConnectionShowsSenderCountAndRetryUsesCurrentReceiverProgress() {
        val context=RuntimeEnvironment.getApplication()
        val row=delivery(a,"failed").apply {error="connection_lost";resumable=true;sentBytes=40}
        val lost=RecentProjection.items(listOf(row),p,emptyMap()).single().recipients.single()
        assertEquals(40L,lost.sent)
        assertTrue(UiPresentation.connectionLost(context,lost.resumable,lost.sent,lost.total).startsWith("Connection lost · "))
        assertEquals("Connection lost",UiPresentation.connectionLost(context,false,40,100))
        assertEquals("Connection lost",UiPresentation.connectionLost(context,true,0,100))
        row.state="sending"
        val resumed=RecentProjection.items(listOf(row),p,mapOf(a.deviceId to LinkState("Connected",sent=20,total=100,transferMediaId=row.mediaId))).single()
        assertEquals(20L,resumed.sent)
    }

}
