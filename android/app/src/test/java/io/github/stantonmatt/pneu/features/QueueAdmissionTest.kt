package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.room.Room
import io.github.stantonmatt.pneu.data.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class QueueAdmissionTest {
    private val a=DeliveryBinding("phone","pin-a","a",1)
    private val b=DeliveryBinding("phone","pin-b","b",2)
    private lateinit var db:QueueDatabase
    private val queue get()=db.queue()
    @Before fun setup() {db=Room.inMemoryDatabaseBuilder(RuntimeEnvironment.getApplication(),QueueDatabase::class.java).allowMainThreadQueries().build()}
    @After fun cleanup() {db.close()}
    private fun row(id:String,kind:String="text",time:Long=2,targets:List<DeliveryBinding> = listOf(a))=Outgoing().apply {
        mediaId=id;transferId=id;this.kind=kind;createdAt=time;recipients=targets;automatic=false
    }
    private fun full(targets:List<DeliveryBinding> = listOf(a),state:String="waiting") {
        db.runInTransaction {
            repeat(2047) {queue.insert(row("file$it","file").apply {peerId=a.pcId})}
            targets.forEach {queue.admit(row("old",time=1).apply {peerId=it.pcId;this.state=state})}
        }
        assertEquals(2048,queue.pendingCount())
    }
    private fun reject(row:Outgoing) {
        val clocks=listOf(a,b).associate {it.pcId to queue.peerTextTime(it.pcId)}
        try {QueueAdmission.admit(db,row);fail("Expected capacity rejection")}catch(_:LocalCapacityException) {}
        assertEquals(2048,queue.pendingCount());assertNull(queue.find(row.mediaId))
        assertEquals("waiting",queue.delivery("old",a.pcId).state)
        clocks.forEach {(peer,time)->assertEquals(time,queue.peerTextTime(peer))}
    }
    @Test fun unrelatedTextDoesNotFreeCapacityAndTheEntireAdmissionRollsBack() {
        full();reject(row("new",targets=listOf(b)))
    }
    @Test fun partialSharedSupersessionDoesNotRetireTheArtifactOrAdvanceAnyWatermark() {
        full(listOf(a,b));reject(row("new"))
        assertEquals("waiting",queue.delivery("old",b.pcId).state)
    }
    @Test fun supersedingEveryRecipientFreesExactlyOneArtifact() {
        full(listOf(a,b));assertTrue(QueueAdmission.admit(db,row("new",targets=listOf(a,b))))
        assertEquals(2048,queue.pendingCount())
        assertTrue(queue.deliveries("old").all {it.state=="superseded"})
        assertEquals(2,queue.deliveries("new").size)
    }
    @Test fun inFlightTextCannotBeSubtractedButOlderIncomingTextConsumesNoSlot() {
        full(state="sending")
        try {QueueAdmission.admit(db,row("new"));fail("Sending text is still active")}catch(_:LocalCapacityException) {}
        assertEquals("sending",queue.delivery("old",a.pcId).state)
        assertNull(queue.find("new"))
        assertTrue(QueueAdmission.admit(db,row("older",time=1)))
        assertEquals("superseded",queue.delivery("older",a.pcId).state)
        assertEquals(2048,queue.pendingCount())
    }
    @Test fun aFileAtCapacityRollsBackAndRetryingAnExistingDeliveryAddsNoArtifact() {
        full();reject(row("new-file","file"))
        assertFalse(QueueAdmission.admit(db,row("file0","file")))
        assertEquals(2048,queue.pendingCount())
    }
    @Test fun suppressedAutomaticAdmissionIsDurablyRetiredAndDoesNotBlockTheNextItem() {
        val echo=row("echo","photo").apply {automatic=true}
        val later=row("later","photo").apply {automatic=true}
        queue.freezeAutomaticAdmission(echo);queue.freezeAutomaticAdmission(later)
        repeat(2) {QueueAdmission.retireAutomatic(db,echo)}
        assertEquals("skipped",echo.state)
        assertEquals(1,queue.seen(echo.mediaId));assertNull(queue.pendingAdmission(echo.mediaId))
        assertNull(queue.find(echo.mediaId))
        assertEquals(listOf("later"),queue.pendingAdmissions().map {it.mediaId})
        assertTrue(QueueAdmission.admit(db,later))
        assertTrue(queue.pendingAdmissions().isEmpty());assertEquals(1,queue.seen(later.mediaId))
        assertEquals(a.pcId,queue.deliveries(later.mediaId).single().peerId)
    }
    @Test fun ordinaryFileProvenanceLookupUsesCompletedJournalBeyondRecentHistory() {
        fun received(id:String,state:String)=ReceivedFile().apply {
            peerId=a.pcId;transferId=id;uri="content://media/external_primary/downloads/$id";this.state=state;catalogVersion="catalog"
        }
        val complete=received("1","complete");val pending=received("2","verified")
        queue.insertReceivedFile(complete);queue.insertReceivedFile(pending)
        assertEquals(a.pcId,queue.receivedFileByMediaId("catalog",1).peerId)
        assertNull(queue.receivedFileByMediaId("catalog",2));assertNull(queue.receivedFileByMediaId("catalog",3));assertNull(queue.receivedFileByMediaId("rebuilt",1))
    }

}
