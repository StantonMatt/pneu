package io.github.stantonmatt.pneu.data

import android.content.ContentResolver
import android.content.Context
import android.os.Bundle
import android.provider.MediaStore
import io.github.stantonmatt.pneu.granted

data class CatalogBoundary(val version: String,val generation: Long,val imageId: Long,val videoId: Long)
data class CatalogSnapshot(val version:String,val generation:Long)
object MediaCatalog {
    const val VOLUME="external_primary"
    fun fullAccess(context:Context,video:Boolean)=context.granted(if(video)android.Manifest.permission.READ_MEDIA_VIDEO else android.Manifest.permission.READ_MEDIA_IMAGES)
    suspend fun version(context:Context)=io.github.stantonmatt.pneu.features.ProviderReads.call {MediaStore.getVersion(context,VOLUME)}
    suspend fun snapshot(context:Context)=io.github.stantonmatt.pneu.features.ProviderReads.call {
        val version=MediaStore.getVersion(context,VOLUME);val generation=MediaStore.getGeneration(context,VOLUME)
        if(MediaStore.getVersion(context,VOLUME)!=version)throw java.io.IOException("Catalog changed")
        CatalogSnapshot(version,generation)
    }
    suspend fun latest(context: Context,video: Boolean): Long {
        val collection=if(video)MediaStore.Video.Media.getContentUri(VOLUME) else MediaStore.Images.Media.getContentUri(VOLUME)
        val query=Bundle().apply {
            putString(ContentResolver.QUERY_ARG_SQL_SORT_ORDER,"_id DESC")
            putInt(ContentResolver.QUERY_ARG_LIMIT,1)
            putInt(MediaStore.QUERY_ARG_MATCH_PENDING,MediaStore.MATCH_INCLUDE)
        }
        return io.github.stantonmatt.pneu.features.ProviderReads.call {signal->
            val cursor=context.contentResolver.query(collection,arrayOf("_id"),query,signal) ?: throw java.io.IOException("Catalog unavailable")
            cursor.use {if(it.moveToFirst())it.getLong(0) else 0}
        }
    }
    suspend fun boundary(context:Context):CatalogBoundary {
        val snapshot=snapshot(context)
        val image=if(fullAccess(context,false))latest(context,false) else 0
        val video=if(fullAccess(context,true))latest(context,true) else 0
        if(version(context)!=snapshot.version)throw java.io.IOException("Catalog changed")
        return CatalogBoundary(snapshot.version,snapshot.generation,image,video)
    }
}
