package io.github.stantonmatt.pneu.features

import android.content.ContentUris
import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.Bundle
import android.os.Environment
import android.os.ParcelFileDescriptor
import android.os.StatFs
import android.provider.MediaStore
import io.github.stantonmatt.pneu.data.MediaCatalog
import java.io.FileOutputStream
import io.github.stantonmatt.pneu.protocol.utf8Prefix
import io.github.stantonmatt.pneu.protocol.randomId

internal interface ReceiveStorage {
    fun version():String=""
    fun checkSpace(bytes:Long,reserve:Long) {}
    fun create(metadata:ReceivedMetadata):Pair<String,String>
    fun open(uri:String):ReceiveOutput
    /** null = missing, true = pending, false = published. */
    fun pending(uri:String):Boolean?
    fun publish(uri:String)
    fun deletePending(uri:String)
    fun cleanOrphans(retained:Set<String>)
}
internal interface ReceiveOutput:AutoCloseable {
    fun write(bytes:ByteArray)
    fun sync()
}
/** Android 14+ lets an app insert/read/update its own MediaStore rows without
 * READ/WRITE storage grants. MediaProvider allocates collision-free filenames.
 * Never write _data, never request a broad storage permission. */
internal class ReceivedMediaStore(private val context:Context):ReceiveStorage {
    private val resolver=context.contentResolver
    override fun version()=MediaStore.getVersion(context,MediaCatalog.VOLUME)
    override fun checkSpace(bytes:Long,reserve:Long) {
        if(bytes>StatFs(Environment.getExternalStorageDirectory().absolutePath).availableBytes-reserve)throw ReceiveStorageException("disk_full")
    }
    private fun collection(mime:String)=when {
        mime.startsWith("image/",ignoreCase=true)->MediaStore.Images.Media.getContentUri(MediaCatalog.VOLUME)
        mime.startsWith("video/",ignoreCase=true)->MediaStore.Video.Media.getContentUri(MediaCatalog.VOLUME)
        else->MediaStore.Downloads.getContentUri(MediaCatalog.VOLUME)
    }
    private fun uniqueName(metadata:ReceivedMetadata):String {
        val original=ReceivePolicy.storageName(metadata.name)
        val extension=original.substringAfterLast('.',"").takeIf {it.length in 1..32}?.let {".$it"}.orEmpty()
        val stem=if(extension.isEmpty())original else original.dropLast(extension.length)
        fun exists(name:String):Boolean {
            val query=Bundle().apply {
                putInt(MediaStore.QUERY_ARG_MATCH_PENDING,MediaStore.MATCH_INCLUDE)
                putString(android.content.ContentResolver.QUERY_ARG_SQL_SELECTION,"relative_path=? AND _display_name=?")
                putStringArray(android.content.ContentResolver.QUERY_ARG_SQL_SELECTION_ARGS,arrayOf(ReceivePolicy.folder(metadata.mime),name))
            }
            return resolver.query(collection(metadata.mime),arrayOf("_id"),query,null)?.use {it.moveToFirst()}
                ?: throw ReceiveStorageException("storage_failed")
        }
        if(!exists(original))return original
        // Keep collision probing bounded, including against pending transfers
        // from other peers. Admission serializes this allocation in the app.
        for(number in 1..20) {
            val suffix=" ($number)$extension"
            val candidate=utf8Prefix(stem,255-suffix.toByteArray().size)+suffix
            if(!exists(candidate))return candidate
        }
        val suffix="-${randomId()}$extension"
        return utf8Prefix(stem,255-suffix.toByteArray().size)+suffix
    }
    // Resume publication copies run outside the policy fence. Keep allocation
    // atomic across peers: two pending inserts must never alias one URI.
    @Synchronized override fun create(metadata:ReceivedMetadata):Pair<String,String> {
        if(metadata.size>StatFs(Environment.getExternalStorageDirectory().absolutePath).availableBytes)throw ReceiveStorageException("disk_full")
        val values=ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME,uniqueName(metadata))
            put(MediaStore.MediaColumns.MIME_TYPE,metadata.mime.lowercase(java.util.Locale.ROOT))
            put(MediaStore.MediaColumns.RELATIVE_PATH,ReceivePolicy.folder(metadata.mime))
            put(MediaStore.MediaColumns.IS_PENDING,1)
        }
        val uri=resolver.insert(collection(metadata.mime),values) ?: throw ReceiveStorageException("storage_failed")
        try {
            val actual=resolver.query(uri,arrayOf(MediaStore.MediaColumns.DISPLAY_NAME),null,null,null)?.use {
                if(it.moveToFirst())it.getString(0) else null
            } ?: throw ReceiveStorageException("storage_failed")
            return uri.toString() to actual
        }catch(e:Exception) {runCatching {resolver.delete(uri,null,null)};throw e}
    }
    override fun open(uri:String):ReceiveOutput {
        val descriptor=resolver.openFileDescriptor(Uri.parse(uri),"w") ?: throw ReceiveStorageException("not_writable")
        return object:ReceiveOutput {
            private val output:FileOutputStream=ParcelFileDescriptor.AutoCloseOutputStream(descriptor)
            override fun write(bytes:ByteArray) {output.write(bytes)}
            override fun sync() {output.flush();output.fd.sync()}
            override fun close()=output.close()
        }
    }
    override fun pending(uri:String):Boolean?=resolver.query(Uri.parse(uri),arrayOf(MediaStore.MediaColumns.IS_PENDING,MediaStore.MediaColumns.OWNER_PACKAGE_NAME),null,null,null)?.use {
        if(it.moveToFirst() && it.getString(1)==context.packageName)it.getInt(0)!=0 else null
    }
    override fun publish(uri:String) {
        if(resolver.update(Uri.parse(uri),ContentValues().apply {put(MediaStore.MediaColumns.IS_PENDING,0)},null,null)!=1)
            throw ReceiveStorageException("storage_failed")
    }
    override fun deletePending(uri:String) {
        if(uri.isNotEmpty() && pending(uri)==true)resolver.delete(Uri.parse(uri),null,null)
    }
    override fun cleanOrphans(retained:Set<String>) {
        // Covers a crash after insert but before the URI journal update. Restrict
        // by owner AND exact receive paths; never sweep camera/user files.
        for(mime in listOf("image/png","video/mp4","application/octet-stream")) {
            val base=collection(mime)
            val args=Bundle().apply {
                putInt(MediaStore.QUERY_ARG_MATCH_PENDING,MediaStore.MATCH_ONLY)
                putString(android.content.ContentResolver.QUERY_ARG_SQL_SELECTION,
                    "owner_package_name=? AND relative_path IN (?,?,?)")
                putStringArray(android.content.ContentResolver.QUERY_ARG_SQL_SELECTION_ARGS,
                    arrayOf(context.packageName,"Pictures/Pneu/","Movies/Pneu/","Download/Pneu/"))
            }
            resolver.query(base,arrayOf(MediaStore.MediaColumns._ID),args,null)?.use {cursor->
                while(cursor.moveToNext()) {
                    val uri=ContentUris.withAppendedId(base,cursor.getLong(0)).toString()
                    if(uri !in retained)deletePending(uri)
                }
            }
        }
    }
}
