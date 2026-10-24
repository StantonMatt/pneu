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
    @Test fun stagedTextFilesAndImagesUseTheSameHashSourcePolicyAsPlainText() {
        for(bytes in listOf("shared text".toByteArray(),byteArrayOf(1,2,3))) {
            val hash=sha256(bytes)
            val latest=p.copy(lastReceivedHash="newer",recentClipboardSources=mapOf(hash to a.deviceId))
            assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(latest,targets,hash))
            assertTrue(ClipboardSendPolicy.targets(latest,targets,hash,automatic=true).isEmpty())
            assertTrue(ClipboardSendPolicy.targets(latest,listOf(targets[0]),hash).isEmpty())
            assertEquals(targets,ClipboardSendPolicy.targets(latest,targets,"different bytes"))
        }
    }
    @Test fun identicalReceiveAdvancesProvenanceWithoutRequiringAnOsMarkerRewrite() {
        val hash=sha256("same".toByteArray())
        val oldMarker=ClipboardDelivery.receivedClip("Pneu","same",hash,a.deviceId)
        val latest=p.copy(lastReceivedHash=hash,lastReceivedSource=b.deviceId,recentClipboardSources=mapOf(hash to b.deviceId))
        assertEquals(listOf(targets[0]),ClipboardSendPolicy.targets(oldMarker,latest,targets))
        val image=ClipData.newRawUri("received image",android.net.Uri.parse("content://images/1")).apply {
            description.extras=PersistableBundle().apply {putBoolean(ClipboardDelivery.OWN_CLIP,true);putString(ClipboardDelivery.OWN_HASH,hash);putString(ClipboardDelivery.OWN_SOURCE,a.deviceId)}
        }
        assertEquals(listOf(targets[0]),ClipboardSendPolicy.targets(image,latest,targets))
    }

    @Test fun ordinaryFileJournalSourceExcludesOnlyItsSenderAndSourceOnlySelectionTerminates() {
        assertTrue(p.recentClipboardSources.isEmpty())
        val source=ClipboardSendPolicy.fileProvenance(a.deviceId)
        assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(p,targets,source))
        assertTrue(ClipboardSendPolicy.targets(p,listOf(targets[0]),source).isEmpty())
        assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(p,listOf(targets[1]),source))
    }

    @Test fun fileJournalSourceTakesPrecedenceOverAnotherClipboardWithTheSameBytes() {
        val hash="same bytes"
        val latest=p.copy(lastReceivedHash=hash,lastReceivedSource=b.deviceId,recentClipboardSources=mapOf(hash to b.deviceId))
        val source=ClipboardSendPolicy.fileProvenance(a.deviceId)
        assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(latest,targets,source))
        assertFalse(source.needsHash)
    }

    @Test fun identicalTextAndImageAdmissionsKeepLatestSourceEvenIfHistoryChangesAfterResolution() {
        val hash=sha256("same".toByteArray())
        val latest=p.copy(lastReceivedHash=hash,lastReceivedSource=b.deviceId,recentClipboardSources=mapOf(hash to b.deviceId))
        val text=ClipboardDelivery.receivedClip("Pneu","same",hash,a.deviceId)
        val image=ClipData.newRawUri("A's retained image",android.net.Uri.parse("content://media/external/images/media/1")).apply {
            description.extras=PersistableBundle(text.description.extras)
        }
        for(clip in listOf(text,image)) {
            val source=ClipboardSendPolicy.clipboardProvenance(clip,latest)
            assertEquals(b.deviceId,source.source);assertFalse(source.needsHash)
            val captured=ClipboardSendPolicy.targets(latest,targets,source)
            assertEquals(listOf(targets[0]),captured)
            // Later admission checks may revalidate recipients, but must not
            // reinterpret A's retained URI or newer clipboard history.
            val changed=latest.copy(lastReceivedSource=a.deviceId,recentClipboardSources=mapOf(hash to a.deviceId))
            assertEquals(captured,ClipboardSendPolicy.targets(changed,captured,source))
            assertTrue(ClipboardSendPolicy.targets(changed,captured,source,automatic=true).isEmpty())
        }
    }

    @Test fun unidentifiedUriDefersHashResolutionWhileJournalAndMarkedClipboardResolveIndependently() {
        val raw=ClipData.newRawUri("image",android.net.Uri.parse("content://media/external/images/media/1"))
        assertTrue(ClipboardSendPolicy.clipboardProvenance(raw,p).needsHash)
        assertTrue(ClipboardSendPolicy.fileProvenance("").needsHash)
        val hash="identical image"
        raw.description.extras=PersistableBundle().apply {
            putBoolean(ClipboardDelivery.OWN_CLIP,true);putString(ClipboardDelivery.OWN_HASH,hash);putString(ClipboardDelivery.OWN_SOURCE,a.deviceId)
        }
        val latest=p.copy(recentClipboardSources=mapOf(hash to b.deviceId))
        assertEquals(listOf(targets[0]),ClipboardSendPolicy.targets(latest,targets,ClipboardSendPolicy.clipboardProvenance(raw,latest)))
        assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(latest,targets,ClipboardSendPolicy.fileProvenance(a.deviceId)))
    }

    @Test fun imageUriUsesItsImageHashEvenWhenTheClipAlsoOffersText() {
        val textHash=sha256("image caption".toByteArray());val imageHash="image bytes"
        val clip=ClipData("received",arrayOf("image/png"),ClipData.Item("image caption",null,android.net.Uri.parse("content://images/1"))).apply {
            description.extras=PersistableBundle().apply {
                putBoolean(ClipboardDelivery.OWN_CLIP,true);putString(ClipboardDelivery.OWN_HASH,imageHash);putString(ClipboardDelivery.OWN_SOURCE,a.deviceId)
            }
        }
        val latest=p.copy(recentClipboardSources=mapOf(textHash to a.deviceId,imageHash to b.deviceId))
        assertEquals(listOf(targets[0]),ClipboardSendPolicy.targets(clip,latest,targets))
        assertEquals(listOf(targets[1]),ClipboardSendPolicy.targets(latest,targets,ClipboardSendPolicy.clipboardProvenance(clip,latest,image=false)))
    }

    @Test fun legacyImageMarkerWithoutHashDoesNotExcludeAnOlderSourceBeforeStaging() {
        val clip=ClipData.newRawUri("legacy",android.net.Uri.parse("content://images/1")).apply {
            description.extras=PersistableBundle().apply {
                putBoolean(ClipboardDelivery.OWN_CLIP,true);putString(ClipboardDelivery.OWN_SOURCE,a.deviceId)
            }
        }
        val hash="same image";val latest=p.copy(recentClipboardSources=mapOf(hash to b.deviceId))
        val decisive=ClipboardSendPolicy.clipboardProvenance(clip,p)
        assertFalse(decisive.needsHash)
        assertTrue(ClipboardSendPolicy.targets(p,listOf(targets[0]),decisive).isEmpty())
        val deferred=ClipboardSendPolicy.clipboardProvenance(clip,latest)
        assertTrue(deferred.needsHash)
        val beforeStaging=ClipboardSendPolicy.targets(latest,targets,deferred)
        assertEquals(targets,beforeStaging)
        val resolved=ClipboardSendPolicy.provenance(latest,hash,owned=deferred.received,markedSource=deferred.source)
        assertEquals(listOf(targets[0]),ClipboardSendPolicy.targets(latest,beforeStaging,resolved))
    }

}
