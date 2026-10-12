package io.github.stantonmatt.pneu

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onRoot
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.junit.Assert.*
import androidx.test.platform.app.InstrumentationRegistry
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.res.Configuration
import android.provider.Settings
import io.github.stantonmatt.pneu.features.SettingsNavigation
import java.util.Locale

@RunWith(AndroidJUnit4::class)
class MainActivityTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    @get:Rule
    val composeRule = createAndroidComposeRule<MainActivity>()

    @Test
    fun emptyScreenLaunches() {
        composeRule.onRoot().assertIsDisplayed()
    }

    @Test fun unpairUsesSingularAndPluralWaitingCopy() {
        val context=InstrumentationRegistry.getInstrumentation().targetContext
        val resources=context.createConfigurationContext(Configuration(context.resources.configuration).apply {setLocale(Locale.US)}).resources
        assertEquals("1 waiting item won't be sent.",resources.getQuantityString(R.plurals.unpair_text,1,1))
        assertEquals("2 waiting items won't be sent.",resources.getQuantityString(R.plurals.unpair_text,2,2))
    }

    @Test fun samsungActivitiesFallBackAfterMissingAndUnexportedComponents() {
        val packageName=InstrumentationRegistry.getInstrumentation().targetContext.packageName
        val attempts=mutableListOf<Intent>()
        assertTrue(SettingsNavigation.openSamsungBattery(packageName) {intent->
            attempts+=intent
            when(attempts.size) {
                1 -> throw ActivityNotFoundException()
                2 -> throw SecurityException("Not exported")
            }
        })
        assertEquals(3,attempts.size)
        assertEquals("com.samsung.android.sm.battery.ui.BatteryNeverSleepingAppsActivity",attempts[0].component?.className)
        assertEquals("com.samsung.android.sm.battery.ui.BatteryActivity",attempts[1].component?.className)
        assertEquals(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,attempts[2].action)
        assertEquals("package:$packageName",attempts[2].data.toString())
        assertFalse(SettingsNavigation.openSamsungBattery(packageName) {throw ActivityNotFoundException()})
    }
}
