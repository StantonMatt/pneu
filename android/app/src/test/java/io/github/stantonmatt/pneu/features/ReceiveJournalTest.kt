package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.room.Room
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.shadows.ShadowLog
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ReceiveJournalTest {
    private val context get()=RuntimeEnvironment.getApplication()
    private val a="a".repeat(32);private val b="b".repeat(32)
    private val metadata=ReceivedMetadata("f".repeat(32),"photo.png","image/png",3,sha256(byteArrayOf(1,2,3)),"photo",123)
    private class Storage:ReceiveStorage {
        val rows=mutableMapOf<String,Boolean>();var creations=0;var publications=0;var beforeDelete:(String)->Unit={};var probes=0
        override fun create(metadata:ReceivedMetadata):Pair<String,String> {val uri="content://media/external_primary/images/media/${++creations}";rows[uri]=true;return uri to metadata.name}
        override fun open(uri:String):ReceiveOutput=error("Not needed for journal checks")
        override fun pending(uri:String):Boolean? {probes++;return rows[uri]}
        override fun publish(uri:String) {check(rows[uri]==true);rows[uri]=false;publications++}
        override fun deletePending(uri:String) {beforeDelete(uri);if(pending(uri)==true)rows.remove(uri)}
        override fun cleanOrphans(retained:Set<String>) {rows.keys.toList().filter {it !in retained}.forEach(::deletePending)}
    }
    private fun row(peer:String=a)=ReceivedFile().apply {peerId=peer;transferId=this@ReceiveJournalTest.metadata.id;this.metadata=this@ReceiveJournalTest.metadata.canonical;fileName=this@ReceiveJournalTest.metadata.name;mime=this@ReceiveJournalTest.metadata.mime;kind=this@ReceiveJournalTest.metadata.kind}
    private fun reserve(journal:ReceiveJournal,storage:Storage,queue:QueueDao,peer:String=a):ReceivedFile {
        val row=row(peer);journal.reserve(row,metadata);row.uri=storage.create(metadata).first;queue.updateReceivedFile(row);return row
    }
    @Test fun permanentSenderScopedReplayConflictAndClipboardPruning()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val storage=Storage();val queue=room.queue();val journal=ReceiveJournal(queue,storage)
            assertEquals(FileAdmission.NEW,journal.admission(a,metadata))
            val first=reserve(journal,storage,queue);journal.verified(first);journal.publish(first)
            assertEquals(FileAdmission.RETAINED_REPLAY,journal.admission(a,metadata))
            assertEquals(FileAdmission.NEW,journal.admission(b,metadata))
            for(changed in listOf(metadata.copy(name="changed.png"),metadata.copy(size=4),metadata.copy(mime="image/jpeg"),metadata.copy(kind="file"),metadata.copy(createdAt=124),metadata.copy(mediaId="different"),metadata.copy(hlc="1",source=a)))
                assertEquals(FileAdmission.RETAINED_CONFLICT,journal.admission(a,changed))
            val second=reserve(journal,storage,queue,b);journal.verified(second);journal.publish(second)
            repeat(100) {index->queue.completeReceipt(Receipt().apply {peerId=a;transferId="clip$index";createdAt=index.toLong();state="applied"})}
            assertEquals(FileAdmission.RETAINED_REPLAY,journal.admission(a,metadata));assertEquals(66,queue.receiptCount())
            queue.clearPeer(a);assertEquals(FileAdmission.NEW,journal.admission(a,metadata));assertEquals(FileAdmission.RETAINED_REPLAY,journal.admission(b,metadata))
            assertEquals(2,storage.rows.size)
        }finally {room.close()}
    }
    @Test fun obsoleteCleanupCannotDeleteAReplacementBindingsPartialFile()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val storage=Storage();val queue=room.queue();val journal=ReceiveJournal(queue,storage)
            for(generation in listOf(1L,2L)) {
                val old=reserve(journal,storage,queue).apply {this.generation=1};queue.updateReceivedFile(old)
                queue.clearPeer(a)
                val replacement=reserve(journal,storage,queue).apply {this.generation=generation};queue.updateReceivedFile(replacement)
                journal.abandon(old)
                assertEquals(replacement.uri,queue.receivedFile(a,metadata.id).uri)
                assertNotNull(queue.receipt(a,metadata.id));assertEquals(true,storage.pending(replacement.uri))
                queue.clearPeer(a)
            }
        }finally {room.close()}
    }
    @Test fun textAndSettingsReservationsConflictWithFiles()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val queue=room.queue();val journal=ReceiveJournal(queue,Storage())
            for(origin in listOf("clipboard","share","peer_settings")) {
                queue.insertReceipt(Receipt().apply {peerId=a;transferId=metadata.id;this.origin=origin;state="reserved"})
                assertEquals(FileAdmission.RETAINED_CONFLICT,journal.admission(a,metadata));queue.deleteReceipt(a,metadata.id)
            }
        }finally {room.close()}
    }
    @Test fun crashAfterPublishBeforeReceiptIsRecoveredAcrossDatabaseReopenWithoutAnotherFile()=runBlocking<Unit>(Dispatchers.IO) {
        val name="receive-crash.db";context.deleteDatabase(name);val storage=Storage()
        fun open()=Room.databaseBuilder(context,QueueDatabase::class.java,name).build()
        val first=open()
        try {
            val queue=first.queue();val journal=ReceiveJournal(queue,storage);val row=reserve(journal,storage,queue)
            journal.verified(row)
            first.openHelper.writableDatabase.execSQL("CREATE TRIGGER fail_received_commit BEFORE UPDATE ON received_files WHEN NEW.state='complete' BEGIN SELECT RAISE(ABORT,'injected completion failure'); END")
            try {journal.publish(row);fail("Commit should fail")}catch(_:android.database.SQLException) {}
            assertEquals(false,storage.pending(row.uri));assertEquals("verified",queue.receivedFile(a,metadata.id).state)
            assertEquals("reserved",queue.receipt(a,metadata.id).state)
        }finally {first.close()}
        val second=open()
        try {
            second.openHelper.writableDatabase.execSQL("DROP TRIGGER fail_received_commit")
            val journal=ReceiveJournal(second.queue(),storage)
            journal.recover(second.queue().receivedFile(a,metadata.id))
            assertEquals(FileAdmission.RETAINED_REPLAY,journal.admission(a,metadata))
            assertEquals(1,storage.creations);assertEquals(1,storage.publications);assertEquals(1,storage.rows.size)
        }finally {second.close();context.deleteDatabase(name)}
    }
    @Test fun recoveryRechecksPolicyOnlyBeforePublicationAndRetainsAnAlreadyPublishedEffect()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val storage=Storage();val queue=room.queue();val journal=ReceiveJournal(queue,storage)
            val pending=reserve(journal,storage,queue);journal.verified(pending)
            journal.recover(pending,allowPublication=false)
            assertEquals(0,storage.publications);assertTrue(storage.rows.isEmpty())
            assertEquals(FileAdmission.NEW,journal.admission(a,metadata))
            val visible=reserve(journal,storage,queue);journal.verified(visible);storage.publish(visible.uri)
            journal.recover(visible,allowPublication=false)
            assertEquals(1,storage.publications);assertEquals(FileAdmission.RETAINED_REPLAY,journal.admission(a,metadata))
        }finally {room.close()}
    }
    @Test fun crashBeforePublishFinishesVerifiedRowAndUnverifiedPartialIsDeleted()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val storage=Storage();val queue=room.queue();val journal=ReceiveJournal(queue,storage)
            val partial=reserve(journal,storage,queue);journal.recover(partial)
            assertTrue(storage.rows.isEmpty());assertEquals(FileAdmission.NEW,journal.admission(a,metadata))
            val verified=reserve(journal,storage,queue);journal.verified(verified);journal.recover(verified)
            assertEquals(1,storage.publications);assertEquals(FileAdmission.RETAINED_REPLAY,journal.admission(a,metadata))
        }finally {room.close()}
    }
    @Test fun replacementBetweenCleanupCheckAndDeleteKeepsItsJournalAndReceiptAtomically()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val queue=room.queue();val storage=Storage();val journal=ReceiveJournal(queue,storage)
            val old=reserve(journal,storage,queue).apply {generation=2};queue.updateReceivedFile(old)
            var replacement:ReceivedFile?=null
            storage.beforeDelete={
                storage.beforeDelete={}
                queue.clearPeer(a)
                replacement=reserve(journal,storage,queue).apply {generation=3};queue.updateReceivedFile(replacement!!)
            }
            journal.abandon(old)
            assertEquals(replacement!!.uri,queue.receivedFile(a,metadata.id).uri)
            assertEquals(3L,queue.receivedFile(a,metadata.id).generation)
            assertEquals("reserved",queue.receipt(a,metadata.id).state)
            assertEquals(true,storage.pending(replacement!!.uri))
        }finally {room.close()}
    }
    @Test fun conditionalRemovalChecksUriGenerationMetadataCatalogAndStateAndNeverRemovesAppliedReceipt()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val q=room.queue();val storage=Storage();val journal=ReceiveJournal(q,storage)
            val original=reserve(journal,storage,q)
            val mutations=listOf<(ReceivedFile)->Unit>({it.stagePath="replacement-spool"},{it.generation++},{it.uri="replacement"},{it.metadata="replacement"},{it.catalogVersion="replacement"},{it.state="verified"},{it.state="complete"})
            for(mutate in mutations) {
                val replacement=q.receivedFile(a,metadata.id);mutate(replacement);q.updateReceivedFile(replacement)
                assertFalse(q.abandonReceivedFile(original));assertNotNull(q.receivedFile(a,metadata.id));assertNotNull(q.receipt(a,metadata.id))
                q.updateReceivedFile(original)
            }
            val receipt=q.receipt(a,metadata.id).apply {state="applied"};q.updateReceipt(receipt)
            assertFalse(q.abandonReceivedFile(original));assertNotNull(q.receivedFile(a,metadata.id))
        }finally {room.close()}
    }
    @Test fun durableCapacityKeepsReplaysAndConflictsAndRejectsFreshIdsAfterDatabaseReopen()=runBlocking<Unit>(Dispatchers.IO) {
        ShadowLog.clear()
        val name="receive-capacity.db";context.deleteDatabase(name);val storage=Storage()
        fun open()=Room.databaseBuilder(context,QueueDatabase::class.java,name).build()
        fun add(q:QueueDao,journal:ReceiveJournal,peer:String,id:String,complete:Boolean=true):ReceivedFile {
            val meta=metadata.copy(id=id)
            val file=row(peer).apply {transferId=id;this.metadata=meta.canonical}
            journal.reserve(file,meta);file.uri=storage.create(meta).first;q.updateReceivedFile(file)
            if(complete) {journal.verified(file);journal.publish(file)}
            return file
        }
        val first=open()
        try {
            val q=first.queue();val journal=ReceiveJournal(q,storage,peerLimit=2,globalLimit=3)
            add(q,journal,a,metadata.id);add(q,journal,a,"1".repeat(32),complete=false)
            try {add(q,journal,a,"2".repeat(32));fail("Per-peer cap")}catch(e:ReceiveStorageException) {assertEquals("storage_failed",e.code)}
            add(q,journal,b,"3".repeat(32))
            try {add(q,journal,"c".repeat(32),"4".repeat(32));fail("Global cap")}catch(e:ReceiveStorageException) {assertEquals("storage_failed",e.code)}
            assertEquals(3,storage.creations)
        }finally {first.close()}
        val second=open()
        try {
            val q=second.queue();val journal=ReceiveJournal(q,storage,peerLimit=2,globalLimit=3)
            assertEquals(FileAdmission.RETAINED_REPLAY,journal.admission(a,metadata))
            assertEquals(FileAdmission.RETAINED_CONFLICT,journal.admission(a,metadata.copy(name="other")))
            try {add(q,journal,a,"5".repeat(32));fail("Durable cap after restart")}catch(_:ReceiveStorageException) {}
            assertEquals(3,q.receivedFileCount());assertEquals(3,q.receiptCount())
            val logs=ShadowLog.getLogsForTag("PneuReceive").filter {it.msg.contains("phase=journal_capacity")}
            assertEquals(3,logs.size)
            assertTrue(logs[0].msg.contains("peer=$a transfer_id=${"2".repeat(32)} kind=photo"))
            assertTrue(logs[0].msg.contains("peer_count=2 peer_limit=2 global_count=2 global_limit=3"))
            assertTrue(logs[1].msg.contains("peer_count=0 peer_limit=2 global_count=3 global_limit=3"))
            assertTrue(logs[2].msg.contains("peer_count=2 peer_limit=2 global_count=3 global_limit=3"))
            for(log in logs) {
                assertEquals(android.util.Log.WARN,log.type);assertNull(log.throwable)
                assertFalse(log.msg.contains(metadata.name));assertFalse(log.msg.contains(metadata.hash))
                assertFalse(log.msg.contains("content://"))
            }
        }finally {second.close();context.deleteDatabase(name)}
    }
    @Test fun unpairPagesOnlyUnfinishedRowsAndNeverProbesCompletedHistory()=runBlocking<Unit>(Dispatchers.IO) {
        val room=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
        try {
            val q=room.queue();val storage=Storage();val journal=ReceiveJournal(q,storage)
            room.runInTransaction {
                repeat(10000) {n->q.insertReceivedFile(row().apply {transferId="complete-$n";uri="visible-$n";state="complete"})}
                repeat(ReceiveJournal.CLEANUP_PAGE*2+1) {n->q.insertReceivedFile(row().apply {transferId="partial-${n.toString().padStart(4,'0')}";uri="pending-$n";storage.rows[uri]=true})}
            }
            val other=reserve(journal,storage,q,b)
            journal.forget(a)
            assertEquals(ReceiveJournal.CLEANUP_PAGE*2+1,storage.probes)
            assertEquals(setOf(other.uri),storage.rows.keys)
            assertEquals(ReceiveJournal.CLEANUP_PAGE,q.unfinishedPeerReceivedFiles(a,"",ReceiveJournal.CLEANUP_PAGE).size)
        }finally {room.close()}
    }

}
