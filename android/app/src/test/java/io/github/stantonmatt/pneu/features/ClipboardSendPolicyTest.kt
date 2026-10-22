package io.github.stantonmatt.pneu.features

import android.app.Application
import android.content.*
import android.os.PersistableBundle
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.sha256
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ClipboardSendPolicyTest {
    private val a=Peer("A","pin-a","pc-a",emptyList(),generation=1)
    private val b=a.copy(name="B",pin="pin-b",deviceId="pc-b")
    private val p=Prefs(deviceId="phone",peers=listOf(a,b),peer=a)
    private val targets=p.devices.map {DeliveryBinding.from(p,it.deviceId)!!}
    @Test fun explicitResendExcludesTheActualSourceEvenAfterAnotherClipboardWasReceived() {
        val clip=ClipboardDelivery.receivedClip("Pneu","link",sha256("link".toByteArray()),b.deviceId)
        val latest=p.copy(lastReceivedHash="different",lastReceivedSource=a.deviceId)
        assertEquals(listOf(targets[0]),ClipboardSendPolicy.targets(clip,latest,targets))
        assertTrue(ClipboardSendPolicy.targets(clip,p,targets,automatic=true).isEmpty())
        assertEquals(b.deviceId,clip.description.extras!!.getString(ClipboardDelivery.OWN_SOURCE))
    }
    @Test fun legacyOwnedAndMarkerStrippedReceivedClipsUseTheLastReceivedSource() {
        val hash=sha256("link".toByteArray());val latest=p.copy(lastReceivedHash=hash,lastReceivedSource=a.deviceId,recentClipboardHashes=listOf(hash))
        val clip=ClipData.newPlainText("clipboard","link")
        assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(clip,latest,targets))
        assertTrue(ClipboardSendPolicy.targets(clip,latest,targets,automatic=true).isEmpty())
        clip.description.extras=PersistableBundle().apply {putBoolean(ClipboardDelivery.OWN_CLIP,true)}
        assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(clip,latest,targets))
        assertTrue(ClipboardSendPolicy.targets(clip,latest,targets,automatic=true).isEmpty())
    }
    @Test fun sensitiveClipIsRejectedForExplicitAndAutomaticSendsAndNeverAcquiresNewTargets() {
        val clip=ClipboardDelivery.receivedClip("Pneu","secret","hash",a.deviceId)
        clip.description.extras=PersistableBundle(clip.description.extras).apply {putBoolean(ClipDescription.EXTRA_IS_SENSITIVE,true)}
        for(automatic in listOf(false,true))assertTrue(ClipboardSendPolicy.targets(clip,p,targets,automatic).isEmpty())
        val userClip=ClipData.newPlainText("user","new content")
        assertEquals(targets,ClipboardSendPolicy.targets(userClip,p,targets))
        assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(userClip,p.copy(peers=listOf(b)),targets))
        assertEquals(listOf(targets[0]),ClipboardSendPolicy.targets(userClip,p,listOf(targets[0])))
    }
    @Test fun automaticEchoSuppressionIncludesOlderReceivedHashes() {
        val clip=ClipData.newPlainText("other-app","older received")
        val latest=p.copy(lastReceivedHash="newer",lastReceivedSource=b.deviceId,recentClipboardHashes=listOf(sha256("older received".toByteArray())))
        assertTrue(ClipboardSendPolicy.targets(clip,latest,targets,automatic=true).isEmpty())
    }
    @Test fun historyRestoresExcludeTheirOwnSourceWithStrippedOrLegacyOwnershipExtras() {
        val x=sha256("X".toByteArray());val y=sha256("Y".toByteArray())
        val latest=p.copy(lastReceivedHash=y,lastReceivedSource=b.deviceId,recentClipboardHashes=listOf(x,y),
            recentClipboardSources=mapOf(x to a.deviceId,y to b.deviceId))
        for(owned in listOf(false,true)) {
            val clip=ClipData.newPlainText("history","X").apply {if(owned)description.extras=PersistableBundle().apply {putBoolean(ClipboardDelivery.OWN_CLIP,true)}}
            assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(clip,latest,targets))
            assertTrue(ClipboardSendPolicy.targets(clip,latest,targets,automatic=true).isEmpty())
        }
    }
}
