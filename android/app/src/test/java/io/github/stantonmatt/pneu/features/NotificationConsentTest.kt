package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.data.Preferences
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class NotificationConsentTest {
    @Test fun defaultsSetupPreselectionPersistenceAndBindingRemoval()=runBlocking<Unit> {
        val context=RuntimeEnvironment.getApplication()
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val file=File(context.cacheDir,"notification-${System.nanoTime()}.preferences_pb")
        val store=PreferenceDataStoreFactory.create(scope=owner) {file}
        val a=Peer("A","a".repeat(64),"a".repeat(32),emptyList(),generation=1)
        val b=a.copy(name="B",deviceId="b".repeat(32),pin="b".repeat(64),generation=2)
        try {
            store.edit {it[stringPreferencesKey("deviceId")]="c".repeat(32);PeerRegistry.write(it,listOf(a))}
            val preferences=Preferences(context,store)
            assertEquals(NotificationConsent(),preferences.read().notificationConsent)
            preferences.notificationMaster(true)
            assertEquals(setOf(NotificationConsent.computer(a)),preferences.read().notificationConsent.computers)
            assertTrue(preferences.read().notificationConsent.apps.isEmpty())
            store.edit {PeerRegistry.write(it,listOf(a,b))}
            assertFalse(NotificationConsent.computer(b) in preferences.read().notificationConsent.computers)
            preferences.notificationSelection(false,"0/chat",true)
            preferences.notificationSelection(true,NotificationConsent.computer(a),false)
            preferences.notificationNamesOnly(true)
            val choices=preferences.read().notificationConsent
            preferences.notificationMaster(false);preferences.notificationMaster(true)
            assertEquals(choices,Preferences(context,store).read().notificationConsent)
            preferences.notificationSelection(true,NotificationConsent.computer(a),true)
            store.edit {PeerRegistry.remove(it,a.deviceId)}
            assertTrue(preferences.read().notificationConsent.computers.isEmpty())
            preferences.notificationSelection(true,NotificationConsent.computer(a),true)
            assertTrue(preferences.read().notificationConsent.computers.isEmpty())
            store.edit {PeerRegistry.write(it,listOf(b,a.copy(generation=3)))}
            assertTrue(preferences.read().notificationConsent.computers.isEmpty())
            assertEquals(setOf("0/chat"),preferences.read().notificationConsent.apps)
        }finally {owner.cancel();owner.coroutineContext[Job]?.join();file.delete()}
    }
}
