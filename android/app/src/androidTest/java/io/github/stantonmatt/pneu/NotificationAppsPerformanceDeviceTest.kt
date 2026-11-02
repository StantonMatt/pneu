package io.github.stantonmatt.pneu

import android.graphics.Color
import android.os.SystemClock
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.material3.Surface
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithContent
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.Prefs
import io.github.stantonmatt.pneu.features.NotificationConsent
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Use the normal Android frame clock: a Compose test rule batches gesture frames. */
@RunWith(AndroidJUnit4::class)
class NotificationAppsPerformanceDeviceTest {
    @Test fun largeAppListPerformanceEvidence() {
        val instrumentation=InstrumentationRegistry.getInstrumentation()
        val context=instrumentation.targetContext
        val directory=File(context.filesDir,"notification-performance").apply {mkdirs()}
        val apps=(0 until 300).map {index->NotificationPickerApp("0/org.example.app$index","Fixture app %03d".format(index),context.getDrawable(R.drawable.ic_notifications))}
        var show by mutableStateOf(false)
        val emptyDrawn=CountDownLatch(1);val listDrawn=CountDownLatch(1)
        ActivityScenario.launch(ComponentActivity::class.java).use {scenario->
            scenario.onActivity {activity->
                activity.enableEdgeToEdge(SystemBarStyle.dark(Color.TRANSPARENT),SystemBarStyle.dark(Color.TRANSPARENT))
                activity.setContent {
                    PneuTheme(dark=true) {Surface(Modifier.fillMaxSize().drawWithContent {drawContent();emptyDrawn.countDown()},color=LocalPneuColors.current.bg) {
                        if(show)Box(Modifier.fillMaxSize().drawWithContent {drawContent();listDrawn.countDown()}) {
                            NotificationsContent(Prefs(notificationConsent=NotificationConsent(enabled=true)),true,false,apps,{},{},{},{_,_,_->},{})
                        }
                    }}
                }
            }
            assertTrue("Initial frame",emptyDrawn.await(15,TimeUnit.SECONDS))
            val start=SystemClock.elapsedRealtime()
            scenario.onActivity {show=true}
            assertTrue("First usable list frame",listDrawn.await(15,TimeUnit.SECONDS))
            val openMs=SystemClock.elapsedRealtime()-start
            fun shell(command:String):String=instrumentation.uiAutomation.executeShellCommand(command).use {
                java.io.FileInputStream(it.fileDescriptor).bufferedReader().readText()
            }
            shell("dumpsys gfxinfo ${context.packageName} reset")
            val device=UiDevice.getInstance(instrumentation)
            repeat(12) {
                assertTrue(device.swipe(device.displayWidth/2,device.displayHeight*85/100,device.displayWidth/2,device.displayHeight*35/100,70))
            }
            device.waitForIdle(3000)
            File(directory,"metrics.txt").writeText("open_ms=$openMs\n"+shell("dumpsys gfxinfo ${context.packageName}"))
            if(InstrumentationRegistry.getArguments().getString("performanceGate")=="true") {
                val gate=File(directory,"continue");gate.delete()
                File(directory,"ready").writeText("mid-scroll-dark")
                runBlocking {withTimeout(120000) {while(!gate.exists())delay(50)}}
                gate.delete()
            }
        }
    }
}
