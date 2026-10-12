package io.github.stantonmatt.pneu.features

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.media.MediaMetadataRetriever
import io.github.stantonmatt.pneu.data.Outgoing
import kotlinx.coroutines.CancellationException
import java.io.File

/** Recent never reopens an untrusted share URI. Missing/evicted private bytes
 * use the normal item icon; decode work also has a bounded worker/deadline. */
object MediaPreview {
    private fun decode(context:Context,row:Outgoing):Bitmap? {
        if(!row.mime.startsWith("image/") && !row.mime.startsWith("video/"))return null
        val file=File(row.path).canonicalFile
        val roots=listOf(File(context.filesDir,"outgoing").canonicalFile,File(context.cacheDir,"thumbnails").canonicalFile)
        if(file.parentFile !in roots || !file.isFile)return null
        if(row.mime.startsWith("video/") && file.parentFile==roots.first())return MediaMetadataRetriever().use {retriever->
            retriever.setDataSource(file.path);retriever.getScaledFrameAtTime(0,MediaMetadataRetriever.OPTION_CLOSEST_SYNC,96,96)
        }
        val bounds=BitmapFactory.Options().apply {inJustDecodeBounds=true};BitmapFactory.decodeFile(file.path,bounds)
        if(bounds.outWidth<=0 || bounds.outHeight<=0)return null
        var sample=1;while(maxOf(bounds.outWidth,bounds.outHeight)/sample>192)sample*=2
        return BitmapFactory.decodeFile(file.path,BitmapFactory.Options().apply {inSampleSize=sample})?.let {ImageOrientation.apply(file,it)}
    }
    suspend fun load(context:Context,row:Outgoing):Bitmap?=try {
        ProviderReads.preview(timeout=2000,discard={it?.recycle()}) {decode(context,row)}
    }catch(e:CancellationException) {throw e}catch(_:Exception) {null}
    suspend fun makeThumbnail(context:Context,row:Outgoing,target:File,beforeWrite:(File)->Unit={}):File?=try {
        // Open before launching the abandonable worker. Timeout/cancellation
        // closes this stream; a late decoder cannot recreate an unlinked path.
        target.outputStream().use {output->
            ProviderReads.preview {
                val bitmap=decode(context,row) ?: return@preview null
                try {
                    beforeWrite(target)
                    check(bitmap.compress(Bitmap.CompressFormat.JPEG,75,output))
                    if(target.length()>64*1024)null else target
                }finally {bitmap.recycle()}
            }
        }
    }catch(e:CancellationException) {throw e}catch(_:Exception) {null}
}
