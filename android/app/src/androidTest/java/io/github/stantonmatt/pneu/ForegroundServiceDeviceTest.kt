package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Notification
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import androidx.activity.ComponentActivity
import androidx.core.content.ContextCompat
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.features.ShareAdmissionService
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.IOException
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.atomic.AtomicBoolean

/** Real AMS starts/promotion/shutdown. A timeout crash kills instrumentation,
 * so waiting beyond Android's deadline also witnesses process survival. */
@RunWith(AndroidJUnit4::class)
class ForegroundServiceDeviceTest {
    @get:Rule val activity=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val commands=ConcurrentLinkedQueue<Pair<PneuService,Int>>()
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.POST_NOTIFICATIONS)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        PneuService.onForegroundCommand={service,id->commands.add(service to id)}
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeReadStart={}
        DeviceFixture.stopPneuService()
        ShareAdmissionService.stop(c)
        i.waitForIdleSync()
        PneuService.onForegroundCommand={_,_->};ShareAdmissionService.onForegroundCommand={_,_->}
        DeviceFixture.reset()
    }
    private suspend fun delivered(count:Int) = withTimeout(5000) {while(commands.size<count)delay(20)}
    private suspend fun pending() {
        val raw="pneu://pair?v=1&name=Startup&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null))
        withTimeout(5000) {app.prefs.first {it.pending!=null}}
    }
    @Test(timeout=30000) fun rapidStartStopAndUnpairedRestartsAlwaysPromoteBeforeStopping()=runBlocking<Unit> {
        repeat(5) {
            val before=commands.size
            PneuService.start(c)
            // Same order as a fast Cancel/Unpair or the restoration fixture,
            // without waiting for onCreate or onStartCommand to be delivered.
            PneuService.stop(c)
            delivered(before+2)
            DeviceFixture.stopPneuService()
        }
        val next=commands.size+1
        PneuService.start(c);delivered(next)
        DeviceFixture.stopPneuService()
        delay(12000)
        assertTrue("Foreground callbacks must occur for start and stop commands",commands.size>=10)
    }
    @Test(timeout=30000) fun stalledPreferencesRepeatedCommandsAndStickyRestoreStillPromoteSynchronously()=runBlocking<Unit> {
        pending()
        val readEntered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        app.preferences.beforeReadStart={readEntered.complete(Unit);release.await()}
        try {
            PneuService.start(c);delivered(1)
            withTimeout(5000) {readEntered.await()}
            withTimeout(5000) {
                while(c.getSystemService(NotificationManager::class.java).activeNotifications.none {
                    it.id==1 && it.notification.flags and Notification.FLAG_FOREGROUND_SERVICE!=0
                })delay(20)
            }
            PneuService.start(c);delivered(2)
            val (service,id)=commands.last()
            i.runOnMainSync {
                assertEquals(Service.START_STICKY,service.onStartCommand(null,Service.START_FLAG_RETRY,id))
            }
            delivered(3)
            c.startService(Intent(c,PneuService::class.java).setAction(PneuService.ACTION_PAUSE));delivered(4)
            // Stay stalled beyond the framework's foreground-start deadline.
            delay(12000)
            assertFalse(release.isCompleted)
        } finally {release.complete(Unit);app.preferences.beforeReadStart={}}
        DeviceFixture.stopPneuService()
    }
    @Test(timeout=30000) fun failedInitializationCommandsPromoteAndCanRetryWithoutRecreation()=runBlocking<Unit> {
        pending()
        val failing=AtomicBoolean(true)
        app.preferences.beforeReadStart={if(failing.get())throw IOException("Synthetic startup failure")}
        try {
            PneuService.start(c);delivered(1)
            withTimeout(5000) {app.connection.state.first {it.status=="Storage unavailable"}}
            val original=commands.first().first
            PneuService.start(c);delivered(2)
            assertSame(original,commands.last().first)
            delay(12000)
            failing.set(false);app.preferences.beforeReadStart={}
            PneuService.start(c);delivered(3)
            withTimeout(5000) {app.connection.state.first {it.status!="Storage unavailable"}}
        } finally {failing.set(false);app.preferences.beforeReadStart={}}
        DeviceFixture.stopPneuService()
    }
    @Test(timeout=30000) fun emptyAndRestoredShareGrantStartsPromoteEvenWhenStoppingImmediately()=runBlocking<Unit> {
        val delivered=ConcurrentLinkedQueue<Pair<ShareAdmissionService,Int>>()
        val restored=AtomicBoolean(true);val restoreResults=ConcurrentLinkedQueue<Int>()
        ShareAdmissionService.onForegroundCommand={service,id->
            delivered.add(service to id)
            // Simulate Android's redelivered null/missing-owner command on a
            // live service instance, before its immediate idle shutdown.
            if(restored.compareAndSet(true,false))restoreResults.add(service.onStartCommand(null,Service.START_FLAG_REDELIVERY,id))
        }
        repeat(5) {
            val expected=delivered.size+2
            ContextCompat.startForegroundService(c,Intent(c,ShareAdmissionService::class.java))
            ShareAdmissionService.stop(c)
            withTimeout(5000) {while(delivered.size<expected)delay(20)}
        }
        delay(12000)
        assertTrue(delivered.size>=10)
        assertEquals(listOf(Service.START_NOT_STICKY),restoreResults.toList())
    }
}
