package io.github.stantonmatt.pneu.features

import android.graphics.Bitmap
import android.graphics.Matrix
import androidx.exifinterface.media.ExifInterface
import java.io.File

/** Takes ownership of decoded pixels. The returned bitmap is in display
 * orientation; replaced pixels (also on a failed transform) are recycled here. */
internal object ImageOrientation {
    fun apply(file:File,decoded:Bitmap):Bitmap = try {
        val orientation=runCatching {
            ExifInterface(file.path).getAttributeInt(ExifInterface.TAG_ORIENTATION,ExifInterface.ORIENTATION_NORMAL)
        }.getOrDefault(ExifInterface.ORIENTATION_NORMAL)
        if(orientation !in 2..8)decoded else
            Bitmap.createBitmap(decoded,0,0,decoded.width,decoded.height,
                Matrix().apply {setValues(ImagePolicy.matrix(orientation))},true).also {
                if(it!==decoded)decoded.recycle()
            }
    }catch(e:Throwable) {decoded.recycle();throw e}
}
