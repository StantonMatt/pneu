package io.github.stantonmatt.pneu.features

import android.app.Notification
import android.app.NotificationManager
import android.app.Person
import android.os.Process
import android.service.notification.NotificationListenerService.Ranking
import android.service.notification.NotificationListenerService.RankingMap
import android.service.notification.StatusBarNotification
import io.github.stantonmatt.pneu.PneuApp
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.*
import org.robolectric.util.ReflectionHelpers

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=PneuApp::class,shadows=[PhoneNotificationListenerTest.AlertRanking::class])
class PhoneNotificationListenerTest {
    @Implements(RankingMap::class)
    class AlertRanking {
        @Implementation fun getRanking(key:String,out:Ranking):Boolean {
            ReflectionHelpers.setField(out,"mImportance",NotificationManager.IMPORTANCE_HIGH)
            return true
        }
    }
    private val app get()=(RuntimeEnvironment.getApplication() as PneuApp)
    private lateinit var listener:PhoneNotificationListener
    private val pkg="org.example.chat"
    private val consent get()=app.graph.prefs.value.notificationConsent
    @Before fun setup()=runBlocking<Unit> {
        app.graph.awaitPrefs()
        app.graph.preferences.notificationMaster(true)
        app.graph.preferences.notificationSelection(false,NotificationConsent.app(Process.myUserHandle().hashCode(),pkg),true)
        withTimeout(5000) {while(pkg !in app.graph.prefs.value.notificationConsent.apps.map {it.substringAfter('/')})yield()}
        val info=android.content.pm.ApplicationInfo().apply {packageName=pkg;nonLocalizedLabel="Chat";icon=android.R.drawable.ic_dialog_email}
        org.robolectric.Shadows.shadowOf(app.packageManager).installPackage(android.content.pm.PackageInfo().apply {packageName=pkg;applicationInfo=info})
        listener=Robolectric.buildService(PhoneNotificationListener::class.java).create().get()
        app.graph.phoneNotifications.replace(emptyList())
    }
    @After fun cleanup() {if(::listener.isInitialized)listener.onDestroy();app.graph.scope.cancel();app.graph.database.close()}
    private fun notification(text:String="first",title:String="Ana",onlyOnce:Boolean=false)=
        Notification.Builder(app,"chat").setSmallIcon(android.R.drawable.ic_dialog_email)
            .setContentTitle(title).setContentText(text).setOnlyAlertOnce(onlyOnce).setVisibility(Notification.VISIBILITY_PRIVATE)
    private fun sbn(n:Notification,time:Long=1)=StatusBarNotification(pkg,pkg,42,null,10000,0,0,n,Process.myUserHandle(),time)
    private fun post(n:Notification,time:Long=1) {listener.onNotificationPosted(sbn(n,time))}
    private fun drain(session:NotificationMirrorSession):List<NotificationEvent> {
        val events=mutableListOf<NotificationEvent>()
        repeat(10) {
            val next=session.next(app.graph.phoneNotifications.snapshot(),consent,true) ?: return events
            events+=next;session.committed(next)
        }
        error("unbounded")
    }
    @Test fun groupAlertSummaryChildIsAdmitted() {
        post(notification().setGroup("chat").setGroupAlertBehavior(Notification.GROUP_ALERT_SUMMARY).build())
        assertEquals(1,drain(NotificationMirrorSession()).count {it.action==NotificationAction.UPSERT})
    }
    @Test fun groupSummaryIsExcluded() {
        post(notification().setGroup("chat").setGroupAlertBehavior(Notification.GROUP_ALERT_SUMMARY).setGroupSummary(true).build())
        assertEquals(listOf(NotificationAction.CLEAR),drain(NotificationMirrorSession()).map {it.action})
    }
    @Test fun onlyAlertOnceChangedContentIsSentOnce() {
        val session=NotificationMirrorSession()
        post(notification(onlyOnce=true).build());assertEquals(1,drain(session).count {it.action==NotificationAction.UPSERT})
        post(notification("changed",onlyOnce=true).build(),2)
        assertEquals("changed",drain(session).single().text)
        post(notification("changed",onlyOnce=true).build(),3);assertTrue(drain(session).isEmpty())
        post(notification("changed","Bea",onlyOnce=true).build(),4)
        assertEquals("Bea",drain(session).single().title)
    }
    @Test fun reconnectDoesNotReplayUnchangedButSendsChangedKey() {
        post(notification(onlyOnce=true).build());drain(NotificationMirrorSession())
        val session=NotificationMirrorSession(app.graph.phoneNotifications.postPosition())
        assertEquals(listOf(NotificationAction.CLEAR),drain(session).map {it.action})
        post(notification(onlyOnce=true).build(),2);assertTrue(drain(session).isEmpty())
        post(notification("after reconnect",onlyOnce=true).build(),3)
        assertEquals("after reconnect",drain(session).single().text)
    }
    @Test fun snapshotAndConsentRefreshDoNotReplayUnchangedButSendChangedKey() {
        val n=notification(onlyOnce=true).build()
        app.graph.phoneNotifications.replace(listOf(MirrorNotice(sbn(n).key,0,pkg,"Chat",1,"Ana","first")))
        val session=NotificationMirrorSession(app.graph.phoneNotifications.postPosition());drain(session)
        post(n,2);assertTrue(drain(session).isEmpty())
        post(notification("fresh content",onlyOnce=true).build(),3)
        assertEquals("fresh content",drain(session).single().text)
    }
    @Test fun unchangedLiveContentIsNotResentWhenTimestampChanges() {
        val session=NotificationMirrorSession()
        post(notification().build());drain(session)
        post(notification().build(),2);assertTrue(drain(session).isEmpty())
    }
    @Test fun blankExtrasUseLastMessagingStyleMessage() {
        val n=notification().setStyle(Notification.MessagingStyle(Person.Builder().setName("Me").build())
            .addMessage("last message",1,Person.Builder().setName("Bea").build())).build()
        n.extras.putCharSequence(Notification.EXTRA_TITLE,"  ");n.extras.putCharSequence(Notification.EXTRA_TEXT,"")
        post(n)
        val event=drain(NotificationMirrorSession()).last()
        assertEquals("Bea",event.title);assertEquals("last message",event.text)
    }
}
