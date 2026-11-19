package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import androidx.room.Room
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.connection.QueueSchedule
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.data.Preferences
import io.github.stantonmatt.pneu.protocol.FOLDER_MIME
import io.github.stantonmatt.pneu.protocol.randomId
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.StandardTestDispatcher
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class FolderGrantLifecycleTest {
    private val uri="content://folders/tree/root"
    private fun folder()=Outgoing().apply {
        mediaId="folder:test";transferId="transfer";peerId="peer";bindingGeneration=7
        origin="folder";kind="file";mime=FOLDER_MIME;this.uri=this@FolderGrantLifecycleTest.uri
        automatic=false;resumable=true;attempted=true;sentBytes=167
    }
    private suspend fun retained(queue:QueueDao,expected:Boolean) {
        val uris=if(expected)listOf(uri) else emptyList()
        assertEquals("Restart reconciliation",uris,queue.activeFolderUris())
        assertEquals("Live reconciliation",uris,withTimeout(5000) {queue.folderGrantChanges().first()})
    }
    @Test(timeout=30000) fun offlineQueueDisabledConnectionLossKeepsGrantAcrossRestartAndManualRetryResumes()=runBlocking<Unit>(Dispatchers.IO) {
        val context=RuntimeEnvironment.getApplication()
        val file=File(context.cacheDir,"folder-grant-${randomId()}.preferences_pb")
        val owner=SupervisorJob()
        val preferences=Preferences(context,PreferenceDataStoreFactory.create(scope=CoroutineScope(owner+Dispatchers.IO)) {file})
        // Keep background services stopped while exercising real recovery/Retry.
        fun open()=AppGraph(context,preferences,CoroutineScope(SupervisorJob()+StandardTestDispatcher())).also {
            // Recovery is invoked explicitly; the frozen background scope never
            // runs AppGraph startup, while Retry now correctly awaits that fence.
            it.initialized.complete(Unit)
        }
        var app=open()
        try {
            preferences.toggle("queue",false)
            app.queue.insert(folder().apply {state="sending"})
            app.recoverOutgoing()
            val failed=app.queue.delivery("folder:test","peer")
            assertEquals("failed",failed.state);assertEquals("connection_lost",failed.error)
            assertEquals(Long.MAX_VALUE,failed.nextAttempt)
            retained(app.queue,true)
            app.scope.cancel();app.database.close()
            app=open()
            retained(app.queue,true)
            assertNull(QueueSchedule.choose(app.queue.pending(),0,{it.nextAttempt}).row)
            assertTrue(app.retry("folder:test"))
            val resumed=requireNotNull(QueueSchedule.choose(app.queue.pending(),0,{it.nextAttempt}).row)
            assertEquals("waiting",resumed.state);assertEquals("",resumed.error)
            assertEquals("transfer",resumed.transferId);assertEquals(167L,resumed.sentBytes)
            assertEquals(uri,resumed.uri);retained(app.queue,true)
            resumed.state="sent";app.queue.update(resumed)
            retained(app.queue,false)
        }finally {
            app.scope.cancel();app.database.close();context.deleteDatabase("outgoing.db")
            owner.cancelAndJoin();file.delete()
        }
    }
    @Test fun retryableFailuresRetainAccessRegardlessOfAutomaticRetryDeadline()=runBlocking<Unit>(Dispatchers.IO) {
        val db=Room.inMemoryDatabaseBuilder(RuntimeEnvironment.getApplication(),QueueDatabase::class.java).build()
        try {
            val queue=db.queue();val row=folder().apply {state="failed"};queue.insert(row)
            for(error in listOf("connection_lost","local_read","local_storage","storage_failed","hash_mismatch","size_mismatch")) {
                for(deadline in listOf(60000L,Long.MAX_VALUE)) {
                    row.error=error;row.nextAttempt=deadline;queue.update(row);retained(queue,true)
                }
            }
        }finally {db.close()}
    }
    @Test fun finalFailuresAndRemovedRowsReleaseOnlyWhenNoOtherDeliveryNeedsTheTree()=runBlocking<Unit>(Dispatchers.IO) {
        val db=Room.inMemoryDatabaseBuilder(RuntimeEnvironment.getApplication(),QueueDatabase::class.java).build()
        try {
            val queue=db.queue();val row=folder().apply {state="failed";nextAttempt=Long.MAX_VALUE};queue.insert(row)
            for(error in listOf("folder_changed","folder_access_lost","folder_update_required","unsupported_kind","invalid_payload","id_conflict","local_file_missing")) {
                row.error=error;queue.update(row);retained(queue,false)
            }
            row.error="connection_lost";queue.update(row)
            val other=folder().apply {peerId="other";state="waiting"};queue.insert(other)
            row.state="discarded";queue.update(row);retained(queue,true)
            other.state="sent";queue.update(other);retained(queue,false)
            row.state="waiting";queue.update(row);retained(queue,true)
            queue.deleteOutgoing(row.mediaId);retained(queue,false)
        }finally {db.close()}
    }
}
