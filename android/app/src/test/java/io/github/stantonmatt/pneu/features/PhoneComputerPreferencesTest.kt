package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import io.github.stantonmatt.pneu.data.Preferences
import io.github.stantonmatt.pneu.protocol.randomId
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
class PhoneComputerPreferencesTest {
    @Test fun switchesDefaultOnPersistLocallyAndDoNotChangePeerSettings()=runBlocking<Unit>(Dispatchers.IO) {
        val context=RuntimeEnvironment.getApplication()
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val file=File(context.cacheDir,"phone-computer-${randomId()}.preferences_pb")
        val store=PreferenceDataStoreFactory.create(scope=owner) {file}
        try {
            val preferences=Preferences(context,store)
            val before=preferences.read()
            assertTrue(before.showBatteryLevel);assertTrue(before.allowRinging)
            preferences.toggle("showBatteryLevel",false);preferences.toggle("allowRinging",false)
            val readAgain=Preferences(context,store).read()
            assertFalse(readAgain.showBatteryLevel);assertFalse(readAgain.allowRinging)
            assertEquals(before.devices,readAgain.devices)
            preferences.toggle("showBatteryLevel",true);preferences.toggle("allowRinging",true)
            assertEquals(before,preferences.read())
        } finally {owner.cancel();owner.coroutineContext[Job]?.join();file.delete()}
    }
}
