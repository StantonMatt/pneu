package io.github.stantonmatt.pneu

import android.app.Application
import android.app.Notification
import kotlinx.coroutines.test.*
import kotlinx.coroutines.ExperimentalCoroutinesApi
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.*
import org.robolectric.annotation.Config

@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class StatusNotificationUpdatesTest {
    @Test fun burstsPostTheLatestContentAndUnchangedContentDoesNotPostAgain()=runTest {
        val c=RuntimeEnvironment.getApplication()
        var text="Connecting"
        val posted=mutableListOf<Notification>()
        val updates=StatusNotificationUpdates(backgroundScope,{
            Notification.Builder(c,"status").setContentTitle("Pneu").setContentText(text).build()
        },posted::add)
        repeat(100) {updates.request()};runCurrent()
        advanceTimeBy(499);assertTrue(posted.isEmpty())
        text="Connected";updates.request();advanceTimeBy(1);runCurrent()
        assertEquals(listOf("Connected"),posted.map {it.extras.getString(Notification.EXTRA_TEXT)})
        repeat(100) {updates.request()};runCurrent();advanceTimeBy(500);runCurrent()
        assertEquals(1,posted.size)
        text="Sending";updates.request();runCurrent();advanceTimeBy(500);runCurrent()
        assertEquals(2,posted.size)
        text="Disconnected";updates.request();runCurrent();advanceTimeBy(500);runCurrent()
        assertEquals("Disconnected",posted.last().extras.getString(Notification.EXTRA_TEXT))
    }
}
