package io.github.stantonmatt.pneu.features

import android.app.Application
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.media.AudioManager
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class RingVolumeRecoveryTest {
    @Test fun restartRestoresJournaledVolumeWithoutReplayingRing() {
        val c=RuntimeEnvironment.getApplication();val audio=c.getSystemService(AudioManager::class.java)
        val notifications=c.getSystemService(NotificationManager::class.java)
        notifications.createNotificationChannel(NotificationChannel(PhoneRinging.CHANNEL,"Ringing",NotificationManager.IMPORTANCE_HIGH))
        fun post(id:Int)=notifications.notify(id,Notification.Builder(c,PhoneRinging.CHANNEL).setContentTitle("Pneu is ringing").build())
        post(PhoneRinging.NOTIFICATION_ID);post(71)
        val journal=c.getSharedPreferences("ring-volume",0)
        audio.setStreamVolume(AudioManager.STREAM_ALARM,2,0)
        journal.edit().putInt("previous",2).commit()
        audio.setStreamVolume(AudioManager.STREAM_ALARM,audio.getStreamMaxVolume(AudioManager.STREAM_ALARM),0)
        assertTrue(RingAudio.restorePending(c));assertEquals(2,audio.getStreamVolume(AudioManager.STREAM_ALARM))
        assertFalse(journal.contains("previous"))
        assertNull(shadowOf(notifications).getNotification(PhoneRinging.NOTIFICATION_ID))
        assertNotNull(shadowOf(notifications).getNotification(71))
        // A stale notification can also survive after the journal was cleared.
        post(PhoneRinging.NOTIFICATION_ID)
        audio.setStreamVolume(AudioManager.STREAM_ALARM,3,0)
        assertTrue(RingAudio.restorePending(c));assertEquals(3,audio.getStreamVolume(AudioManager.STREAM_ALARM))
        assertNull(shadowOf(notifications).getNotification(PhoneRinging.NOTIFICATION_ID))
    }
}
