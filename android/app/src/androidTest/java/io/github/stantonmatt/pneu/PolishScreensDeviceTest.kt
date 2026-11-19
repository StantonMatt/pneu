package io.github.stantonmatt.pneu

import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.unit.Density
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

/** Render production composables at the target phone size; the host captures via Device panel. */
@RunWith(AndroidJUnit4::class)
class PolishScreensDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation()
    private val context=i.targetContext
    private val app=context.graph
    private val old=Peer("Review PC","a".repeat(64),"a".repeat(32),emptyList(),generation=1,lastConnected=System.currentTimeMillis()-86400000)
    private val fresh=old.copy(pin="b".repeat(64),deviceId="b".repeat(32),generation=2)
    @Before fun reset()=runBlocking<Unit> {DeviceFixture.reset();app.initialized.await()}
    @After fun cleanup()=runBlocking<Unit> {File(context.filesDir,"polish-ready").delete();File(context.filesDir,"polish-release").delete();DeviceFixture.reset()}
    private fun gate(name:String) {
        if(InstrumentationRegistry.getArguments().getString("polishCaptures")!="true")return
        val release=File(context.filesDir,"polish-release");release.delete()
        File(context.filesDir,"polish-ready").writeText(name)
        runBlocking {withTimeout(120000) {while(!release.exists())delay(50)}}
        release.delete()
    }
    private fun readable(text:String,wrap:Boolean=false) {
        val node=compose.onNodeWithText(text,useUnmergedTree=true).assertIsDisplayed()
        val results=mutableListOf<TextLayoutResult>()
        node.performSemanticsAction(androidx.compose.ui.semantics.SemanticsActions.GetTextLayoutResult) {it(results)}
        assertTrue("Text layout exists: $text",results.isNotEmpty())
        val layout=results.single()
        if(wrap)assertFalse("Full status visible: $text; lines=${layout.lineCount}, size=${layout.size}, widthOverflow=${layout.didOverflowWidth}, heightOverflow=${layout.didOverflowHeight}",layout.hasVisualOverflow)
        if(wrap)assertTrue("Status wraps: $text",results.single().lineCount>=2)
    }
    @Test(timeout=600000) fun phonePolishInBothThemesAndLargeFont()=runBlocking<Unit> {
        var scene by mutableStateOf("initial")
        var dark by mutableStateOf(false)
        var font by mutableFloatStateOf(1f)
        fun fixture(state:String,error:String)=Outgoing().apply {mediaId="fixture";transferId="fixture";peerId=old.deviceId;bindingGeneration=1;fileName="Wedding footage";kind="file";automatic=false;size=432_000_000;sentBytes=265_000_000;resumable=true;createdAt=System.currentTimeMillis();this.state=state;this.error=error}
        var row by mutableStateOf(fixture("failed",ReinstallReplacement.CANCELLED))
        val p=app.preferences.read().copy(setup=true,peers=listOf(old))
        val sending=LinkState("Connected","Wi-Fi",sent=262_000_000,total=row.size,transferMediaId=row.mediaId,transferKind="file",transferFileName=row.fileName)
        val apps=listOf("Camera" to "com.android.camera2","Calendar" to "com.google.android.calendar","Messages" to "com.google.android.apps.messaging").map { (name,pkg)->
            NotificationPickerApp(pkg,name,runCatching {context.packageManager.getApplicationIcon(pkg)}.getOrElse {context.getDrawable(R.drawable.ic_notifications)!!})
        }
        compose.setContent {
            val base=LocalDensity.current
            CompositionLocalProvider(LocalDensity provides Density(base.density,font),LocalUiActions provides UiActions {message,_->fail("Unexpected action: $message")}) {
                PneuTheme(dark=dark) {Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {
                    when(scene) {
                        "device-sending","device-done"->DeviceScreen(p,DeviceState(old,if(scene=="device-sending")sending else LinkState("Connected","Wi-Fi"),emptySet(),true),{},{},{},{}) {_,_->}
                        "apps"->NotificationsContent(p.copy(notificationConsent=NotificationConsent(true,true,false,setOf(NotificationConsent.computer(old)))),true,false,apps,{},{},{},{_,_,_->},{})
                        "blocked","adb"->NotificationsContent(p,false,restrictedNotificationSource(if(scene=="blocked")android.content.pm.PackageInstaller.PACKAGE_SOURCE_LOCAL_FILE else android.content.pm.PackageInstaller.PACKAGE_SOURCE_UNSPECIFIED),emptyList(),{},{},{},{_,_,_->},{})
                        else->{
                            val peers=if(scene=="replacement-offline")listOf(old,fresh) else listOf(old)
                            val prefs=p.copy(peers=peers)
                            val links=if(scene=="replacement-offline")mapOf(old.deviceId to LinkState(ConnectionStatus.FINGERPRINT_MISMATCH.key,mismatchPin=fresh.pin),fresh.deviceId to LinkState("Connected","Wi-Fi")) else mapOf(old.deviceId to if(scene=="sending")sending else LinkState("Connected","Wi-Fi"))
                            val outcomes=RecentProjection.items(listOf(row),prefs,links)
                            HomeScreen(prefs,if(scene=="sending")sending else LinkState("Connected","Wi-Fi"),listOf(row),Health(true,true,true,true),Modifier,{},{},{},outcomes.associateBy {it.row.mediaId},DeviceProjection.states(prefs,links),links)
                        }
                    }
                }}
            }
        }
        for(theme in listOf("light","dark"))for(state in listOf("replaced","failed","sending","device-sending","device-done","apps","replacement-offline","blocked","adb")) {
            compose.runOnIdle {
                dark=theme=="dark";scene=state;font=1f
                row=fixture(if(state=="sending")"sending" else "failed",if(state=="replaced")ReinstallReplacement.CANCELLED else "connection_lost")
            }
            compose.activityRule.scenario.onActivity {activity->
                val style=if(theme=="dark")SystemBarStyle.dark(android.graphics.Color.TRANSPARENT) else SystemBarStyle.light(android.graphics.Color.TRANSPARENT,android.graphics.Color.TRANSPARENT)
                activity.enableEdgeToEdge(style,style)
            }
            compose.waitForIdle()
            gate("$state-$theme")
            when(state) {
                "replaced"->readable("Not sent · old device replaced",true)
                "failed"->readable("Connection lost · 265 of 432 MB sent",true)
                "sending"->{readable("Sending · 262 of 432 MB");compose.onNodeWithText("1 waiting").assertDoesNotExist()}
                "device-sending"->readable("Sending Wedding footage · 262 of 432 MB")
                "device-done"->{compose.onNodeWithText("Connected").assertIsDisplayed();compose.onNodeWithText("Sending Wedding footage · 262 of 432 MB").assertDoesNotExist()}
                "apps"->compose.onNodeWithText("Camera").assertIsDisplayed()
                "replacement-offline"->{compose.onNodeWithText("Not connected").assertIsDisplayed();compose.onNodeWithText("Couldn't verify computer").assertDoesNotExist()}
                "blocked"->compose.onNodeWithText("Android blocked notification access").assertIsDisplayed()
                "adb"->compose.onNodeWithText("Android blocked notification access").assertDoesNotExist()
            }
            if(state=="replaced" || state=="failed") {
                compose.runOnIdle {font=1.3f};compose.waitForIdle()
                gate("$state-$theme-font130")
                readable(if(state=="replaced")"Not sent · old device replaced" else "Connection lost · 265 of 432 MB sent",true)
            }
        }
        File(context.filesDir,"polish-ready").writeText("complete")
    }
}
