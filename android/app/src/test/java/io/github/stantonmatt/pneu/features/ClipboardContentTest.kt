package io.github.stantonmatt.pneu.features

import android.app.Application
import android.content.ClipData
import android.net.Uri
import io.github.stantonmatt.pneu.protocol.sha256
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ClipboardContentTest {
    @Test fun currentTextUsesExactContentRatherThanOwnershipOrHistory() {
        val text="café\r\ntext "
        val clip=ClipboardDelivery.receivedClip("Pneu",text,"untrusted-marker","source")
        assertTrue(ClipboardContent.textMatches(clip,text))
        assertFalse(ClipboardContent.textMatches(clip,"café\ntext "))
        assertFalse(ClipboardContent.textMatches(clip,text.trim()))
        assertFalse(ClipboardContent.textMatches(ClipData.newPlainText("local","intervening copy"),text))
        assertFalse(ClipboardContent.textMatches(null,text))
        assertTrue(ClipboardContent.textMatches(ClipData.newPlainText("other app",text),text))
    }
    @Test fun imageComparisonRequiresThePreparedBytesAndTheStillCurrentRepresentation() {
        val uri=Uri.parse("content://images/1");val hash=sha256(byteArrayOf(1,2,3))
        val snapshot=ClipboardContent.ImageSnapshot(uri,hash)
        val clip=ClipData("image",arrayOf("image/png"),ClipData.Item(uri))
        assertTrue(ClipboardContent.imageMatches(clip,hash,snapshot))
        assertFalse(ClipboardContent.imageMatches(clip,sha256(byteArrayOf(1,2,4)),snapshot))
        assertFalse(ClipboardContent.imageMatches(ClipData.newRawUri("changed",Uri.parse("content://images/2")),hash,snapshot))
        assertFalse(ClipboardContent.imageMatches(ClipData.newPlainText("local","changed"),hash,snapshot))
        assertFalse(ClipboardContent.imageMatches(clip,hash,null))
        // Like the PC Formats representation, matching text is sufficient even
        // when an image representation is also present on the current offer.
        val formats=ClipData("formats",arrayOf("text/plain","image/png"),ClipData.Item("same bytes",null,uri))
        assertTrue(ClipboardContent.imageMatches(formats,sha256("same bytes".toByteArray()),null))
    }
}
