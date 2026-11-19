package io.github.stantonmatt.pneu

import android.Manifest
import android.app.NotificationManager
import android.media.AudioAttributes
import android.media.AudioManager
import android.content.Intent
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Surface
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class PhoneComputerDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val peers=mutableListOf<LocalPeerFixture>()
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))
            i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,false)
        app.preferences.toggle("setup",true)
    }
    @After fun cleanup()=runBlocking<Unit> {
        i.runOnMainSync {app.ringing.stop(RingStopSource.SERVICE_STOP)}
        DeviceFixture.stopPneuService();app.connection.stop();app.connection.awaitStopped()
        peers.forEach {it.close()};DeviceFixture.reset()
    }
    private suspend fun peer(id:Char,frames:Channel<Frame.Control>,hellos:Channel<Frame.Control>,caps:Set<String>):LocalPeerFixture {
        val uri="pneu://pair?v=1&name=Studio%20PC&fp=${id.toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"c".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(uri,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(uri),id.toString().repeat(32)))
        withTimeout(5000) {app.prefs.first {it.devices.any {p->p.deviceId==id.toString().repeat(32)}}}
        return LocalPeerFixture(app,capabilities=caps,onHello={hellos.trySend(it)},handle={frame,_->
            if(frame.type in setOf(Type.BATTERY_STATE,Type.RING_STATE,Type.PONG,Type.ERROR)) {frames.trySend(frame);true} else false
        }).also {peers+=it;it.connect(id.toString().repeat(32),holdFirstFrame=false)}
    }
    private val pending=mutableListOf<Pair<Channel<Frame.Control>,Frame.Control>>()
    private suspend fun next(frames:Channel<Frame.Control>,type:Type,predicate:(Frame.Control)->Boolean={true}):Frame.Control=withTimeout(10000) {
        val cached=pending.indexOfFirst {it.first===frames && it.second.type==type && predicate(it.second)}
        if(cached>=0)return@withTimeout pending.removeAt(cached).second
        while(true) {
            val frame=frames.receive()
            if(frame.type==type) {if(predicate(frame))return@withTimeout frame}
            else pending+=frames to frame
        }
        @Suppress("UNREACHABLE_CODE") error("unreachable")
    }
    @Test fun liveRingUsesAlarmMaximumAndNotificationStopRestoresVolume()=runBlocking<Unit> {
        ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("home",true)).use {
            val frames=Channel<Frame.Control>(Channel.UNLIMITED);val hellos=Channel<Frame.Control>(Channel.UNLIMITED)
            val peer=peer('a',frames,hellos,setOf("files","clipboard_text","clipboard_image","mesh",CAP_RING,CAP_BATTERY))
            PneuService.start(c)
            withTimeout(5000) {while(!withContext(Dispatchers.Main) {app.ringing.serviceAttached})delay(20)}
            val hello=withTimeout(5000) {hellos.receive()}
            assertTrue(hello.body["capabilities"].asJsonArray.any {it.asString==CAP_RING})
            next(frames,Type.BATTERY_STATE);next(frames,Type.RING_STATE) {!it.body["ringing"].asBoolean}
            val audio=c.getSystemService(AudioManager::class.java)
            val original=audio.getStreamVolume(AudioManager.STREAM_ALARM)
            val previous=(audio.getStreamMaxVolume(AudioManager.STREAM_ALARM)/2).coerceAtLeast(1)
            audio.setStreamVolume(AudioManager.STREAM_ALARM,previous,0)
            try {
                peer.sendControl(Frame.Control(Type.RING,json("action" to "start")))
                next(frames,Type.RING_STATE) {it.body["ringing"].asBoolean}
                assertTrue(app.ringing.playing)
                assertEquals(AudioAttributes.USAGE_ALARM,app.ringing.audioAttributes!!.usage)
                assertEquals(audio.getStreamMaxVolume(AudioManager.STREAM_ALARM),audio.getStreamVolume(AudioManager.STREAM_ALARM))
                // Repeated start must retain the original restoration volume.
                peer.sendControl(Frame.Control(Type.RING,json("action" to "start")))
                next(frames,Type.RING_STATE) {it.body["ringing"].asBoolean}
                assertEquals(previous,c.getSharedPreferences("ring-volume",0).getInt("previous",-1))
                val notification=c.getSystemService(NotificationManager::class.java).activeNotifications.single {it.id==PhoneRinging.NOTIFICATION_ID}.notification
                assertEquals("Pneu is ringing",notification.extras.getString("android.title"))
                notification.actions.single {it.title.toString()=="Stop"}.actionIntent.send()
                next(frames,Type.RING_STATE) {!it.body["ringing"].asBoolean}
                assertFalse(app.ringing.playing);assertEquals(previous,audio.getStreamVolume(AudioManager.STREAM_ALARM))
                assertFalse(c.getSharedPreferences("ring-volume",0).contains("previous"))
                app.preferences.toggle("showBatteryLevel",false)
                assertEquals(0,next(frames,Type.BATTERY_STATE).body.size())
                peer.sendControl(Frame.Control(Type.RING,json("action" to "start")))
                next(frames,Type.RING_STATE) {it.body["ringing"].asBoolean}
                app.preferences.toggle("allowRinging",false)
                next(frames,Type.RING_STATE) {!it.body["ringing"].asBoolean}
                assertFalse(app.ringing.playing);assertEquals(previous,audio.getStreamVolume(AudioManager.STREAM_ALARM))
                val updated=withTimeout(10000) {hellos.receive()}
                assertFalse(updated.body["capabilities"].asJsonArray.any {it.asString==CAP_RING})
            } finally {i.runOnMainSync {app.ringing.stop(RingStopSource.SERVICE_STOP)};audio.setStreamVolume(AudioManager.STREAM_ALARM,original,0)}
        }
    }
    @Test fun anyComputerCanStopAndPauseClearsAndDeniesStarts()=runBlocking<Unit> {
        ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("home",true)).use {
            val a=Channel<Frame.Control>(Channel.UNLIMITED);val b=Channel<Frame.Control>(Channel.UNLIMITED)
            val caps=setOf("files","clipboard_text","clipboard_image","mesh",CAP_RING,CAP_BATTERY)
            val first=peer('a',a,Channel(Channel.UNLIMITED),caps)
            val second=peer('b',b,Channel(Channel.UNLIMITED),caps)
            PneuService.start(c)
            withTimeout(5000) {while(!withContext(Dispatchers.Main) {app.ringing.serviceAttached})delay(20)}
            next(a,Type.RING_STATE);next(b,Type.RING_STATE)
            first.sendControl(Frame.Control(Type.RING,json("action" to "start")))
            next(a,Type.RING_STATE) {it.body["ringing"].asBoolean};next(b,Type.RING_STATE) {it.body["ringing"].asBoolean}
            second.sendControl(Frame.Control(Type.RING,json("action" to "stop")))
            next(a,Type.RING_STATE) {!it.body["ringing"].asBoolean};next(b,Type.RING_STATE) {!it.body["ringing"].asBoolean}
            first.sendControl(Frame.Control(Type.RING,json("action" to "start")))
            next(a,Type.RING_STATE) {it.body["ringing"].asBoolean};next(b,Type.RING_STATE) {it.body["ringing"].asBoolean}
            app.preferences.toggle("paused",true)
            next(a,Type.RING_STATE) {!it.body["ringing"].asBoolean};next(b,Type.RING_STATE) {!it.body["ringing"].asBoolean}
            next(a,Type.BATTERY_STATE) {it.body.size()==0};next(b,Type.BATTERY_STATE) {it.body.size()==0}
            assertFalse(app.ringing.playing)
            first.sendControl(Frame.Control(Type.RING,json("action" to "start")))
            next(a,Type.RING_STATE) {!it.body["ringing"].asBoolean};assertFalse(app.ringing.playing)
        }
    }
    @Test fun legacyPeerGetsNoExtensionFrames()=runBlocking<Unit> {
        val frames=Channel<Frame.Control>(Channel.UNLIMITED);val hellos=Channel<Frame.Control>(Channel.UNLIMITED)
        val peer=peer('a',frames,hellos,setOf("files","clipboard_text","clipboard_image"))
        peer.sendControl(Frame.Control(Type.PING,json("nonce" to "0".repeat(16))))
        val first=withTimeout(10000) {frames.receive()}
        assertEquals(Type.PONG,first.type)
        assertTrue(frames.tryReceive().isFailure)
    }
    private fun capture(name:String) {
        val background=if(name.endsWith("dark"))0xFF000000.toInt() else 0xFFF2F3F6.toInt()
        val deadline=android.os.SystemClock.elapsedRealtime()+5000
        while(true) {
            compose.waitForIdle()
            val bitmap=requireNotNull(i.uiAutomation.takeScreenshot())
            try {
                // Compose semantics can be ready before SurfaceFlinger presents
                // the first activity frame. Poll the committed background and
                // accent pixels so a blank launch window can never pass capture.
                var accent=0
                for(y in 100 until bitmap.height-100 step 12)for(x in 0 until bitmap.width step 12)
                    if(bitmap.getPixel(x,y)==0xFF4F5BFF.toInt())accent++
                if(bitmap.getPixel(0,bitmap.height/2)==background && accent>20) {
                    File(c.filesDir,"ring-battery-screens/$name.png").apply {parentFile!!.mkdirs()}.outputStream().use {assertTrue(bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG,100,it))}
                    return
                }
            } finally {bitmap.recycle()}
            check(android.os.SystemClock.elapsedRealtime()<deadline) {"Scene was not presented: $name"}
        }
    }
    private fun screen(dark:Boolean,settings:Boolean)=runBlocking<Unit> {
        ActivityScenario.launch(ComponentActivity::class.java).use {scene->
            val prefs=app.preferences.read();withTimeout(5000) {app.savedClips.state.first {it.loaded}}
            scene.onActivity {activity->
                val transparent=android.graphics.Color.TRANSPARENT
                val bars=if(dark)SystemBarStyle.dark(transparent) else SystemBarStyle.light(transparent,transparent)
                activity.enableEdgeToEdge(bars,bars)
                activity.setContent {
                    CompositionLocalProvider(LocalUiActions provides UiActions {message,_->fail(message)}) {
                        PneuTheme(dark) {Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {
                            if(settings)SettingsScreen(prefs,Health(true,true,true,true),0,{},{})
                            else RingingScreen("Studio PC") {}
                        }}
                    }
                }
            }
            if(settings)compose.onNodeWithText("Show battery level").performScrollTo()
            compose.onNodeWithText(if(settings)"Allow ringing" else "Pneu is ringing").assertIsDisplayed()
            capture("${if(settings)"settings" else "ringing"}-${if(dark)"dark" else "light"}")
        }
    }
    @Test fun settingsLight()=screen(false,true)
    @Test fun settingsDark()=screen(true,true)
    @Test fun ringingLight()=screen(false,false)
    @Test fun ringingDark()=screen(true,false)
}
