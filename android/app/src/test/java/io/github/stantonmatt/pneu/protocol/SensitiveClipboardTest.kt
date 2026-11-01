package io.github.stantonmatt.pneu.protocol

import android.app.Application
import android.content.ClipDescription
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.features.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class SensitiveClipboardTest {
    private fun roundtrip(type:Type,body:com.google.gson.JsonObject):com.google.gson.JsonObject {
        val bytes=ByteArrayOutputStream();Protocol.write(bytes,Frame.Control(type,body))
        return (Protocol.read(ByteArrayInputStream(bytes.toByteArray())) as Frame.Control).body
    }
    @Test fun textAndClipboardImageSerializersOmitFalseAndRoundtripTrueAndOldMessages() {
        val row=Outgoing().apply {mediaId="manual:test";transferId="a".repeat(32);text="example";createdAt=1;kind="clipboard_image";mime="image/png";fileName="image.png";hash="b".repeat(64);size=24}
        for(sensitive in listOf(false,true)) {
            row.sensitive=sensitive
            val text=roundtrip(Type.CLIPBOARD,Staging.clipboardMetadata(row))
            val image=roundtrip(Type.FILE_BEGIN,Staging.metadata(row))
            for(body in listOf(text,image)) {
                assertEquals(sensitive,body.has("sensitive"))
                assertEquals(sensitive,body["sensitive"]?.asBoolean==true)
            }
            val received=ReceivedMetadata.parse(image);assertEquals(sensitive,received.sensitive)
            assertEquals(sensitive,com.google.gson.JsonParser.parseString(received.canonical).asJsonObject.has("sensitive"))
        }
        row.kind="file";assertFalse(Staging.metadata(row).has("sensitive"))
    }
    @Test fun explicitFalseParsesAndNonBooleanSensitiveIsRejected() {
        val text=json("transfer_id" to "a".repeat(32),"text" to "example","created_at" to 1,"sensitive" to false)
        assertFalse(roundtrip(Type.CLIPBOARD,text)["sensitive"].asBoolean)
        text.addProperty("sensitive","true")
        assertTrue(runCatching {Protocol.validate(Type.CLIPBOARD,text)}.isFailure)
    }
    @Test fun receivedClipSetsSensitiveAndroidFlagAndRetainsExistingOwnership() {
        val normal=ClipboardDelivery.receivedClip("Pneu","normal","hash","PC")
        assertFalse(normal.description.extras!!.containsKey(ClipDescription.EXTRA_IS_SENSITIVE))
        val secret=ClipboardDelivery.receivedClip("Pneu","secret","hash","PC",sensitive=true)
        assertTrue(secret.description.extras!!.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE))
        assertTrue(secret.description.extras!!.getBoolean(ClipboardDelivery.OWN_CLIP))
        assertEquals("PC",secret.description.extras!!.getString(ClipboardDelivery.OWN_SOURCE))
    }
}
