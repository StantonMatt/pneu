package io.github.stantonmatt.pneu.features

import android.app.*
import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.data.Preferences
import kotlinx.coroutines.*
import kotlinx.coroutines.test.StandardTestDispatcher
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.*
import org.robolectric.annotation.*
import org.robolectric.shadows.ShadowNotificationManager
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,shadows=[SetupNotificationsTest.ThrowingNotifications::class])
class SetupNotificationsTest {
    @Implements(NotificationManager::class)
    class ThrowingNotifications:ShadowNotificationManager() {
        var failPost=false
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
        assertEquals(listOf("Allow","Don't allow"),n.actions.map {it.title.toString()})
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
