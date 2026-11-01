package io.github.stantonmatt.pneu.features

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import io.github.stantonmatt.pneu.data.SavedClips

/** Reject known oversized images without opening their payload. Unknown sizes
 * and changing provider contents are bounded by the staging copy instead. */
internal object ClipboardImageSize {
    fun requireAllowed(size:Long) {if(size>SavedClips.MAX_IMAGE)throw ShareFailure("image_too_large")}
    fun readLimit(count:Long,requested:Int):Int = minOf(requested.toLong(),SavedClips.MAX_IMAGE+1L-count).toInt()
    suspend fun check(context:Context,uri:Uri) {
        val size=ProviderReads.call {signal->
            context.contentResolver.query(uri,arrayOf(OpenableColumns.SIZE),null,null,null,signal)?.use {cursor->
                val column=cursor.getColumnIndex(OpenableColumns.SIZE)
                if(cursor.moveToFirst() && column>=0 && !cursor.isNull(column))cursor.getLong(column) else null
            }
        }
        size?.let(::requireAllowed)
    }
}
