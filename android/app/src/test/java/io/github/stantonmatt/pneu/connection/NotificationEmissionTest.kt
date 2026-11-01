package io.github.stantonmatt.pneu.connection

import android.app.Application
import android.app.NotificationManager
import android.content.ComponentName
import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.data.Preferences
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.test.StandardTestDispatcher
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.OutputStream
import java.net.Socket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class NotificationEmissionTest {
    private class StalledSocket(private val beforeBytes:Boolean):Socket() {
        val entered=CompletableDeferred<Unit>()
        val release=CountDownLatch(1)
        val bytes=ByteArrayOutputStream()
        private fun stall() {
            entered.complete(Unit)
            check(release.await(10,TimeUnit.SECONDS)) {"Test did not release stalled socket"}
        }
        private val output=object:OutputStream() {
            override fun write(value:Int) {if(!beforeBytes && bytes.size()==0)stall();bytes.write(value)}
            override fun write(value:ByteArray,offset:Int,length:Int) {
                if(!beforeBytes && bytes.size()==0)stall()
                bytes.write(value,offset,length)
            }
        }
        override fun getOutputStream():OutputStream {if(beforeBytes)stall();return output}
        override fun close() {release.countDown()}
    }
    private class Fixture(val graph:AppGraph,val manager:ConnectionManager,val binding:DeliveryBinding,
                          val session:NotificationMirrorSession,val event:NotificationEvent,val socket:StalledSocket) {
        suspend fun send()=manager.sendNotification(binding,"owner",session,event)
    }
    private suspend fun fixture(beforeBytes:Boolean=true,body:suspend CoroutineScope.(Fixture)->Unit)=coroutineScope {
        val context=RuntimeEnvironment.getApplication()
        val storeOwner=SupervisorJob()
        val file=File(context.cacheDir,"notification-emission-${randomId()}.preferences_pb")
        val store=PreferenceDataStoreFactory.create(scope=CoroutineScope(storeOwner+Dispatchers.IO)) {file}
        val prefs=Preferences(context,store)
        val peer=Peer("PC","a".repeat(64),"b".repeat(32),emptyList(),generation=1)
        val socket=StalledSocket(beforeBytes)
        val graph=AppGraph(context,prefs,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
        try {
            store.edit {it[stringPreferencesKey("deviceId")]="c".repeat(32);PeerRegistry.write(it,listOf(peer))}
            prefs.notificationMaster(true);prefs.notificationSelection(false,"0/chat",true)
            shadowOf(context.getSystemService(NotificationManager::class.java)).setNotificationListenerAccessGranted(
                ComponentName(context,PhoneNotificationListener::class.java),true)
            graph.phoneNotifications.replace(emptyList())
            graph.phoneNotifications.post(MirrorNotice("local",0,"chat","Chat",1,title="Private",postSequence=1))
            val session=NotificationMirrorSession()
            session.committed(session.next(emptyList(),prefs.read().notificationConsent,true)!!)
            val event=session.next(graph.phoneNotifications.snapshot(),prefs.read().notificationConsent,true)!!
            assertEquals(NotificationAction.UPSERT,event.action)
            val manager=ConnectionManager(graph,peer.pin)
            fun field(name:String,value:Any) {ConnectionManager::class.java.getDeclaredField(name).apply {isAccessible=true}.set(manager,value)}
            field("socket",socket);field("ready",true);field("admissionSession","owner")
            body(Fixture(graph,manager,DeliveryBinding.from(prefs.read(),peer.deviceId)!!,session,event,socket))
        }finally {
            socket.close();graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")
            storeOwner.cancelAndJoin();file.delete()
        }
    }
    @Test fun stalledSocketWriteDoesNotBlockOptOutOrBindingOperations()=runBlocking<Unit> {
        fixture(beforeBytes=false) {f->
            val sending=async(Dispatchers.IO) {f.send()}
            try {
                withTimeout(5000) {f.socket.entered.await()}
                assertEquals("Opt-out waited for socket I/O",true,withTimeoutOrNull(2000) {
                    f.graph.preferences.notificationMaster(false);true
                })
                assertEquals("Binding operations waited for socket I/O",true,withTimeoutOrNull(2000) {
                    f.graph.withBindingLifecycle {true}
                })
                assertFalse(sending.isCompleted)
            }finally {f.socket.release.countDown();sending.await()}
        }
    }
    @Test fun stalledSocketWriteDoesNotBlockPause()=runBlocking<Unit> {
        fixture(beforeBytes=false) {f->
            val sending=async(Dispatchers.IO) {f.send()}
            try {
                withTimeout(5000) {f.socket.entered.await()}
                assertEquals("Pause waited for socket I/O",true,withTimeoutOrNull(2000) {
                    f.graph.preferences.toggle("paused",true);true
                })
                assertTrue(f.graph.preferences.read().paused)
                assertFalse(sending.isCompleted)
            }finally {f.socket.release.countDown();sending.await()}
        }
    }
    private suspend fun rejectedAfterChange(change:suspend (Fixture)->Unit)=fixture {f->
        val sending=async(Dispatchers.IO) {f.send()}
        try {
            withTimeout(5000) {f.socket.entered.await()}
            assertEquals("Mutation waited for frame admission",true,withTimeoutOrNull(2000) {change(f);true})
        }finally {f.socket.release.countDown()}
        assertFalse("Revoked admission reached the wire",sending.await())
        assertEquals(0,f.socket.bytes.size())
    }
    @Test fun admittedFrameSendsNoBytesAfterOptOutCompletes()=runBlocking<Unit> {
        rejectedAfterChange {it.graph.preferences.notificationMaster(false)}
    }
    @Test fun optOutThenReenableStillRejectsTheOldAdmission()=runBlocking<Unit> {
        rejectedAfterChange {it.graph.preferences.notificationMaster(false);it.graph.preferences.notificationMaster(true)}
    }
    @Test fun appOptOutThenReselectStillRejectsTheOldAdmission()=runBlocking<Unit> {
        rejectedAfterChange {f->
            f.graph.preferences.notificationSelection(false,"0/chat",false)
            f.graph.preferences.notificationSelection(false,"0/chat",true)
        }
    }
    @Test fun computerOptOutThenReselectStillRejectsTheOldAdmission()=runBlocking<Unit> {
        rejectedAfterChange {f->
            val key=NotificationConsent.computer(f.graph.preferences.read().devices.single())
            f.graph.preferences.notificationSelection(true,key,false)
            f.graph.preferences.notificationSelection(true,key,true)
        }
    }
    @Test fun namesOnlyRoundTripStillRejectsTheOldContentAdmission()=runBlocking<Unit> {
        rejectedAfterChange {f->
            f.graph.preferences.notificationNamesOnly(true)
            f.graph.preferences.notificationNamesOnly(false)
        }
    }
    @Test fun admittedFrameSendsNoBytesAfterPauseCompletes()=runBlocking<Unit> {
        rejectedAfterChange {it.graph.preferences.toggle("paused",true)}
    }
    @Test fun admittedFrameSendsNoBytesAfterBindingRemoval()=runBlocking<Unit> {
        rejectedAfterChange {f->f.graph.withBindingLifecycle {f.graph.preferences.forget(peerId=f.binding.pcId)}}
    }
    @Test fun unchangedAdmissionWritesOneValidNotificationFrame()=runBlocking<Unit> {
        fixture {f->
            f.socket.release.countDown()
            assertTrue(f.send())
            val frame=Protocol.read(f.socket.bytes.toByteArray().inputStream()) as Frame.Control
            assertEquals(Type.NOTIFICATION_EVENT,frame.type)
            assertEquals(f.event.key,frame.body["key"].asString)
        }
    }
}
