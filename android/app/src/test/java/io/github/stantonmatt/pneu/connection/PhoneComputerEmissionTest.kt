package io.github.stantonmatt.pneu.connection

import android.app.Application
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.NetworkInfo
import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.data.Preferences
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
import org.robolectric.shadows.ShadowNetworkInfo
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.OutputStream
import java.lang.reflect.InvocationTargetException
import java.net.Socket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.coroutines.intrinsics.suspendCoroutineUninterceptedOrReturn

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class PhoneComputerEmissionTest {
    private class Wire:Socket() {
        val entered=CountDownLatch(1)
        val release=CountDownLatch(1)
        val bytes=ByteArrayOutputStream()
        var stalled=false
        private val output=object:OutputStream() {
            override fun write(value:Int) {
                if(stalled) {entered.countDown();check(release.await(10,TimeUnit.SECONDS)) {"Writer not released"}}
                bytes.write(value)
            }
            override fun write(b:ByteArray,off:Int,len:Int) {
                if(stalled) {entered.countDown();check(release.await(10,TimeUnit.SECONDS)) {"Writer not released"}}
                bytes.write(b,off,len)
            }
        }
        override fun getOutputStream()=output
    }
    private fun ConnectionManager.field(name:String,value:Any?) {
        ConnectionManager::class.java.getDeclaredField(name).apply {isAccessible=true}.set(this,value)
    }
    // Exercise the actual frame admission/write path without a live TLS peer.
    private suspend fun ConnectionManager.sendPhone(frame:Frame.Control,binding:DeliveryBinding):Boolean=
        suspendCoroutineUninterceptedOrReturn {continuation->
            try {
                ConnectionManager::class.java.declaredMethods.single {it.name=="send"}.apply {isAccessible=true}
                    .invoke(this,frame,null,binding,continuation)
            } catch(e:InvocationTargetException) {throw e.targetException}
        }
    private suspend fun fixture(body:suspend (AppGraph,ConnectionManager,DeliveryBinding,Wire)->Unit) {
        val context=RuntimeEnvironment.getApplication()
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val file=File(context.cacheDir,"phone-emission-${randomId()}.preferences_pb")
        val prefs=Preferences(context,PreferenceDataStoreFactory.create(scope=owner) {file})
        val graph=AppGraph(context,prefs,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
        val wire=Wire()
        try {
            val peer=Peer("PC","b".repeat(64),"b".repeat(32),emptyList(),generation=7)
            prefs.edit {p->p[stringPreferencesKey("deviceId")]="a".repeat(32);PeerRegistry.write(p,listOf(peer))}
            val cm=context.getSystemService(ConnectivityManager::class.java)
            shadowOf(cm).setActiveNetworkInfo(ShadowNetworkInfo.newInstance(NetworkInfo.DetailedState.CONNECTED,
                ConnectivityManager.TYPE_WIFI,0,true,true))
            val capabilities=NetworkCapabilities()
            shadowOf(capabilities).addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
            shadowOf(cm).setNetworkCapabilities(requireNotNull(cm.activeNetwork),capabilities)
            val connection=ConnectionManager(graph,peer.pin)
            val binding=requireNotNull(DeliveryBinding.from(prefs.read()))
            connection.field("socket",wire);connection.field("ready",true)
            connection.field("admissionSession","session");connection.field("authenticatedBinding",binding)
            connection.field("capabilities",setOf("mesh",CAP_BATTERY,CAP_RING))
            connection.field("activeEndpoint",Endpoint("127.0.0.1",1234))
            body(graph,connection,binding.copy(sessionId="session"),wire)
        } finally {
            wire.release.countDown();wire.close();graph.scope.cancel();graph.database.close()
            owner.coroutineContext[Job]!!.cancelAndJoin();context.deleteDatabase("outgoing.db");file.delete()
        }
    }
    private suspend fun stalledWriter(type:Type)=fixture {app,connection,binding,wire->
        coroutineScope {
            wire.stalled=true
            val frame=if(type==Type.BATTERY_STATE)Frame.Control(type,json("level" to 42,"charging" to false))
                else Frame.Control(type,json("ringing" to false))
            val writer=async(Dispatchers.IO) {connection.sendPhone(frame,binding)}
            try {
                assertTrue("Frame must reach blocking socket I/O",wire.entered.await(5,TimeUnit.SECONDS))
                withTimeout(1500) {
                    app.preferences.toggle("paused",true)
                    app.preferences.toggle("showBatteryLevel",false)
                    app.preferences.toggle("allowRinging",false)
                    app.withBindingLifecycle {assertTrue(app.preferences.read().paused)}
                }
                assertFalse("Privacy controls must finish while the writer is stalled",writer.isCompleted)
                val current=app.preferences.read()
                assertTrue(current.paused);assertFalse(current.showBatteryLevel);assertFalse(current.allowRinging)
            } finally {wire.release.countDown()}
            assertTrue(writer.await())
        }
    }
    @Test fun stalledBatteryWriteDoesNotBlockPrivacyControls()=runBlocking(Dispatchers.IO) {stalledWriter(Type.BATTERY_STATE)}
    @Test fun stalledRingWriteDoesNotBlockPrivacyControls()=runBlocking(Dispatchers.IO) {stalledWriter(Type.RING_STATE)}

    @Test fun batteryWaitingForWriterUsesCurrentPolicy()=runBlocking(Dispatchers.IO) {
        fixture {app,connection,binding,wire->
            coroutineScope {
                val queued=CompletableDeferred<Deferred<Boolean>>()
                connection.withWriteLockForTest {
                    queued.complete(async(start=CoroutineStart.UNDISPATCHED) {
                        connection.sendPhone(Frame.Control(Type.BATTERY_STATE,json("level" to 42,"charging" to false)),binding)
                    })
                    app.preferences.toggle("showBatteryLevel",false)
                }
                assertTrue(queued.await().await())
                val sent=Protocol.read(wire.bytes.toByteArray().inputStream()) as Frame.Control
                assertEquals(Type.BATTERY_STATE,sent.type);assertEquals(json(),sent.body)
            }
        }
    }
    @Test fun revokedSessionWaitingForWriterEmitsNothing()=runBlocking(Dispatchers.IO) {
        fixture {_,connection,binding,wire->
            coroutineScope {
                val queued=CompletableDeferred<Deferred<Boolean>>()
                connection.withWriteLockForTest {
                    queued.complete(async(start=CoroutineStart.UNDISPATCHED) {
                        connection.sendPhone(Frame.Control(Type.RING_STATE,json("ringing" to false)),binding)
                    })
                    connection.bindingChanged()
                }
                assertFalse(queued.await().await());assertEquals(0,wire.bytes.size())
            }
        }
    }
}
