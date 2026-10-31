package io.github.stantonmatt.pneu.features

import android.app.*
import android.content.*
import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.PneuApp
import io.github.stantonmatt.pneu.PneuService
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.data.Preferences
import kotlinx.coroutines.*
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.TestDispatcher
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.*
import org.robolectric.annotation.*
import org.robolectric.shadows.ShadowNotificationManager
import org.robolectric.util.ReflectionHelpers
import java.io.File
import kotlin.coroutines.ContinuationInterceptor

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,shadows=[SetupNotificationsTest.ThrowingNotifications::class])
class SetupNotificationsTest {
    @Implements(NotificationManager::class)
    class ThrowingNotifications:ShadowNotificationManager() {
        var failPost=false
        var requestCancels=0
        @Implementation override fun cancel(tag:String?,id:Int) {
            if(tag==SetupNotifications.REQUEST)requestCancels++
            super.cancel(tag,id)
        }
        @Implementation override fun notify(tag:String?,id:Int,notification:Notification) {
            if(failPost)throw SecurityException("Injected post failure")
            super.notify(tag,id,notification)
        }
    }
    private val c get()=RuntimeEnvironment.getApplication()
    private val nm get()=c.getSystemService(NotificationManager::class.java)
    private val shadow get()=Shadows.shadowOf(nm) as ThrowingNotifications
    private val peer=Peer("Laptop","b".repeat(64),"b".repeat(32),emptyList(),generation=1)
    private val notice=SetupNotice("test-notice",peer.deviceId,peer.name,"c".repeat(32),"Computer")
    private fun resetUiStore()=SetupUiStore::class.java.getDeclaredField("instance").apply {isAccessible=true}.set(null,null)
    private suspend fun fixture(body:suspend (AppGraph,Prefs)->Unit) {
        val file=File(c.cacheDir,"notification-test.preferences_pb")
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val prefs=Preferences(c,PreferenceDataStoreFactory.create(scope=owner) {file})
        val graph=AppGraph(c,prefs,CoroutineScope(SupervisorJob()+StandardTestDispatcher()))
        try {
            resetUiStore()
            c.setupUi.presentedNotices.value=emptySet();c.setupUi.notifiedPrompts.value=emptySet()
            SetupNotifications.channel(c)
            prefs.edit {p->p[stringPreferencesKey("deviceId")]="a".repeat(32);PeerRegistry.write(p,listOf(peer));SetupStorage.write(p,SetupState(notices=listOf(notice)))}
            body(graph,prefs.read())
        }finally {graph.scope.cancel();graph.database.close();owner.coroutineContext[Job]!!.cancelAndJoin();c.deleteDatabase("outgoing.db");file.delete()}
    }
    @Test fun blockedNotificationsRetainJoinsAndDoNotAcknowledgeRequests()=runBlocking<Unit> {
        fixture {app,p->
            val prompt=SetupPrompt(peer.deviceId,peer.pin,peer.generation,0,peer.name)
            val requested=p.copy(membership=p.membership.copy(prompts=listOf(prompt)))
            val posted=mutableSetOf<String>()
            shadow.setNotificationsEnabled(false)
            SetupNotifications.show(app,nm,requested,false,emptySet(),posted)
            assertEquals(listOf(notice),app.preferences.read().membership.notices)
            assertTrue(posted.isEmpty());assertTrue(c.setupUi.notifiedPrompts.value.isEmpty());assertEquals(0,shadow.size())
            shadow.setNotificationsEnabled(true)
            nm.deleteNotificationChannel("setup_requests")
            nm.createNotificationChannel(NotificationChannel("setup_requests","Link requests",NotificationManager.IMPORTANCE_NONE))
            nm.deleteNotificationChannel("alerts")
            nm.createNotificationChannel(NotificationChannel("alerts","Alerts",NotificationManager.IMPORTANCE_NONE))
            assertFalse(SetupNotifications.canPost(c))
            SetupNotifications.show(app,nm,requested,false,emptySet(),posted)
            assertEquals(listOf(notice),app.preferences.read().membership.notices)
            assertTrue(c.setupUi.notifiedPrompts.value.isEmpty())
        }
    }
    @Test fun throwingPostCanRetryWithoutLosingTheDurableNotice()=runBlocking<Unit> {
        fixture {app,p->
            val posted=mutableSetOf<String>();shadow.failPost=true
            try {SetupNotifications.show(app,nm,p,false,emptySet(),posted);fail("Expected post failure")}catch(_:SecurityException) {}
            assertTrue(posted.isEmpty());assertEquals(listOf(notice),app.preferences.read().membership.notices)
            shadow.failPost=false
            SetupNotifications.show(app,nm,p,false,emptySet(),posted)
            assertEquals(setOf(notice.id),posted);assertTrue(app.preferences.read().membership.notices.isEmpty())
        }
    }
    @Test fun alsoLinkedWaitsForHomeAndPresentedJoinsNeverNotify()=runBlocking<Unit> {
        fixture {app,p->
            val also=notice.copy(alsoLinked=true)
            app.preferences.edit {SetupStorage.write(it,p.membership.copy(notices=listOf(also)))}
            SetupNotifications.show(app,nm,app.preferences.read(),false,emptySet(),mutableSetOf())
            assertEquals(listOf(also),app.preferences.read().membership.notices);assertEquals(0,shadow.size())
            c.setupUi.presentNotices(setOf(notice.id))
            SetupNotifications.show(app,nm,p,false,emptySet(),mutableSetOf())
            assertEquals(0,shadow.size())
            resetUiStore() // A recreated store must still suppress the displayed notice.
            assertTrue(notice.id in c.setupUi.presentedNotices.value)
        }
    }
    @Test fun requestsHaveTheirOwnExplicitGroupAndKeepBothActions() {
        SetupNotifications.channel(c)
        val prompt=SetupPrompt(peer.deviceId,peer.pin,1,0,peer.name)
        SetupNotifications.postRequest(c,Prefs(peers=listOf(peer)),listOf(prompt))
        val n=shadow.getNotification(SetupNotifications.REQUEST,0)
        assertEquals("setup_requests",n.group)
        assertEquals("setup_requests",n.channelId)
        assertEquals(NotificationManager.IMPORTANCE_HIGH,nm.getNotificationChannel(n.channelId).importance)
        assertFalse(n.flags and Notification.FLAG_GROUP_SUMMARY!=0)
        assertEquals(listOf("Don't allow","Allow"),n.actions.map {it.title.toString()})
    }
    @Test fun readmissionNotificationCarriesExactBarrierFencesAndStaleActionsDoNothing()=runBlocking<Unit> {
        fixture {app,p->
            val removed=io.github.stantonmatt.pneu.protocol.SetupMember("e".repeat(32),"e".repeat(64),"computer","Old laptop","removed","10",peer.deviceId)
            val observation=Readmission(removed.copy(status="member",hlc="20"),peer.deviceId,peer.name,3)
            val state=p.membership.copy(setupId="f".repeat(32),generation=3,barriers=mapOf(removed.key to RemovalBarrier(removed,readmission=observation)),notices=emptyList())
            app.preferences.edit {SetupStorage.write(it,state)}
            val current=app.preferences.read();val prompt=SetupPresentation.readmissions(current).single()
            SetupNotifications.show(app,nm,current,false,emptySet(),mutableSetOf())
            val notification=shadow.getNotification(prompt.actionKey,0)
            assertEquals("setup_requests",notification.channelId)
            assertEquals(listOf("Keep removed","Allow"),notification.actions.map {it.title.toString()})
            val intent=Shadows.shadowOf(notification.actions[1].actionIntent).savedIntent
            assertEquals(SetupNotifications.READMIT_ALLOW,intent.action)
            assertArrayEquals(arrayOf(prompt.actionKey),intent.getStringArrayExtra("keys"))
            assertEquals("Old laptop was removed from this phone",notification.extras.getCharSequence(Notification.EXTRA_TITLE).toString())
            assertEquals("Laptop added it again",notification.extras.getCharSequence(Notification.EXTRA_TEXT).toString())
            assertEquals("Laptop added it again",notification.extras.getCharSequence(Notification.EXTRA_BIG_TEXT).toString())
            SetupNotifications.show(app,nm,current,true,emptySet(),mutableSetOf())
            assertNull(shadow.getNotification(prompt.actionKey,0))
            app.preferences.edit {SetupStorage.write(it,state.copy(generation=4))}
            app.preferences.answerReadmission(prompt,true)
            assertTrue(app.preferences.read().membership.denies(removed.deviceId,removed.fingerprint))
            assertTrue(app.preferences.read().devices.none {it.deviceId==removed.deviceId})
        }
    }
    @Test fun repeatedEmptySnapshotsAndForegroundDismissalsCancelEachRequestOnlyOnce()=runBlocking<Unit> {
        fixture {app,p->
            val prompt=SetupPrompt(peer.deviceId,peer.pin,1,0,peer.name)
            val requested=p.copy(membership=p.membership.copy(prompts=listOf(prompt),notices=emptyList()))
            val empty=requested.copy(membership=requested.membership.copy(prompts=emptyList()))
            repeat(30) {SetupNotifications.show(app,nm,empty,false,emptySet(),mutableSetOf())}
            assertEquals(0,shadow.requestCancels)
            SetupNotifications.show(app,nm,requested,false,emptySet(),mutableSetOf())
            repeat(30) {SetupNotifications.cancelRequest(c);SetupNotifications.show(app,nm,empty,false,emptySet(),mutableSetOf())}
            assertEquals(1,shadow.requestCancels)
            assertNull(shadow.getNotification(SetupNotifications.REQUEST,0))
            // A surviving system notification still cancels after process-local markers disappear.
            SetupNotifications.postRequest(c,requested,listOf(prompt))
            repeat(30) {SetupNotifications.cancelRequest(c)}
            assertEquals(2,shadow.requestCancels)
            assertNull(shadow.getNotification(SetupNotifications.REQUEST,0))
        }
    }
    @OptIn(ExperimentalCoroutinesApi::class)
    @Test fun notificationAllowRestartsTheStoppedServiceAfterInstallingTheBinding()=runBlocking<Unit> {
        fixture {app,p->
            // Keep unrelated graph initialization out of this receiver test.
            // Robolectric records service starts without launching connection workers.
            app.scope.coroutineContext[Job]!!.children.forEach {it.cancel()}
            val scheduler=(app.scope.coroutineContext[ContinuationInterceptor] as TestDispatcher).scheduler
            scheduler.runCurrent()
            val removed=io.github.stantonmatt.pneu.protocol.SetupMember("e".repeat(32),"e".repeat(64),"computer","Old laptop","removed","10",peer.deviceId)
            val state=p.membership.copy(setupId="f".repeat(32),generation=3,roster=listOf(removed),
                barriers=mapOf(removed.key to RemovalBarrier(removed,readmission=Readmission(removed.copy(status="member",hlc="20"),peer.deviceId,peer.name,3))))
            app.preferences.edit {SetupStorage.write(it,state)}
            val current=app.preferences.read();val prompt=SetupPresentation.readmissions(current).single()
            SetupNotifications.postReadmission(c,current,prompt)
            val action=Shadows.shadowOf(shadow.getNotification(prompt.actionKey,0).actions.last().actionIntent).savedIntent
            val application=PneuApp().apply {graph=app}
            val context=object:ContextWrapper(c) {override fun getApplicationContext():Context=application}
            val receiver=SetupActionReceiver()
            val dispatched=CompletableDeferred<BroadcastReceiver.PendingResult>()
            // Supply a real pending broadcast result while using the isolated graph.
            val relay=object:BroadcastReceiver() {
                override fun onReceive(ignored:Context,intent:Intent) {
                    val pending=goAsync();ReflectionHelpers.setField(receiver,"mPendingResult",pending)
                    receiver.onReceive(context,action);dispatched.complete(pending)
                }
            }
            val relayAction="test.READMIT_ALLOW"
            c.registerReceiver(relay,IntentFilter(relayAction),Context.RECEIVER_NOT_EXPORTED)
            try {
                assertNull(Shadows.shadowOf(c).peekNextStartedService())
                c.sendBroadcast(Intent(relayAction).setPackage(c.packageName))
                Shadows.shadowOf(android.os.Looper.getMainLooper()).idle()
                val pending=withTimeout(5000) {dispatched.await()}
                withTimeout(5000) {
                    while(!Shadows.shadowOf(pending).future.isDone) {scheduler.runCurrent();delay(10)}
                }
                val admitted=app.preferences.read()
                assertTrue(admitted.devices.any {it.deviceId==removed.deviceId})
                assertFalse(admitted.membership.denies(removed.deviceId,removed.fingerprint))
                assertEquals(PneuService::class.java.name,Shadows.shadowOf(c).nextStartedService?.component?.className)
            }finally {c.unregisterReceiver(relay)}
        }
    }
    @Test fun repeatedJoinPostsUseTheSameIdentityAndOnlyAlertOnce() {
        SetupNotifications.channel(c)
        val p=Prefs(deviceId="a".repeat(32),peers=listOf(peer))
        repeat(2) {SetupNotifications.postJoined(c,p,notice)}
        assertEquals(1,shadow.size())
        val notification=shadow.getNotification(SetupNotifications.joinedTag(notice.id),0)
        assertTrue(notification.flags and Notification.FLAG_ONLY_ALERT_ONCE!=0)
    }
    @Test fun watcherReconcilesNewerUnansweredRequestsAndCanRepostAfterActivityCancellation()=runBlocking<Unit> {
        fixture {app,p->
            val first=SetupPrompt(peer.deviceId,peer.pin,1,0,"Laptop")
            val next=first.copy(bindingGeneration=2)
            for(prompt in listOf(first,first,next)) {
                val current=p.copy(peers=listOf(peer.copy(generation=prompt.bindingGeneration)),membership=SetupState(prompts=listOf(prompt)))
                SetupNotifications.show(app,nm,current,false,emptySet(),mutableSetOf())
                assertNotNull(shadow.getNotification(SetupNotifications.REQUEST,0))
                assertEquals(setOf(SetupPresentation.promptKey(prompt)),c.setupUi.notifiedPrompts.value)
                if(prompt==first)SetupNotifications.cancelRequest(c)
            }
        }
    }
}
