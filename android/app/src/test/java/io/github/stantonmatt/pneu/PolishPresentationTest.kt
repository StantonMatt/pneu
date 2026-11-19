package io.github.stantonmatt.pneu

import android.app.Application
import android.content.pm.PackageInstaller.*
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import org.junit.Test
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class PolishPresentationTest {
    private val old=Peer("Desktop","a".repeat(64),"a".repeat(32),emptyList(),generation=1,lastConnected=100)
    private val fresh=old.copy(pin="b".repeat(64),deviceId="b".repeat(32),generation=2)
    private val p=Prefs(deviceId="c".repeat(32),peers=listOf(old,fresh))
    private val context get()=RuntimeEnvironment.getApplication()
    @Test fun compactProgressMatchesDesktopUnitsAndRounding() {
        for((sent,total,want) in listOf(Triple(262_000_000L,432_000_000L,"262 of 432 MB"),Triple(600_000_000L,1_800_000_000L,"0.6 of 1.8 GB"),Triple(1_200_000L,2_400_000L,"1.2 of 2.4 MB"),Triple(320L,900L,"320 of 900 B"),Triple(12_000_000L,40_000_000L,"12 of 40 MB"),Triple(1_200_000_000L,4_000_000_000L,"1.2 of 4.0 GB")))assertEquals(want,UiPresentation.sizePair(sent,total))
        assertEquals("Sending · 262 of 432 MB",UiPresentation.sendingProgress(context,262_000_000,432_000_000))
        assertEquals("900 of 900 B",UiPresentation.sizePair(901,900))
    }
    @Test fun deviceCardShowsFileAndFolderProgressOnlyWhileInFlight() {
        val link=LinkState("Connected",sent=300_000_000,total=432_000_000,transferMediaId="folder",transferKind="file",transferFileName="Wedding footage")
        assertEquals("Sending Wedding footage · 300 of 432 MB",UiPresentation.deviceTransfer(context,link))
        assertEquals("Sending Report.pdf · 300 of 432 MB",UiPresentation.deviceTransfer(context,link.copy(transferFileName="Report.pdf")))
        assertNull(UiPresentation.deviceTransfer(context,link.copy(transferMediaId="")))
    }
    @org.robolectric.annotation.GraphicsMode(org.robolectric.annotation.GraphicsMode.Mode.NATIVE)
    @Test fun appIconsShareRoundedTileAndInsetAcrossCircleAndSquareDrawables() {
        val fill=android.graphics.Color.GRAY;val ink=android.graphics.Color.RED
        val square=android.graphics.drawable.ColorDrawable(ink)
        val circle=android.graphics.drawable.GradientDrawable().apply {shape=android.graphics.drawable.GradientDrawable.OVAL;setColor(ink)}
        for(drawable in listOf(square,circle)) {
            drawable.setBounds(1,2,3,4)
            val bitmap=notificationIconBitmap(drawable,108,fill)
            assertEquals(108,bitmap.width);assertEquals(108,bitmap.height)
            assertEquals(android.graphics.Color.TRANSPARENT,bitmap.getPixel(0,0))
            assertEquals(fill,bitmap.getPixel(54,6))
            assertEquals(ink,bitmap.getPixel(54,54))
            assertEquals(android.graphics.Rect(1,2,3,4),drawable.bounds)
        }
        val bitmap=notificationIconBitmap(square,108,fill)
        assertEquals(fill,bitmap.getPixel(11,54));assertEquals(ink,bitmap.getPixel(12,54))
    }
    @Test fun restrictedCardRequiresExplicitFileInstallProvenance() {
        assertTrue(restrictedNotificationSource(PACKAGE_SOURCE_LOCAL_FILE))
        assertTrue(restrictedNotificationSource(PACKAGE_SOURCE_DOWNLOADED_FILE))
        for(source in listOf(PACKAGE_SOURCE_UNSPECIFIED,PACKAGE_SOURCE_OTHER,PACKAGE_SOURCE_STORE,-1))assertFalse(restrictedNotificationSource(source))
    }
    @Test fun connectedRetryPreparesImmediatelyAndDoesNotCountAsWaiting() {
        val row=Outgoing().apply {peerId=old.deviceId;mediaId="retry";state="waiting";automatic=false}
        val links=mapOf(old.deviceId to LinkState("Connected"))
        val item=RecentProjection.items(listOf(row),p,links).single()
        assertEquals("Preparing",UiPresentation.recentOutcome(context,item))
        assertEquals(0,RecentProjection.waitingCount(listOf(row),links))
        assertTrue(RecentProjection.items(listOf(row),p.copy(paused=true),links).single().recipients.single().preparing)
        row.kind="recording"
        assertFalse(RecentProjection.items(listOf(row),p.copy(originals=false),mapOf(old.deviceId to LinkState("Connected",cellular=true))).single().recipients.single().preparing)
        row.kind="file"
        assertEquals(1,RecentProjection.waitingCount(listOf(row),emptyMap()))
        row.state="failed";assertEquals(0,RecentProjection.waitingCount(listOf(row),emptyMap()))
        row.state="sending";assertEquals(0,RecentProjection.waitingCount(listOf(row),emptyMap()))
    }
    @Test fun replacementPromptAndKnownPairedCertificateAreNeutralButUnknownStaysDanger() {
        val mismatch=LinkState(ConnectionStatus.FINGERPRINT_MISMATCH.key)
        val choice=ReplacementChoice(ConfirmedBinding.from(p,old.deviceId),ConfirmedBinding.from(p,fresh.deviceId),"computer",old.name,100)
        val offered=p.copy(replacements=ReplacementState(offers=listOf(choice)))
        assertEquals(ConnectionStatus.UNREACHABLE,DeviceProjection.displayLink(offered,old,mismatch).connectionStatus)
        assertEquals(ConnectionStatus.UNREACHABLE,DeviceProjection.displayLink(offered,old,LinkState(ConnectionStatus.CONNECTING.key)).connectionStatus)
        assertEquals(ConnectionStatus.UNREACHABLE,DeviceProjection.displayLink(p,old,mismatch.copy(mismatchPin=fresh.pin)).connectionStatus)
        assertEquals(ConnectionStatus.FINGERPRINT_MISMATCH,DeviceProjection.displayLink(p,old,mismatch.copy(mismatchPin="d".repeat(64))).connectionStatus)
        assertEquals(ConnectionStatus.CONNECTED,DeviceProjection.displayLink(offered,old,LinkState("Connected")).connectionStatus)
    }
}
