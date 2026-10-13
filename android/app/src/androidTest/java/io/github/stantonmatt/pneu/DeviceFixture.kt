package io.github.stantonmatt.pneu

import android.content.Intent
import android.app.ActivityManager
import android.app.NotificationManager
import android.net.Uri
import android.content.ContentUris
import android.provider.MediaStore
import android.provider.Settings
import androidx.test.uiautomator.*
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.test.platform.app.InstrumentationRegistry
import com.google.gson.JsonParser
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.data.MediaCatalog
import io.github.stantonmatt.pneu.data.Prefs
import io.github.stantonmatt.pneu.features.ShareAdmissionService
import kotlinx.coroutines.delay
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.Assert.*
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/** Reset before every test, including policy keys that forgetBinding deliberately preserves.
 * Prepared peers keep their exported leaf; host-witnessed cold phases validate rather
 * than erase the state whose creation is the subject of the test. Permissions are
 * managed by each fixture/host (the permanent-denial class must retain its denials). */
internal object DeviceFixture {
    /** Start a new Settings task for each channel: reusing a channel-details
     * Activity can leave its fragment displaying the previous channel. Target
     * the main Show notifications control, never an unrelated checked switch. */
    suspend fun setChannelEnabled(channelId:String,enabled:Boolean) {
        val i=InstrumentationRegistry.getInstrumentation();val c=i.targetContext
        val nm=c.getSystemService(NotificationManager::class.java);val device=UiDevice.getInstance(i)
        if((nm.getNotificationChannel(channelId).importance!=NotificationManager.IMPORTANCE_NONE)==enabled)return
        c.startActivity(Intent(Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS)
            .putExtra(Settings.EXTRA_APP_PACKAGE,c.packageName).putExtra(Settings.EXTRA_CHANNEL_ID,channelId)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        val control=device.findObject(UiSelector().packageName("com.android.settings").text("Show notifications"))
        assertTrue("Show notifications control for $channelId must appear",control.waitForExists(5000))
        control.click()
        val changed=withTimeoutOrNull(5000) {
            while((nm.getNotificationChannel(channelId).importance!=NotificationManager.IMPORTANCE_NONE)!=enabled)delay(50)
            true
        } ?: false
        if(!changed) {
            val evidence=File(c.filesDir,"badge-settings-failures").apply {mkdirs()}
            device.dumpWindowHierarchy(File(evidence,"$channelId-$enabled.xml"))
            device.takeScreenshot(File(evidence,"$channelId-$enabled.png"))
        }
        assertTrue("Settings must set $channelId enabled=$enabled; actual importance=${nm.getNotificationChannel(channelId).importance}",changed)
    }
    /** Deliver stop behind a queued foreground start; raw stopService can
     * remove that start before the service has acknowledged its deadline. */
    suspend fun stopPneuService() {
        val i=InstrumentationRegistry.getInstrumentation();val c=i.targetContext
        PneuService.stop(c)
        i.waitForIdleSync()
        withTimeout(10000) {
            while(c.getSystemService(ActivityManager::class.java).getRunningServices(Int.MAX_VALUE)
                .any {it.service.className==PneuService::class.java.name})delay(20)
        }
        i.waitForIdleSync()
    }
    suspend fun stopShareAdmissionService() {
        val i=InstrumentationRegistry.getInstrumentation();val c=i.targetContext
        ShareAdmissionService.stop(c)
        i.waitForIdleSync()
        withTimeout(10000) {
            while(c.getSystemService(ActivityManager::class.java).getRunningServices(Int.MAX_VALUE)
                .any {it.service.className==ShareAdmissionService::class.java.name})delay(20)
        }
        i.waitForIdleSync()
    }
    suspend fun reset(preserveIdentity:Boolean=InstrumentationRegistry.getArguments().getString("identityPrepared")=="true")=withContext(Dispatchers.IO) {
        val c=InstrumentationRegistry.getInstrumentation().targetContext
        val app=c.graph
        app.preferences.beforeMutation={}
        app.preferences.beforeRead={}
        app.preferences.beforeReadStart={}
        app.preferences.readBoundary={MediaCatalog.latest(c,it)}
        app.preferences.readSnapshot={MediaCatalog.snapshot(c)}
        app.preferences.readCatalogVersion={MediaCatalog.version(c)}
        app.connection.beforeUnpairRemoval={}
        app.connection.onUnauthorized=null
        app.connection.openPayload={it.inputStream()}
        app.connection.beforeFirstFrame={}
        app.staging.beforeThumbnailWrite={}
        app.staging.beforePayloadWrite={}
        app.staging.beforeAutomaticPayloadWrite={}
        app.staging.afterAutomaticCaptureDrop={}
        app.onMediaObserverReady={}
        withTimeout(10000) {app.initialized.await()}
        stopPneuService()
        stopShareAdmissionService()
        // Flush service destruction before joining its cancelled connection/senders.
        InstrumentationRegistry.getInstrumentation().waitForIdleSync()
        app.connection.stop()
        withTimeout(10000) {app.connection.awaitStopped()}
        // Failure-injection triggers are test-only. A failed test may miss its finally.
        val db=app.database.openHelper.writableDatabase
        val triggers=mutableListOf<String>()
        db.query("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'fail_%'").use {cursor->
            while(cursor.moveToNext())triggers.add(cursor.getString(0))
        }
        triggers.forEach {db.execSQL("DROP TRIGGER IF EXISTS \"${it.replace("\"","\"\"")}\"")}
        val before=app.preferences.read()
        if(preserveIdentity) {
            assertNull("Prepared peer must start unpaired",before.peer)
            assertNull("Prepared peer must have no pending invitation",before.pending)
            assertArrayEquals(File(c.filesDir,"client.der").readBytes(),TlsIdentity(before.tlsAlias).certificate.encoded)
        } else app.forgetBinding()
        app.staging.abortSources()
        app.clearDelivery()
        val identity=app.preferences.read()
        // Atomic reset avoids transport-order/media-permission side effects. Use the
        // same DataStore instance, never a second instance for the same backing file.
        app.preferences.store.edit {prefs->
            prefs.clear()
            prefs[stringPreferencesKey("deviceId")]=identity.deviceId
            prefs[stringPreferencesKey("tlsAlias")]=identity.tlsAlias
        }
        app.connection.bindingChanged()
        app.staging.cleanup()
        val expected=Prefs(deviceId=identity.deviceId,tlsAlias=identity.tlsAlias)
        assertEquals(expected,app.preferences.read())
        withTimeout(5000) {app.prefs.first {it==expected}}
        assertTrue(app.queue.all().isEmpty())
        assertEquals(0,app.queue.receiptCount())
        assertEquals(0L,app.queue.latestTextTime())
    }

    /** Baseline first, then hold this source's actual write across a saved policy
     * mutation. A separate drop latch proves cleanup and rescan, not mere activity. */
    suspend fun changePolicyDuringCapture(create:()->Uri,change:suspend ()->Unit):Uri=withContext(Dispatchers.IO) {
        val c=InstrumentationRegistry.getInstrumentation().targetContext
        val app=c.graph
        val ready=CountDownLatch(1);val entered=CountDownLatch(1);val release=CountDownLatch(1)
        val dropped=CountDownLatch(1);val resume=CountDownLatch(1);val first=AtomicBoolean(true)
        val source=java.util.concurrent.atomic.AtomicReference<Uri>()
        val copying=java.util.concurrent.atomic.AtomicReference<io.github.stantonmatt.pneu.data.Outgoing>()
        app.onMediaObserverReady={ready.countDown()}
        app.staging.beforeAutomaticPayloadWrite={row->
            if(synchronized(source) {row.uri==source.get()?.toString()} && first.compareAndSet(true,false)) {
                copying.set(row);entered.countDown();check(release.await(8,TimeUnit.SECONDS))
            }
        }
        app.staging.afterAutomaticCaptureDrop={row->if(row===copying.get()) {
            dropped.countDown();check(resume.await(8,TimeUnit.SECONDS))
        }}
        try {
            app.requestMediaScan()
            assertTrue("Observer did not finish its startup baselines",ready.await(8,TimeUnit.SECONDS))
            // MediaStore may notify during creation. Hold the copy hook until the
            // source URI is known, rather than race publication against assignment.
            val uri=synchronized(source) {create().also {source.set(it)}}
            assertTrue("Observer did not begin copying this source",entered.await(5,TimeUnit.SECONDS))
            change();release.countDown()
            assertTrue("Stale capture did not drop and request rescan",dropped.await(5,TimeUnit.SECONDS))
            val row=copying.get()!!
            assertNull(app.queue.find(row.mediaId));assertEquals(0,app.queue.seen(row.mediaId))
            assertFalse(File(row.path).exists())
            val id="external_primary:${MediaStore.getVersion(c,"external_primary")}:images:${ContentUris.parseId(uri)}"
            assertEquals(id,row.mediaId)
            resume.countDown()
            uri
        }finally {
            release.countDown();resume.countDown()
            app.onMediaObserverReady={};app.staging.beforeAutomaticPayloadWrite={};app.staging.afterAutomaticCaptureDrop={}
        }
    }

    suspend fun resumeColdShare() {
        val app=InstrumentationRegistry.getInstrumentation().targetContext.graph
        val record=JsonParser.parseString(File(app.context.filesDir,"cold-share.json").readText()).asJsonObject
        val p=app.awaitPrefs()
        assertEquals(record["phone"].asString,p.deviceId)
        assertEquals(record["pin"].asString,p.peer?.pin)
        assertEquals(record["pc"].asString,p.peer?.deviceId)
        assertEquals(if(record["mode"].asString=="multiple")2 else 1,app.queue.pending().size)
    }

    suspend fun resumeFreshPairing() {
        // Never generate/rotate an identity before verifying the host's cold launch.
        val app=InstrumentationRegistry.getInstrumentation().targetContext.graph
        val p=app.awaitPrefs()
        assertTrue(p.loaded);assertTrue(p.deviceId.isNotEmpty());assertTrue(p.tlsAlias.isNotEmpty())
        assertNull(p.peer);assertNull(p.pending);assertTrue(app.queue.all().isEmpty())
    }
}
