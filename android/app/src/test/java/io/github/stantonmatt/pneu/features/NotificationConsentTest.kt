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
    private suspend fun replacementFixture(enabled:Boolean,body:suspend (Preferences,io.github.stantonmatt.pneu.AppGraph,DeliveryBinding,Peer,Peer)->Unit) {
        val context=RuntimeEnvironment.getApplication()
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val file=File(context.cacheDir,"replacement-consent-${System.nanoTime()}.preferences_pb")
        val store=PreferenceDataStoreFactory.create(scope=owner) {file}
        val preferences=Preferences(context,store)
        val peer=Peer("New PC","b".repeat(64),"b".repeat(32),emptyList(),generation=2)
        val other=peer.copy(name="Other PC",deviceId="d".repeat(32),pin="d".repeat(64))
        val graph=io.github.stantonmatt.pneu.AppGraph(context,preferences,
            CoroutineScope(SupervisorJob()+kotlinx.coroutines.test.StandardTestDispatcher()))
        try {
            store.edit {
                it[stringPreferencesKey("deviceId")]="c".repeat(32);PeerRegistry.write(it,listOf(peer,other))
                it[stringSetPreferencesKey("mirrorApps")]=setOf("0/chat")
                it[booleanPreferencesKey("mirrorNamesOnly")]=true
                // The master remembers its computer choices while it is off.
                it[booleanPreferencesKey("mirrorInitialized")]=true;it[booleanPreferencesKey("mirrorEnabled")]=enabled
                it[stringSetPreferencesKey("mirrorComputers")]=setOf(NotificationConsent.computer(other))
            }
            body(preferences,graph,requireNotNull(DeliveryBinding.from(preferences.read(),peer.deviceId)),peer,other)
        }finally {
            graph.scope.cancel();graph.database.close();context.deleteDatabase("outgoing.db")
            owner.cancel();owner.coroutineContext[Job]?.join();file.delete()
        }
    }
    @Test fun replacementActionGrantsOnlyTheExactNewComputerAndPreservesAppChoices()=runBlocking<Unit> {
        replacementFixture(enabled=true) {preferences,graph,binding,peer,other->
            graph.withBinding(binding) {graph.replacementNotificationAction.turnOnForNewComputer(binding)}
            val consent=preferences.read().notificationConsent
            assertTrue(consent.enabled);assertTrue(consent.initialized);assertTrue(consent.namesOnly)
            assertEquals(setOf(NotificationConsent.computer(other),NotificationConsent.computer(peer)),consent.computers)
            assertEquals(setOf("0/chat"),consent.apps)
            preferences.edit {PeerRegistry.write(it,listOf(peer.copy(generation=3),other))}
            assertTrue(runCatching {graph.replacementNotificationAction.turnOnForNewComputer(binding)}.isFailure)
            assertFalse(NotificationConsent.computer(peer.copy(generation=3)) in preferences.read().notificationConsent.computers)
        }
    }
    @Test fun replacementActionWithMasterOffNeverResumesOtherComputers()=runBlocking<Unit> {
        replacementFixture(enabled=false) {preferences,graph,binding,peer,other->
            graph.withBinding(binding) {graph.replacementNotificationAction.turnOnForNewComputer(binding)}
            val p=preferences.read();val consent=p.notificationConsent
            assertFalse(consent.enabled);assertTrue(consent.initialized)
            assertEquals(setOf(NotificationConsent.computer(other),NotificationConsent.computer(peer)),consent.computers)
            assertFalse(consent.allows(p,p.devices.single {it.deviceId==other.deviceId},access=true,capable=true))
            assertFalse(consent.allows(p,p.devices.single {it.deviceId==peer.deviceId},access=true,capable=true))
        }
    }
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
