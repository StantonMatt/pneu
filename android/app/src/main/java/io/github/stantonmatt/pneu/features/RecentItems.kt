package io.github.stantonmatt.pneu.features

import android.content.Context
import android.content.ContentUris
import android.content.Intent
import android.net.Uri
import android.provider.MediaStore
import io.github.stantonmatt.pneu.data.MediaCatalog
import io.github.stantonmatt.pneu.data.Outgoing
import kotlinx.coroutines.CancellationException

object RecentItems {
    suspend fun openOriginal(context:Context,row:Outgoing,open:(Intent)->Unit={context.startActivity(it)}) {
        if(row.state!="sent" || !row.automatic)return
        val uri=Uri.parse(row.uri)
        if(uri.scheme!="content" || uri.authority!="media")return
        try {
            val mime=ProviderReads.call {signal->
                val version=MediaStore.getVersion(context,MediaCatalog.VOLUME)
                val id=ContentUris.parseId(uri)
                val collection=when(uri) {
                    ContentUris.withAppendedId(MediaStore.Images.Media.getContentUri(MediaCatalog.VOLUME),id)->"images"
                    ContentUris.withAppendedId(MediaStore.Video.Media.getContentUri(MediaCatalog.VOLUME),id)->"video"
                    else->return@call null
                }
                if(row.mediaId!="${MediaCatalog.VOLUME}:$version:$collection:$id")return@call null
                val type=context.contentResolver.query(uri,arrayOf(MediaStore.MediaColumns.MIME_TYPE),null,null,null,signal)?.use {
                    if(it.moveToFirst())it.getString(0) else null
                }
                if(MediaStore.getVersion(context,MediaCatalog.VOLUME)==version)type?.takeIf(String::isNotBlank) else null
            }
            if(mime!=null)open(Intent(Intent.ACTION_VIEW).setDataAndType(uri,mime).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
        }catch(e:CancellationException) {throw e}catch(_:Exception) { /* Missing/inaccessible originals do nothing. */ }
    }
}
