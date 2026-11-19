package io.github.stantonmatt.pneu.features

import android.content.Intent
import android.os.BatteryManager
import io.github.stantonmatt.pneu.protocol.BatteryState
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=android.app.Application::class)
class PhoneBatteryTest {
    private fun sample(level:Int=82,scale:Int=100,status:Int=BatteryManager.BATTERY_STATUS_CHARGING,plug:Int=1)=
        batterySample(Intent(Intent.ACTION_BATTERY_CHANGED).putExtra(BatteryManager.EXTRA_LEVEL,level)
            .putExtra(BatteryManager.EXTRA_SCALE,scale).putExtra(BatteryManager.EXTRA_STATUS,status).putExtra(BatteryManager.EXTRA_PLUGGED,plug))
    @Test fun fullOnExternalPowerIsChargingAndSamplesAreWholePercent() {
        assertEquals(BatteryState(82,true),sample(165,200))
        assertEquals(BatteryState(100,true),sample(100,status=BatteryManager.BATTERY_STATUS_FULL))
        assertEquals(BatteryState(82,false),sample(status=BatteryManager.BATTERY_STATUS_NOT_CHARGING))
        assertEquals(BatteryState(82,false),sample(plug=0))
    }
    @Test fun missingOrInvalidOsSampleClears() {
        assertEquals(BatteryState(),batterySample(Intent()))
        for(value in listOf(sample(-1),sample(101),sample(scale=0),sample(status=-1)))assertEquals(BatteryState(),value)
    }
}
