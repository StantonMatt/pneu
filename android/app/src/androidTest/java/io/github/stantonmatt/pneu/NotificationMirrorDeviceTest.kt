package io.github.stantonmatt.pneu

import android.Manifest
import android.app.*
import android.content.*
import android.os.Bundle
import android.os.Process
import androidx.datastore.preferences.core.*
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

/** Separate test APK/process supplies genuine Android notification posts, never Pneu's own notices. */
class MirrorNotificationPosterActivity:Activity() {
    override fun onCreate(state:Bundle?) {
        super.onCreate(state)
        val manager=getSystemService(NotificationManager::class.java)
        if(intent.getBooleanExtra("cancel",false)) {manager.cancel(42);finish();return}
        manager.createNotificationChannel(NotificationChannel("mirror-test","Mirror test",NotificationManager.IMPORTANCE_HIGH))
        manager.notify(42,Notification.Builder(this,"mirror-test").setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle("Ana").setContentText(intent.getStringExtra("text") ?: "¿Llegaste?").setVisibility(if(intent.getBooleanExtra("secret",false))Notification.VISIBILITY_SECRET else Notification.VISIBILITY_PRIVATE).build())
        finish()
    }
}

@RunWith(AndroidJUnit4::class)
class NotificationMirrorDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation()
    private val context=i.targetContext
    private val app=context.graph
    @Before fun reset()=runBlocking<Unit> {DeviceFixture.reset()}
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.reset()}
    @Test fun listenerPostIsEmittedThroughAuthenticatedMeshPeerAndOptOutClears()=runBlocking<Unit> {
        val component=ComponentName(context,PhoneNotificationListener::class.java).flattenToString()
        // The host grants access with cmd notification allow_listener before this focused run.
        assertTrue("Host must grant notification listener access",context.notificationAccess())
        val frames=Channel<NotificationEvent>(128)
        val peer=Peer("Notification test PC","a".repeat(64),"d".repeat(32),emptyList())
        app.preferences.store.edit {PeerRegistry.write(it,listOf(peer))}
        LocalPeerFixture(app,setOf("files","clipboard_text","clipboard_image","mesh",CAP_NOTIFICATIONS),handle={frame,_->
            if(frame.type==Type.NOTIFICATION_EVENT) {assertTrue(frames.trySend(com.google.gson.Gson().fromJson(frame.body,NotificationEvent::class.java)).isSuccess);true} else false
        }).use {computer->
            computer.connect(holdFirstFrame=false)
            assertEquals(NotificationAction.CLEAR,withTimeout(5000) {frames.receive()}.action)
            val paired=app.preferences.read().devices.single()
            app.preferences.notificationMaster(true)
            val pkg=i.context.packageName
            app.preferences.notificationSelection(false,NotificationConsent.app(Process.myUserHandle().hashCode(),pkg),true)
            withTimeout(10000) {app.prefs.first {it.notificationConsent.apps.isNotEmpty()}}
            withTimeout(10000) {while(!app.phoneNotifications.connected)delay(20)}
            i.uiAutomation.grantRuntimePermission(pkg,Manifest.permission.POST_NOTIFICATIONS)
            // This is a project test-runner launch of the separate test app's posting fixture.
            i.context.startActivity(Intent(i.context,MirrorNotificationPosterActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            val event=withTimeout(15000) {var found:NotificationEvent;do {found=frames.receive()}while(found.action!=NotificationAction.UPSERT);found}
            assertEquals(pkg,event.appPackage);assertEquals("Ana",event.title);assertEquals("¿Llegaste?",event.text)
            assertTrue(event.key!!.matches(Regex("[0-9a-f]{32}")))
            assertEquals(NotificationConsent.computer(paired),app.preferences.read().notificationConsent.computers.single())
            i.context.startActivity(Intent(i.context,MirrorNotificationPosterActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK).putExtra("text","Updated"))
            val update=withTimeout(5000) {frames.receive()}
            assertEquals(NotificationAction.UPSERT,update.action);assertEquals(event.key,update.key);assertEquals("Updated",update.text)
            i.context.startActivity(Intent(i.context,MirrorNotificationPosterActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK).putExtra("secret",true))
            val removal=withTimeout(5000) {frames.receive()}
            assertEquals(NotificationAction.REMOVE,removal.action);assertEquals(event.key,removal.key);assertNull(removal.text)
            app.preferences.notificationMaster(false)
            val clear=withTimeout(5000) {var found:NotificationEvent;do {found=frames.receive()}while(found.action!=NotificationAction.CLEAR);found}
            assertNull(clear.key);assertNull(clear.title);assertNull(clear.text)
        }
        assertTrue(component.contains("PhoneNotificationListener"))
    }
}
