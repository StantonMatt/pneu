package io.github.stantonmatt.pneu

import android.Manifest
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.google.gson.Gson
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

/** Optional capture barrier lets the Device panel take the actual screenshots. */
@RunWith(AndroidJUnit4::class)
class FolderScreensDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation();private val app=i.targetContext.graph
    private var scene by mutableStateOf("button")
    private var dark by mutableStateOf(false)
    private fun clearCaptureFlags() {
        i.targetContext.filesDir.listFiles()?.filter {it.name=="folder-scene" || it.name.startsWith("folder-next-")}?.forEach {it.delete()}
    }
    @Before fun reset()=runBlocking {clearCaptureFlags();DeviceFixture.reset()}
    @After fun cleanup()=runBlocking {clearCaptureFlags();DeviceFixture.reset()}
    @Test(timeout=600000) fun buttonPreparingAndSkippedInBothThemes()=runBlocking<Unit> {
        val peer=Peer("MJStanton-Desktop","d".repeat(64),"d".repeat(32),emptyList())
        val p=app.preferences.read().copy(setup=true,peer=peer,peers=listOf(peer),paused=false,photos=false,recordings=false)
        val link=LinkState("Connected","Local network",capabilities=setOf("files",CAP_FILE_RESUME,CAP_FOLDER))
        fun row(name:String,skipped:FolderSkipped)=Outgoing().apply {
            mediaId=name;fileName=name;kind="file";origin="folder";mime=FOLDER_MIME;automatic=false;state="sent";size=1800000000;createdAt=System.currentTimeMillis();text=Gson().toJson(FolderSummary(165,1204,skipped))
        }
        val skipped=listOf(row("Camera backup",FolderSkipped(2,listOf("locked.jpg","changed.jpg"))),row("Project files",FolderSkipped(links=2)))
        val preparations=listOf(FolderPreparing(row("Vacation 2026",FolderSkipped()).apply {state="preparing"},1204),FolderPreparing(row("Project files",FolderSkipped()).apply {state="preparing"},312,400000000,1800000000))
        compose.setContent {CompositionLocalProvider(LocalUiActions provides UiActions {message,_->fail("Unexpected fixture action: $message")}) {PneuTheme(dark=dark) {
            Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {
                if(scene in listOf("button","unreachable"))DeviceScreen(p,DeviceState(peer,if(scene=="unreachable")link.copy(status="Unreachable") else link,emptySet(),true),{},{},{},{}) {_,_->}
                else Scaffold(containerColor=LocalPneuColors.current.bg,topBar={HomeTopBar(false,history={}) {}}) {padding->
                    HomeScreen(p,link,if(scene=="skipped")skipped else emptyList(),Health(true,true,true,true),Modifier.padding(padding),{},{},{},deviceLinks=mapOf(peer.deviceId to link),folderPreparations=if(scene in listOf("preparing","skipped"))preparations else emptyList())
                }
            }
        }}}
        for(name in listOf("button","unreachable","preparing","skipped"))for(theme in listOf(false,true)) {
            compose.runOnIdle {scene=name;dark=theme}
            compose.activityRule.scenario.onActivity {activity->
                val transparent=android.graphics.Color.TRANSPARENT
                val style=if(theme)SystemBarStyle.dark(transparent) else SystemBarStyle.light(transparent,transparent)
                activity.enableEdgeToEdge(style,style)
            }
            compose.waitForIdle()
            if(name=="button") {compose.onNodeWithText("Send files").assertIsDisplayed();compose.onNodeWithText("Send folder").assertIsDisplayed()}
            if(name=="preparing")compose.onNodeWithText("Preparing · 1,204 items").assertIsDisplayed()
            if(name=="skipped")compose.onNodeWithText("Camera backup").assertIsDisplayed()
            if(InstrumentationRegistry.getArguments().getString("folderScreens")=="true") {
                val key="$name-${if(theme)"dark" else "light"}"
                File(i.targetContext.filesDir,"folder-scene").writeText(key)
                withTimeout(120000) {val next=File(i.targetContext.filesDir,"folder-next-$key");while(!next.exists())delay(50);next.delete()}
            }
        }
        File(i.targetContext.filesDir,"folder-scene").delete()
    }
}
