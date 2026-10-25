package io.github.stantonmatt.pneu

import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.ReaderException
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.qrcode.QRCodeReader

internal fun decodePairQr(luminance: ByteArray, width: Int, height: Int, rowStride: Int): String? {
    // The Y plane has one byte per pixel; padding belongs to the source width.
    val source=PlanarYUVLuminanceSource(luminance,rowStride,height,0,0,width,height,false)
    val reader=QRCodeReader()
    for(candidate in listOf(source,source.invert())) {
        try {
            return reader.decode(BinaryBitmap(HybridBinarizer(candidate)),mapOf(DecodeHintType.TRY_HARDER to true)).text
        } catch(_: ReaderException) { }
    }
    return null
}
