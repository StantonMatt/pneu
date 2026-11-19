package io.github.stantonmatt.pneu

import android.app.Application
import android.graphics.Canvas
import android.graphics.drawable.ColorDrawable
import android.os.Looper
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class NotificationIconCacheTest {
    @Test fun rasterizesOffMainAndReusesPixelsUntilSizeOrThemeChanges()=runBlocking {
        var draws=0
        val icon=object:ColorDrawable(android.graphics.Color.RED) {
            override fun draw(canvas:Canvas) {
                assertNotSame(Looper.getMainLooper(),Looper.myLooper())
                draws++
                super.draw(canvas)
            }
        }
        val app=NotificationPickerApp("0/org.example.cache","Cache fixture",icon)
        val pm=ApplicationProvider.getApplicationContext<Application>().packageManager
        val cache=NotificationIconCache()
        val first=cache.load(app,72,android.graphics.Color.GRAY,pm)!!
        assertEquals(72,first.width)
        assertSame(first,cache.load(app,72,android.graphics.Color.GRAY,pm))
        assertEquals(1,draws)
        assertNotSame(first,cache.load(app,108,android.graphics.Color.GRAY,pm))
        assertNotSame(first,cache.load(app,72,android.graphics.Color.BLACK,pm))
        assertEquals(3,draws)
        // An app update changes its install path, so the cached icon is not reused.
        assertNotSame(first,cache.load(app.copy(version="/data/app/~~updated/base.apk"),72,android.graphics.Color.GRAY,pm))
        assertEquals(4,draws)
    }
    @Test fun byteBudgetEvictsOldIcons()=runBlocking {
        val pm=ApplicationProvider.getApplicationContext<Application>().packageManager
        val icon=ColorDrawable(android.graphics.Color.RED)
        val first=NotificationPickerApp("0/org.example.first","First",icon)
        val second=NotificationPickerApp("0/org.example.second","Second",icon)
        val cache=NotificationIconCache(36*36*4)
        val bitmap=cache.load(first,36,0,pm)!!
        cache.load(second,36,0,pm)
        assertNotSame(bitmap,cache.load(first,36,0,pm))
    }
}
