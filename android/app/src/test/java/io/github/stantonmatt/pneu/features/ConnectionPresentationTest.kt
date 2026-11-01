package io.github.stantonmatt.pneu.features

import android.app.Application
import android.content.ClipData
import android.content.ClipDescription
import android.os.PersistableBundle
import android.net.Uri
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.data.*
import org.junit.Test
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ConnectionPresentationTest {
    private val a=Peer("Desktop","a".repeat(64),"a".repeat(32),emptyList())
    private val b=Peer("MacBook","b".repeat(64),"b".repeat(32),emptyList())
    private val prefs=Prefs(deviceId="phone",peers=listOf(a,b))
    @Test fun typedStatusAndPairingFeedbackAreMappedOutsideUi() {
        for(status in ConnectionStatus.entries)assertEquals(status,LinkState(status.key).connectionStatus)
        assertEquals(PairingIssue.UNREACHABLE,UiFeedback("Can't reach your PC").issue)
        assertEquals(PairingIssue.CLOCK,UiFeedback("Your PC's clock says this code expired").issue)
        assertEquals(PairingIssue.EXPIRED,UiFeedback(PairingIssue.EXPIRED_MESSAGE).issue)
        assertEquals(PairingIssue.FEEDBACK,UiFeedback("That isn't a Pneu code").issue)
        val context=RuntimeEnvironment.getApplication()
        assertEquals("This code expired. Scan the new one.",UiPresentation.pairing(context,UiFeedback(PairingIssue.EXPIRED_MESSAGE)))
        assertEquals("Update required",UiPresentation.status(context,ConnectionStatus.UPDATE_REQUIRED))
        assertEquals("Unreachable",UiPresentation.status(context,ConnectionStatus.UNREACHABLE))
    }
    @Test fun incompatibleHelloAndExplicitErrorPropagateWithoutUnpairing() {
        assertEquals(ConnectionStatus.UPDATE_REQUIRED,ConnectionStatus.hello(2,setOf("files")))
        assertEquals(ConnectionStatus.UPDATE_REQUIRED,ConnectionStatus.hello(1,setOf("future_feature")))
        assertNull(ConnectionStatus.hello(1,setOf("files"))) // v1 without mesh remains compatible.
        assertEquals(ConnectionStatus.UPDATE_REQUIRED,ConnectionStatus.remoteError("version_mismatch"))
        val state=LinkState(ConnectionStatus.UPDATE_REQUIRED.key)
        val projected=DeviceProjection.states(prefs,mapOf(b.deviceId to state))
        assertEquals(ConnectionStatus.UPDATE_REQUIRED,projected.single {it.peer.deviceId==b.deviceId}.connection.connectionStatus)
        assertFalse(projected.any {it.peer.pairingLost});assertEquals(2,prefs.devices.size)
        assertEquals(ConnectionStatus.UPDATE_REQUIRED,LinkProjection.aggregate(prefs,mapOf(b.pin to state),null).connectionStatus)
    }
    @Test fun typedPairingFailureSurvivesWorkerRemovalAndOtherComputerUpdates() {
        val context=RuntimeEnvironment.getApplication()
        for(status in listOf(ConnectionStatus.UPDATE_REQUIRED,ConnectionStatus.FINGERPRINT_MISMATCH,ConnectionStatus.UNREACHABLE)) {
            val failure=LinkState.pairingFailure(status)
            val first=LinkProjection.aggregate(Prefs(),emptyMap(),failure)
            assertEquals(status,first.connectionStatus)
            assertEquals(status,first.pairingFeedback!!.status)
            assertEquals(UiPresentation.status(context,status),UiPresentation.pairing(context,first.pairingFeedback!!))
            val other=LinkProjection.aggregate(prefs,mapOf(a.pin to LinkState("Connected")),failure)
            assertEquals(ConnectionStatus.CONNECTED,other.connectionStatus)
            assertEquals(status,other.pairingFeedback!!.status)
            assertEquals(failure.pairingFeedback!!.id,other.pairingFeedback!!.id)
        }
        assertEquals(PairingIssue.UNREACHABLE,LinkState.pairingFailure(ConnectionStatus.UNREACHABLE).pairingFeedback!!.issue)
    }
    @Test fun receivingSurvivesAggregationAndRevocationClearsIt() {
        val incoming=IncomingTransfer(b.deviceId,"transfer","report.pdf",20,100)
        val states=mapOf(a.pin to LinkState("Connected"),b.pin to LinkState("Connected",incoming=incoming))
        assertEquals(incoming,LinkProjection.aggregate(prefs,states,null).incoming)
        assertNull(LinkProjection.aggregate(prefs.copy(resetPeers=setOf(b.deviceId)),states,null).incoming)
    }
    @Test fun displayedAddressMatchesRouteWithMissingOrReorderedCategories() {
        val lan=io.github.stantonmatt.pneu.protocol.Endpoint("192.168.1.20",47821)
        val vpn=io.github.stantonmatt.pneu.protocol.Endpoint("100.67.144.20",47821)
        for(addresses in listOf(listOf(lan,vpn),listOf(vpn,lan))) {
            assertEquals(lan.host,UiPresentation.pairedAddress(addresses,"Wi-Fi"))
            assertEquals(vpn.host,UiPresentation.pairedAddress(addresses,"Tailscale"))
        }
        assertEquals(vpn.host,UiPresentation.pairedAddress(listOf(vpn),"Tailscale"))
        assertNull(UiPresentation.pairedAddress(listOf(vpn),"Wi-Fi"))
        assertNull(UiPresentation.pairedAddress(listOf(lan),"Tailscale"))
        assertNull(UiPresentation.pairedAddress(listOf(io.github.stantonmatt.pneu.protocol.Endpoint("::1",47821)),"Wi-Fi"))
    }
    @Test fun clipboardNeedsContentAndAnEligibleConnectedRecipient() {
        val clip=ClipData.newPlainText("note","Hello")
        val links=mapOf(b.deviceId to LinkState("Connected",capabilities=setOf("clipboard_text")))
        assertTrue(ClipboardSendPolicy.available(clip,prefs,links))
        assertFalse(ClipboardSendPolicy.available(null,prefs,links))
        assertFalse(ClipboardSendPolicy.available(ClipData.newPlainText("empty",""),prefs,links))
        assertFalse(ClipboardSendPolicy.available(clip,prefs,emptyMap()))
        assertFalse(ClipboardSendPolicy.available(clip,prefs.copy(peers=listOf(a,b.copy(clipboardSend=false))),links))
        clip.description.extras=PersistableBundle().apply {putBoolean(ClipDescription.EXTRA_IS_SENSITIVE,true)}
        assertTrue(ClipboardSendPolicy.available(clip,prefs,links))
    }
    @Test fun providerMimeRatherThanDescriptionSelectsTheEligibleClipboardRepresentation() {
        val imageLinks=mapOf(b.deviceId to LinkState("Connected",capabilities=setOf("files","clipboard_image")))
        val textLinks=mapOf(b.deviceId to LinkState("Connected",capabilities=setOf("clipboard_text")))
        val raw=ClipData.newRawUri("image",Uri.parse("content://fixture/image"))
        assertTrue(raw.description.hasMimeType("text/uri-list"))
        assertTrue(ClipboardSendPolicy.available(raw,prefs,imageLinks,"image/png"))
        assertFalse(ClipboardSendPolicy.available(raw,prefs,textLinks,"image/png"))
        assertFalse(ClipboardSendPolicy.available(raw,prefs,imageLinks,"application/pdf"))
        assertFalse(ClipboardSendPolicy.available(raw,prefs,imageLinks,null))
        for(caps in listOf(setOf("files"),setOf("clipboard_image"))) {
            assertFalse(ClipboardSendPolicy.available(raw,prefs,mapOf(b.deviceId to LinkState("Connected",capabilities=caps)),"image/png"))
        }
        // An advertised image whose provider resolves to another type follows
        // the same text fallback as the send action.
        val fallback=ClipData("fallback",arrayOf("image/png"),ClipData.Item("Text",null,Uri.parse("content://fixture/file")))
        assertTrue(ClipboardSendPolicy.available(fallback,prefs,textLinks,"application/pdf"))
        assertFalse(ClipboardSendPolicy.available(fallback,prefs,imageLinks,"application/pdf"))
        assertFalse(ClipboardSendPolicy.available(raw,prefs.copy(resetPeers=setOf(b.deviceId)),imageLinks,"image/png"))
        raw.description.extras=PersistableBundle().apply {putBoolean(ClipDescription.EXTRA_IS_SENSITIVE,true)}
        assertTrue(ClipboardSendPolicy.available(raw,prefs,imageLinks,"image/png"))
    }
}
