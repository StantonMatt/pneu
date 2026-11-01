package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.data.Preferences
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.setMain
import kotlinx.coroutines.test.resetMain
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
@OptIn(ExperimentalCoroutinesApi::class)
class ResumeAdmissionCleanupTest {
    private val id="f".repeat(32)
    private val meta=ReceivedMetadata(id,"partial.bin","application/octet-stream",100,sha256(ByteArray(100)),"file",123)
    private class Storage:ReceiveStorage {
        var available=Long.MAX_VALUE
        override fun checkSpace(bytes:Long,reserve:Long) {if(bytes>available-reserve)throw ReceiveStorageException("disk_full")}
        override fun create(metadata:ReceivedMetadata):Pair<String,String> = error("No publication expected")
        override fun open(uri:String):ReceiveOutput=error("No publication expected")
        override fun pending(uri:String):Boolean?=null
        override fun publish(uri:String) {error("No publication expected")}
        override fun deletePending(uri:String) {}
        override fun cleanOrphans(retained:Set<String>) {}
    }
    private suspend fun fixture(body:suspend (AppGraph,DeliveryBinding,File)->Unit) {
        val context=RuntimeEnvironment.getApplication()
        val file=File(context.cacheDir,"resume-review-${randomId()}.preferences_pb")
        val owner=SupervisorJob()
        val preferences=Preferences(context,PreferenceDataStoreFactory.create(scope=CoroutineScope(owner+Dispatchers.IO)) {file})
        preferences.edit {p->
            p[stringPreferencesKey("deviceId")]="1".repeat(32)
            PeerRegistry.write(p,listOf(Peer("PC","a".repeat(64),"a".repeat(32),emptyList(),generation=7)))
        }
        val app=AppGraph(context,preferences,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
        val root=File(context.filesDir,"review-spool-${randomId()}")
        Dispatchers.setMain(Dispatchers.Unconfined)
        try {withTimeout(15000) {body(app,requireNotNull(DeliveryBinding.from(preferences.read())),root)}}
        finally {Dispatchers.resetMain();app.scope.cancel();app.database.close();context.deleteDatabase("outgoing.db");owner.cancelAndJoin();file.delete();root.deleteRecursively()}
    }
    private fun begin()=com.google.gson.JsonParser.parseString(meta.canonical).asJsonObject.apply {addProperty("resume",true)}
    private fun machine(files:ReceivedFiles,binding:DeliveryBinding)=files.connection(CapturedAuthority(binding),false,true) {true}
    @Test fun retainedPrefixRequiresSuffixAndPublicationSpaceBeforeReady()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {app,binding,root->
            val storage=Storage();val spool=ReceiveSpool(root)
            var available=Long.MAX_VALUE
            val files=ReceivedFiles(app,storage,spool=spool,spoolSpace={available})
            val first=machine(files,binding);assertEquals(Type.FILE_READY,first.begin(begin())!!.type)
            first.chunk(Frame.Chunk(id,0,ByteArray(40)));first.disconnect()
            assertEquals(40L,app.queue.receivedFile(binding.pcId,id).checkpoint)
            // 60 suffix + 100 publication bytes, after the reserve.
            available=ReceivedFiles.SPACE_RESERVE+159
            val privateFull=machine(files,binding).begin(begin())!!
            assertEquals(Type.NACK,privateFull.type);assertEquals("disk_full",privateFull.body["code"].asString)
            available=ReceivedFiles.SPACE_RESERVE+160
            storage.available=ReceivedFiles.SPACE_RESERVE+99
            val publicFull=machine(files,binding).begin(begin())!!
            assertEquals(Type.NACK,publicFull.type);assertEquals("disk_full",publicFull.body["code"].asString)
            storage.available=ReceivedFiles.SPACE_RESERVE+100
            val resumed=machine(files,binding)
            assertEquals(40L,resumed.begin(begin())!!.body["offset"].asLong)
            resumed.cancel(id)
        }
    }
    @Test fun failedUnlinkKeepsDurableBudgetOwnershipAndReleasesTheLeaseForRetry()=runBlocking<Unit>(Dispatchers.IO) {
        fixture {app,binding,root->
            var rejectDelete=true
            val spool=ReceiveSpool(root,beforeDelete={if(rejectDelete)throw java.io.IOException("Injected unlink failure")})
            val files=ReceivedFiles(app,Storage(),spool=spool)
            val first=machine(files,binding);first.begin(begin());first.chunk(Frame.Chunk(id,0,ByteArray(40)))
            // END fails size verification and invokes abort, whose unlink throws.
            assertEquals("size_mismatch",first.end(id).body["code"].asString)
            val retained=app.queue.receivedFile(binding.pcId,id)
            assertNotNull("Failed unlink must keep its durable owner",retained)
            assertNotNull(app.queue.receipt(binding.pcId,id))
            assertTrue(File(retained.stagePath).isFile)
            rejectDelete=false
            // A stale lease would refuse this admission. Recovery truncates to 0.
            val retry=machine(files,binding)
            assertEquals(Type.FILE_READY,retry.begin(begin())!!.type)
            retry.cancel(id)
            assertNull(app.queue.receivedFile(binding.pcId,id));assertNull(app.queue.receipt(binding.pcId,id))
            assertTrue(root.listFiles().orEmpty().isEmpty())
        }
    }
}
