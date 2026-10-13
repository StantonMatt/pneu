package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Notification
import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.media.AudioAttributes
import android.net.Uri
import android.provider.Settings
import androidx.test.uiautomator.*
import androidx.activity.ComponentActivity
import androidx.core.content.ContextCompat
import androidx.core.app.NotificationCompat
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.features.ShareAdmissionService
import io.github.stantonmatt.pneu.features.ManualEntryViewModel
import io.github.stantonmatt.pneu.features.ShareRequest
import io.github.stantonmatt.pneu.data.DeliveryBinding
import io.github.stantonmatt.pneu.connection.LinkState
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.IOException
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.atomic.AtomicBoolean
import java.util.UUID

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
        DeviceFixture.stopShareAdmissionService()
        app.connection.stop();withTimeout(10000) {app.connection.awaitStopped()}
        PneuService.onForegroundCommand={_,_->};ShareAdmissionService.onForegroundCommand={_,_->}
        StatusNotificationChannel.resetForTest();StatusNotificationChannel.create(c)
        DeviceFixture.reset()
    }
    private suspend fun delivered(count:Int) = withTimeout(5000) {while(commands.size<count)delay(20)}
    private suspend fun pending() {
        val raw="pneu://pair?v=1&name=Startup&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null))
        withTimeout(5000) {app.prefs.first {it.pending!=null}}
    }
    private fun assertBadgeFreeChannel(nm:NotificationManager) {
        val status=nm.getNotificationChannel(StatusNotificationChannel.id(c))
        assertNotNull(status);assertFalse(status.canShowBadge())
        assertEquals(NotificationManager.IMPORTANCE_LOW,status.importance)
    }
    private fun retainedChoices():List<Pair<String,(NotificationChannel)->Unit>> = listOf(
        "blocked" to {channel:NotificationChannel->channel.importance=NotificationManager.IMPORTANCE_NONE},
        "minimized" to {channel:NotificationChannel->channel.importance=NotificationManager.IMPORTANCE_MIN},
        "private" to {channel:NotificationChannel->channel.lockscreenVisibility=Notification.VISIBILITY_PRIVATE},
        "secret" to {channel:NotificationChannel->channel.lockscreenVisibility=Notification.VISIBILITY_SECRET},
        "DND bypass" to {channel:NotificationChannel->channel.setBypassDnd(true)},
        "custom sound" to {channel:NotificationChannel->channel.setSound(Uri.parse("content://io.github.stantonmatt.pneu.test.sources/denied"),channel.audioAttributes)}
    )
    private fun allChanges()=retainedChoices()+listOf(
        "higher importance" to {channel:NotificationChannel->channel.importance=NotificationManager.IMPORTANCE_HIGH},
        "public visibility" to {channel:NotificationChannel->channel.lockscreenVisibility=Notification.VISIBILITY_PUBLIC},
        "silent sound" to {channel:NotificationChannel->channel.setSound(null,channel.audioAttributes)},
        "audio attributes" to {channel:NotificationChannel->channel.setSound(channel.sound,AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_ALARM).build())},
        "vibration" to {channel:NotificationChannel->channel.enableVibration(true)},
        "vibration pattern" to {channel:NotificationChannel->channel.vibrationPattern=longArrayOf(0,30,60)},
        "lights" to {channel:NotificationChannel->channel.enableLights(true)},
        "light color" to {channel:NotificationChannel->channel.lightColor=android.graphics.Color.RED},
        "badge" to {channel:NotificationChannel->channel.setShowBadge(false)},
        "bubbles" to {channel:NotificationChannel->channel.setAllowBubbles(true)},
        "blockable" to {channel:NotificationChannel->channel.setBlockable(true)},
        "group" to {channel:NotificationChannel->channel.group="group"},
        "conversation" to {channel:NotificationChannel->channel.setConversationId("parent","conversation")},
        "name" to {channel:NotificationChannel->channel.name="User status"},
        "description" to {channel:NotificationChannel->channel.description="User settings"}
    )
    @Test(timeout=15000) fun untouchedLegacyChannelMigratesWithBadgeOff() {
        val nm=c.getSystemService(NotificationManager::class.java)
        val legacyId="legacy-${UUID.randomUUID()}";val id="replacement-${UUID.randomUUID()}"
        try {
            nm.createNotificationChannel(NotificationChannel(legacyId,"Status",NotificationManager.IMPORTANCE_LOW))
            assertTrue(StatusNotificationChannel.untouched(nm.getNotificationChannel(legacyId),"Status"))
            assertEquals(id,StatusNotificationChannel.migrate(nm,legacyId,id,"Status"))
            assertNull(nm.getNotificationChannel(legacyId))
            val registered=nm.getNotificationChannel(id)
            assertNotNull(registered);assertFalse(registered.canShowBadge())
            assertEquals(NotificationManager.IMPORTANCE_LOW,registered.importance)
            assertEquals(Settings.System.DEFAULT_NOTIFICATION_URI,registered.sound)
        } finally {nm.deleteNotificationChannel(legacyId);nm.deleteNotificationChannel(id)}
    }
    @Test(timeout=15000) fun freshInstallCreatesBadgeFreeStatusChannel() {
        val nm=c.getSystemService(NotificationManager::class.java)
        val legacyId="legacy-${UUID.randomUUID()}";val id="replacement-${UUID.randomUUID()}"
        try {
            assertEquals(id,StatusNotificationChannel.migrate(nm,legacyId,id,"Status"))
            val registered=nm.getNotificationChannel(id)
            assertNotNull(registered);assertFalse(registered.canShowBadge())
            assertEquals(NotificationManager.IMPORTANCE_LOW,registered.importance)
            assertNull(nm.getNotificationChannel(legacyId))
        } finally {nm.deleteNotificationChannel(id)}
    }
    @Test(timeout=15000) fun existingV2WinsWithoutReadingOrChangingLegacySettings() {
        val nm=c.getSystemService(NotificationManager::class.java)
        val legacyId="legacy-${UUID.randomUUID()}";val id="replacement-${UUID.randomUUID()}"
        try {
            nm.createNotificationChannel(NotificationChannel(legacyId,"Status",NotificationManager.IMPORTANCE_NONE))
            nm.createNotificationChannel(NotificationChannel(id,"User status",NotificationManager.IMPORTANCE_HIGH).apply {setShowBadge(false)})
            val legacy=nm.getNotificationChannel(legacyId);val existing=nm.getNotificationChannel(id)
            assertEquals(id,StatusNotificationChannel.migrate(nm,legacyId,id,"Status",loadLegacy={fail("Existing v2 must win without reading legacy");null}))
            assertEquals(existing,nm.getNotificationChannel(id));assertEquals(legacy,nm.getNotificationChannel(legacyId))
        } finally {nm.deleteNotificationChannel(legacyId);nm.deleteNotificationChannel(id)}
    }
    @Test(timeout=15000) fun everyChangedLegacySettingIsKeptWithoutCopying() {
        val nm=c.getSystemService(NotificationManager::class.java)
        for((label,change) in allChanges()) {
            val legacyId="legacy-${UUID.randomUUID()}";val id="replacement-${UUID.randomUUID()}"
            try {
                nm.createNotificationChannel(NotificationChannel(legacyId,"Status",NotificationManager.IMPORTANCE_LOW))
                val registered=nm.getNotificationChannel(legacyId)
                // Model a native user setting from NotificationManager, including
                // values apps cannot write through createNotificationChannel.
                val legacy=nm.getNotificationChannel(legacyId).apply(change)
                assertFalse(label,StatusNotificationChannel.untouched(legacy,"Status"))
                assertEquals(label,legacyId,StatusNotificationChannel.migrate(nm,legacyId,id,"Status",loadLegacy={legacy},register={fail("Changed settings must never be copied")}))
                assertEquals(label,registered,nm.getNotificationChannel(legacyId))
                assertNull(label,nm.getNotificationChannel(id))
            } finally {nm.deleteNotificationChannel(legacyId);nm.deleteNotificationChannel(id)}
        }
    }
    @Test(timeout=15000) fun failedLegacySettingsReadKeepsItsChannelWithoutCrashing() {
        val nm=c.getSystemService(NotificationManager::class.java)
        val legacyId="legacy-${UUID.randomUUID()}";val id="replacement-${UUID.randomUUID()}"
        try {
            nm.createNotificationChannel(NotificationChannel(legacyId,"Status",NotificationManager.IMPORTANCE_NONE))
            val registered=nm.getNotificationChannel(legacyId)
            assertEquals(legacyId,StatusNotificationChannel.migrate(nm,legacyId,id,"Status",loadLegacy={throw SecurityException("Fixture settings failure")}))
            assertEquals(registered,nm.getNotificationChannel(legacyId));assertNull(nm.getNotificationChannel(id))
        } finally {nm.deleteNotificationChannel(legacyId);nm.deleteNotificationChannel(id)}
    }
    @Test(timeout=15000) fun creationFailureKeepsAnExistingChannelAndFreshInstallCanRecover() {
        val nm=c.getSystemService(NotificationManager::class.java)
        val legacyId="legacy-${UUID.randomUUID()}";val id="replacement-${UUID.randomUUID()}"
        try {
            nm.createNotificationChannel(NotificationChannel(legacyId,"Status",NotificationManager.IMPORTANCE_LOW))
            val registered=nm.getNotificationChannel(legacyId)
            assertEquals(legacyId,StatusNotificationChannel.migrate(nm,legacyId,id,"Status",register={throw SecurityException("Fixture creation failure")}))
            assertEquals(registered,nm.getNotificationChannel(legacyId));assertNull(nm.getNotificationChannel(id))
            nm.deleteNotificationChannel(legacyId)
            assertEquals(id,StatusNotificationChannel.migrate(nm,legacyId,id,"Status",register={throw SecurityException("Fixture creation failure")}))
            assertNotNull(nm.getNotificationChannel(id));assertFalse(nm.getNotificationChannel(id).canShowBadge())
        } finally {nm.deleteNotificationChannel(legacyId);nm.deleteNotificationChannel(id)}
    }
    private suspend fun posted(nm:NotificationManager,notificationId:Int,channelId:String):Notification=withTimeout(5000) {
        while(nm.activeNotifications.none {it.id==notificationId && it.notification.channelId==channelId})delay(20)
        nm.activeNotifications.first {it.id==notificationId && it.notification.channelId==channelId}.notification
    }
    @Test(timeout=60000) fun changedLegacyChannelIsResolvedOnceAndUsedByEveryServicePoster()=runBlocking<Unit> {
        val nm=c.getSystemService(NotificationManager::class.java)
        for((index,choice) in retainedChoices().withIndex()) {
            val (label,change)=choice
            val legacyId="badge-service-fixture-legacy-$index";val candidateId="badge-service-fixture-v2-$index"
            nm.createNotificationChannel(NotificationChannel(legacyId,"Status",NotificationManager.IMPORTANCE_LOW))
            val registered=nm.getNotificationChannel(legacyId)
            val legacy=nm.getNotificationChannel(legacyId).apply(change)
            var reads=0
            assertEquals(label,legacyId,StatusNotificationChannel.selectForTest(c,legacyId,candidateId,loadLegacy={reads++;legacy}))
            val binding=CompletableDeferred<DeliveryBinding?>()
            val operation=ManualEntryViewModel(c.applicationContext as Application)
            try {
                pending();val next=commands.size+1;PneuService.start(c);delivered(next)
                assertEquals(label,legacyId,posted(nm,1,legacyId).channelId)
                val service=commands.last().first
                val starting=PneuService::class.java.getDeclaredMethod("startingNotification").apply {isAccessible=true}.invoke(service) as Notification
                nm.notify(602,starting)
                assertEquals(label,legacyId,posted(nm,602,legacyId).channelId)
                app.connection.state.value=LinkState(status="Connected",transferMediaId="fixture",transferKind="file",transferFileName="confidential.pdf",total=100,sent=20)
                val progress=PneuService::class.java.getDeclaredMethod("notification").apply {isAccessible=true}.invoke(service) as Notification
                nm.notify(603,progress)
                assertEquals(label,legacyId,posted(nm,603,legacyId).channelId)
                assertNull(progress.publicVersion)
                i.runOnMainSync {ShareAdmissionService.start(activity.activity,operation,ShareRequest(binding,emptyList(),listOf("fixture")))}
                assertEquals(label,legacyId,posted(nm,4,legacyId).channelId)
                assertEquals(legacyId,StatusNotificationChannel.id(c,loadLegacy={fail("Channel must resolve only once");null}))
                assertEquals(1,reads)
                val retained=nm.getNotificationChannel(legacyId)
                assertNotNull(retained);assertEquals(registered.importance,retained.importance)
                assertEquals(registered.lockscreenVisibility,retained.lockscreenVisibility)
                assertEquals(registered.canShowBadge(),retained.canShowBadge())
                assertNull(nm.getNotificationChannel(candidateId))
            } finally {
                binding.complete(null)
                if(operation.started)withTimeout(5000) {operation.result.first {it!=null}}
                DeviceFixture.stopPneuService();DeviceFixture.stopShareAdmissionService()
                app.connection.stop();withTimeout(10000) {app.connection.awaitStopped()}
                nm.cancel(602);nm.cancel(603)
                // Keep these stable fixture channels registered: AMS validates
                // foreground posts asynchronously even after app-thread idle.
                StatusNotificationChannel.resetForTest();StatusNotificationChannel.create(c)
            }
        }
    }
    @Test(timeout=60000) fun userBlockedAndMinimizedImportanceSurvivesBothRealForegroundPromotions()=runBlocking<Unit> {
        val nm=c.getSystemService(NotificationManager::class.java)
        val device=UiDevice.getInstance(i)
        for((index,importance) in listOf(NotificationManager.IMPORTANCE_NONE,NotificationManager.IMPORTANCE_MIN).withIndex()) {
            val legacyId="badge-importance-fixture-legacy-$index";val candidateId="badge-importance-fixture-v2-$index"
            val settings=Intent(Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE,c.packageName)
                .putExtra(Settings.EXTRA_CHANNEL_ID,legacyId).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
            nm.createNotificationChannel(NotificationChannel(legacyId,"Status",NotificationManager.IMPORTANCE_LOW))
            val binding=CompletableDeferred<DeliveryBinding?>()
            val operation=ManualEntryViewModel(c.applicationContext as Application)
            try {
                if(importance==NotificationManager.IMPORTANCE_NONE) {
                    DeviceFixture.setChannelEnabled(legacyId,false)
                } else {
                    c.startActivity(settings)
                    val minimize=device.findObject(UiSelector().packageName("com.android.settings").text("Minimize"))
                    assertTrue("Minimize must be available",minimize.waitForExists(3000));minimize.click()
                }
                withTimeout(3000) {while(nm.getNotificationChannel(legacyId).importance!=importance)delay(20)}
                assertTrue("Settings must lock the user's importance",nm.getNotificationChannel(legacyId).hasUserSetImportance())
                assertEquals(legacyId,StatusNotificationChannel.selectForTest(c,legacyId,candidateId,loadLegacy={nm.getNotificationChannel(legacyId)}))
                device.pressBack();pending()
                val next=commands.size+1;PneuService.start(c);delivered(next)
                val service=commands.last().first
                val notification=PneuService::class.java.getDeclaredMethod("startingNotification").apply {isAccessible=true}.invoke(service) as Notification
                assertEquals(legacyId,notification.channelId)
                val shared=CompletableDeferred<Unit>()
                ShareAdmissionService.onForegroundCommand={_,_->shared.complete(Unit)}
                i.runOnMainSync {ShareAdmissionService.start(activity.activity,operation,ShareRequest(binding,emptyList(),listOf("fixture")))}
                withTimeout(5000) {shared.await()};delay(300)
                // Pneu keeps the user's channel and never creates the replacement.
                // What Android then does to a blocked channel on promotion is
                // platform behaviour, unchanged by the migration.
                assertNull(nm.getNotificationChannel(candidateId))
            } finally {
                binding.complete(null)
                if(operation.started)withTimeout(5000) {operation.result.first {it!=null}}
                DeviceFixture.stopPneuService();DeviceFixture.stopShareAdmissionService()
                app.connection.stop();withTimeout(10000) {app.connection.awaitStopped()}
                ShareAdmissionService.onForegroundCommand={_,_->}
                if(nm.getNotificationChannel(legacyId).importance==NotificationManager.IMPORTANCE_NONE) {
                    DeviceFixture.setChannelEnabled(legacyId,true)
                } else if(nm.getNotificationChannel(legacyId).importance==NotificationManager.IMPORTANCE_MIN) {
                    c.startActivity(settings)
                    val minimize=device.findObject(UiSelector().packageName("com.android.settings").text("Minimize"))
                    assertTrue(minimize.waitForExists(3000));minimize.click()
                }
                withTimeout(3000) {while(nm.getNotificationChannel(legacyId).importance!=NotificationManager.IMPORTANCE_LOW)delay(20)}
                val restored=nm.getNotificationChannel(legacyId)
                assertEquals(NotificationManager.IMPORTANCE_LOW,restored.importance)
                // This emulator clears the importance lock on some restore
                // paths. Verify eligibility against the state Android actually
                // returns, rather than asserting a lock it did not retain.
                i.sendStatus(0,android.os.Bundle().apply {putString("stream","Restored $legacyId: userImportance=${restored.hasUserSetImportance()}, userSound=${restored.hasUserSetSound()}\n")})
                assertEquals("Restored defaults are untouched only when Android cleared both user-set flags",
                    !restored.hasUserSetImportance() && !restored.hasUserSetSound(),StatusNotificationChannel.untouched(restored,"Status"))
                assertNull(nm.getNotificationChannel(candidateId))
                device.pressHome()
                StatusNotificationChannel.resetForTest();StatusNotificationChannel.create(c)
            }
        }
    }
    @Test(timeout=15000) fun connectionStatusUsesResolvedBadgeFreeChannelAndPreservesAlerts()=runBlocking<Unit> {
        pending()
        val nm=c.getSystemService(NotificationManager::class.java)
        PneuService.start(c);delivered(1)
        assertBadgeFreeChannel(nm)
        val notification=withTimeout(5000) {
            while(nm.activeNotifications.none {it.id==1})delay(20)
            nm.activeNotifications.first {it.id==1}.notification
        }
        assertEquals(StatusNotificationChannel.ID,notification.channelId)
        assertTrue(notification.flags and Notification.FLAG_ONGOING_EVENT!=0)
        assertTrue("Recovery alerts must retain badges",nm.getNotificationChannel("alerts").canShowBadge())
        StatusNotificationChannel.create(c)
        assertBadgeFreeChannel(nm)
    }
    @Test(timeout=15000) fun shareAdmissionUsesResolvedBadgeFreeChannel()=runBlocking<Unit> {
        val nm=c.getSystemService(NotificationManager::class.java)
        val promoted=CompletableDeferred<Unit>()
        ShareAdmissionService.onForegroundCommand={_,_->promoted.complete(Unit)}
        ContextCompat.startForegroundService(c,Intent(c,ShareAdmissionService::class.java))
        // An empty admission stops immediately, so its notification may already
        // be removed by the time instrumentation reads activeNotifications.
        withTimeout(5000) {promoted.await()}
        assertBadgeFreeChannel(nm)
    }
    @Test(timeout=20000) fun missingCachedChannelAndStaleNotificationRecoverBeforeForegroundPromotion()=runBlocking<Unit> {
        pending()
        val nm=c.getSystemService(NotificationManager::class.java)
        val missing="unregistered-${UUID.randomUUID()}"
        assertNull(nm.getNotificationChannel(missing))
        StatusNotificationChannel.resetForTest(missing)
        PneuService.start(c);delivered(1)
        val selected=StatusNotificationChannel.id(c)
        assertNotEquals(missing,selected);assertNotNull(nm.getNotificationChannel(selected))
        assertEquals(selected,posted(nm,1,selected).channelId)
        val (service,startId)=commands.last()
        val stale=NotificationCompat.Builder(c,missing).setSmallIcon(R.drawable.ic_stat_pneu).setContentTitle("Stale fixture").build()
        PneuService::class.java.getDeclaredField("lastNotification").apply {isAccessible=true}.set(service,stale)
        val next=commands.size+1
        i.runOnMainSync {service.onStartCommand(null,Service.START_FLAG_RETRY,startId)}
        delivered(next)
        assertEquals(selected,posted(nm,1,selected).channelId)
        val promoted=CompletableDeferred<Unit>()
        ShareAdmissionService.onForegroundCommand={_,_->promoted.complete(Unit)}
        StatusNotificationChannel.resetForTest(missing)
        ContextCompat.startForegroundService(c,Intent(c,ShareAdmissionService::class.java))
        withTimeout(5000) {promoted.await()}
        assertNotNull(nm.getNotificationChannel(StatusNotificationChannel.id(c)))
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
