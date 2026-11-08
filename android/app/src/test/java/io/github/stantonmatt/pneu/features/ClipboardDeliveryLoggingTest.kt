package io.github.stantonmatt.pneu.features

import android.app.Application
import android.util.Log
import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import androidx.datastore.preferences.core.stringPreferencesKey
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.test.StandardTestDispatcher
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowLog
import java.io.File
import java.io.IOException

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ClipboardDeliveryLoggingTest {
    private val peer="b".repeat(32)
    private val text="private fixture clip that must never reach logcat"
    private val privateMessage="private-file.png content://private/provider /private/path pairing-secret"
    private suspend fun fixture(test:suspend (AppGraph,DeliveryBinding)->Unit) {
        val context=RuntimeEnvironment.getApplication()
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val file=File(context.cacheDir,"clipboard-logging-${randomId()}.preferences_pb")
        val preferences=Preferences(context,PreferenceDataStoreFactory.create(scope=owner) {file})
        // Keep startup/network jobs idle. Every fixture refuses before saved history.
        val app=AppGraph(context,preferences,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
        try {
            preferences.edit {p->
                p[stringPreferencesKey("deviceId")]="a".repeat(32)
                PeerRegistry.write(p,listOf(Peer("Fixture PC","b".repeat(64),peer,emptyList(),generation=7)))
            }
            val binding=requireNotNull(DeliveryBinding.from(preferences.read(),peer))
            ShadowLog.clear()
            withContext(Dispatchers.IO) {test(app,binding)}
        } finally {
            preferences.beforeReadStart={};preferences.beforeMutation={}
            app.scope.cancel();app.database.close();app.savedClips.database.close()
            owner.coroutineContext[Job]!!.cancelAndJoin()
            context.deleteDatabase("outgoing.db");context.deleteDatabase("clipboard-history.db");file.delete()
        }
    }
    private fun body()=json("transfer_id" to randomId(),"text" to text,"created_at" to System.currentTimeMillis(),
        "origin" to "clipboard","hlc" to System.currentTimeMillis().toString(),"source_device" to peer)
    private fun assertWarning(id:String,phase:String,exception:String,code:String="storage_failed"):String {
        val logs=ShadowLog.getLogsForTag("PneuClipboard")
        val line=logs.single {it.msg.contains("phase=$phase ")}
        assertEquals(Log.WARN,line.type)
        assertTrue(line.msg.contains("peer=$peer transfer_id=$id kind=text"))
        assertTrue(line.msg.contains("code=$code"));assertTrue(line.msg.contains(exception))
        for(log in logs) {
            assertNull(log.throwable)
            assertFalse(log.msg.contains(text));assertFalse(log.msg.contains(sha256(text.toByteArray())))
            assertFalse(log.msg.contains(privateMessage));assertFalse(log.msg.contains("private-file.png"))
            assertFalse(log.msg.contains("content://"));assertFalse(log.msg.contains("/private/path"))
            assertFalse(log.msg.contains("pairing-secret"))
        }
        return line.msg
    }
    @Test fun postEffectPreferenceFailureLogsPhaseAndCauseClassesWithoutContent()=runBlocking<Unit> {fixture {app,binding->
        app.preferences.beforeMutation={if(it=="clipboardApplied")throw IOException(privateMessage,IllegalStateException(text))}
        var effects=0
        val body=body()
        val reply=ClipboardDelivery(app) {_,_->effects++}.receive(peer,body,binding,true)
        assertEquals(Type.NACK,reply.type);assertEquals("storage_failed",reply.body["code"].asString)
        assertTrue(reply.body["retryable"].asBoolean);assertEquals(1,effects)
        assertEquals("reserved",app.queue.receipt(peer,body["transfer_id"].asString).state)
        val line=assertWarning(body["transfer_id"].asString,"applied_prefs","java.io.IOException")
        assertTrue(line.contains("java.io.IOException->java.lang.IllegalStateException"))
    }}
    @Test fun bindingReadFailureLogsOuterCatchWithoutChangingReply()=runBlocking<Unit> {fixture {app,binding->
        app.preferences.beforeReadStart={throw IOException(privateMessage)}
        val body=body()
        val reply=ClipboardDelivery(app) {_,_->fail("Binding read failed before apply")}.receive(peer,body,binding,true)
        assertEquals("storage_failed",reply.body["code"].asString)
        assertWarning(body["transfer_id"].asString,"binding_prefs","java.io.IOException")
    }}
    @Test fun clipboardUnavailableLogsClassAndPreservesCleanup()=runBlocking<Unit> {fixture {app,binding->
        val body=body()
        val reply=ClipboardDelivery(app) {_,_->throw SecurityException(privateMessage)}.receive(peer,body,binding,true)
        assertEquals("clipboard_unavailable",reply.body["code"].asString)
        assertEquals(0,app.queue.receiptCount())
        assertWarning(body["transfer_id"].asString,"apply","java.lang.SecurityException","clipboard_unavailable")
    }}
    @Test fun failedApplyCleanupLogsStoragePhaseWithoutChangingRefusal()=runBlocking<Unit> {fixture {app,binding->
        val db=app.database.openHelper.writableDatabase
        db.execSQL("CREATE TRIGGER fail_cleanup BEFORE DELETE ON receipts BEGIN SELECT RAISE(ABORT,'private cleanup'); END")
        try {
            val body=body()
            val reply=ClipboardDelivery(app) {_,_->throw SecurityException(privateMessage)}.receive(peer,body,binding,true)
            assertEquals("storage_failed",reply.body["code"].asString)
            assertEquals(1,app.queue.receiptCount())
            assertWarning(body["transfer_id"].asString,"apply_cleanup","android.database.sqlite.SQLiteConstraintException")
        } finally {db.execSQL("DROP TRIGGER fail_cleanup")}
    }}
}
