package io.github.stantonmatt.pneu

import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.material3.Surface
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.NotificationConsent
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class NotificationScreensDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val context=InstrumentationRegistry.getInstrumentation().targetContext
    @Test fun lightAndDarkPresentationEvidence() {
        var scene by mutableStateOf("off-light")
        val computers=listOf(Peer("MJStanton-Desktop","a".repeat(64),"a".repeat(32),emptyList()),Peer("MacBook Air","b".repeat(64),"b".repeat(32),emptyList()))
        val prefs=Prefs(peers=computers,notificationConsent=NotificationConsent(true,true,false,computers.map(NotificationConsent::computer).toSet()))
        val apps=listOf("Amazon Shopping","Calendar","Chrome","Gmail","Instagram","Messages","WhatsApp").mapIndexed {index,name->
            NotificationPickerApp("0/$index",name,context.getDrawable(R.drawable.ic_notifications))
        }
        compose.setContent {
            PneuTheme(dark=scene.endsWith("dark")) {Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {
                NotificationsContent(if(scene.startsWith("apps"))prefs else prefs.copy(notificationConsent=NotificationConsent()),scene.startsWith("apps"),scene.startsWith("blocked"),apps,{},{},{},{_,_,_->},{})
                if(scene.startsWith("onboarding"))NotificationAccessDialog({},{})
            }}
        }
        val gated=InstrumentationRegistry.getArguments().getString("captureGates")=="true"
        val directory=File(context.filesDir,"notification-evidence").apply {mkdirs()}
        for(theme in listOf("light","dark"))for(screen in listOf("off","onboarding","apps","blocked")) {
            val name="$screen-$theme"
            compose.runOnIdle {scene=name}
            compose.activityRule.scenario.onActivity {activity->
                val transparent=android.graphics.Color.TRANSPARENT
                val style=if(theme=="dark")SystemBarStyle.dark(transparent) else SystemBarStyle.light(transparent,transparent)
                activity.enableEdgeToEdge(style,style)
            }
            compose.waitForIdle()
            compose.onNodeWithText("Show on computers").assertExists()
            if(screen=="apps")compose.onNodeWithText("Apps").assertExists()
            if(screen=="onboarding")compose.onNodeWithText("Allow notification access").assertExists()
            if(screen=="blocked")compose.onNodeWithText("Android blocked notification access").assertExists()
            if(gated) {
                val gate=File(directory,"continue-$name");gate.delete()
                File(directory,"ready").writeText(name)
                runBlocking {withTimeout(120000) {while(!gate.exists())delay(50)}}
                gate.delete()
            }
        }
        File(directory,"ready").writeText("complete")
    }
}
