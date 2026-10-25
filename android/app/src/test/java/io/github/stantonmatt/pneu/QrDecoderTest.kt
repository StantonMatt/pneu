package io.github.stantonmatt.pneu

import com.google.zxing.BarcodeFormat
import com.google.zxing.qrcode.QRCodeWriter
import org.junit.Assert.*
import org.junit.Test

class QrDecoderTest {
    private val invitation="pneu://pair?v=1&name=Test%20Computer&fp=${"ab".repeat(32)}&addr=192.168.1.20%3A47839&token=${"cd".repeat(32)}&exp=1793000000000"

    private fun frame(rowPadding: Int=0, inverted: Boolean=false, rotated: Boolean=false): ByteArray {
        val qr=QRCodeWriter().encode(invitation,BarcodeFormat.QR_CODE,400,400)
        // Camera buffers need not include the padding after the final row.
        val stride=400+rowPadding
        return ByteArray(stride*399+400) { index ->
            val x=index%stride; val y=index/stride
            val dark=x<400 && if(rotated)qr[y,399-x] else qr[x,y]
            if(dark xor inverted)0 else 255.toByte()
        }
    }

    @Test fun decodesPairingInvitation() {
        assertEquals(invitation,decodePairQr(frame(),400,400,400))
    }
    @Test fun decodesPaddedRowsWithoutTrailingPadding() {
        assertEquals(invitation,decodePairQr(frame(rowPadding=37),400,400,437))
    }
    @Test fun decodesInvertedPaddedRows() {
        assertEquals(invitation,decodePairQr(frame(rowPadding=37,inverted=true),400,400,437))
    }
    @Test fun decodesRotatedPairingInvitation() {
        assertEquals(invitation,decodePairQr(frame(rotated=true),400,400,400))
    }
    @Test fun blankFrameHasNoCode() {
        for(value in listOf(0.toByte(),255.toByte()))assertNull(decodePairQr(ByteArray(437*399+400) {value},400,400,437))
    }
}
