package io.github.stantonmatt.pneu.data

import android.content.ClipData
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.core.content.FileProvider
import androidx.room.Room
import io.github.stantonmatt.pneu.protocol.sha256
import io.github.stantonmatt.pneu.features.ImageOrientation
import io.github.stantonmatt.pneu.features.ImageWorkBudget
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.File
import java.io.InputStream
import java.security.MessageDigest

internal data class SavedClipsState(val enabled:Boolean=false,val items:List<SavedClip> = emptyList(),val loaded:Boolean=false,val arrivals:Map<String,Long> = emptyMap())

/** All admission, setting changes and file ownership share one lock. No clipboard
 * reads: callers supply only a sent/received payload or an explicitly chosen ID. */
internal class SavedClips(private val context:Context,scope:CoroutineScope,
    internal val database:SavedClipsDatabase=Room.databaseBuilder(context,SavedClipsDatabase::class.java,"clipboard-history.db").addMigrations(SavedClipsDatabase.MIGRATION_1_2).build(),
    private val now:()->Long=System::currentTimeMillis) {
    private val dao=database.clips()
    private val mutex=Mutex()
    private val directory=File(context.filesDir,"clipboard-history/images")
    private val clipboardDirectory=File(context.filesDir,"clipboard-snapshot")
    private val current=MutableStateFlow(SavedClipsState())
    val state=current.asStateFlow()
    private val ready=scope.async {withContext(Dispatchers.IO) {mutex.withLock {reconcile()}}}

    private var arrivalRevision=0L
    private fun publish(arrived:String?=null) {
        val items=dao.all()
        if(arrived!=null)arrivalRevision++
        val arrivals=items.associate {it.id to if(it.id==arrived)arrivalRevision else current.value.arrivals.getOrDefault(it.id,0L)}
        current.value=SavedClipsState(dao.setting()?.enabled==true,items,true,arrivals)
    }
    private fun file(name:String)=File(directory,name).also {require(name.isNotEmpty() && it.name==name)}
    private fun removeFile(file:File) {check(file.delete() || !file.exists()) {"History file could not be removed"}}
    private fun removeFiles(item:SavedClip) {if(item.image.isNotEmpty())removeFile(file(item.image));if(item.thumbnail.isNotEmpty())removeFile(file(item.thumbnail))}
    private fun reconcile() {
        directory.mkdirs()
        if(dao.setting()?.enabled!=true)dao.clear()
        dao.all().filter {it.image.isNotEmpty() && (!file(it.image).isFile || !file(it.thumbnail).isFile)}.forEach {dao.delete(it.id)}
        val retained=dao.all().flatMap {listOf(it.image,it.thumbnail)}.toSet()
        directory.listFiles()?.filter {it.name !in retained}?.forEach(::removeFile)
        publish()
    }
    private suspend fun <T> locked(body:()->T):T {ready.await();return withContext(Dispatchers.IO) {mutex.withLock {body()}}}
    suspend fun setEnabled(enabled:Boolean)=locked {
        if(enabled==current.value.enabled)return@locked
        // Persist the wipe and setting together; cancellation cannot strand files.
        val old=dao.all()
        database.runInTransaction {old.forEach(::removeFiles);dao.clear();dao.setting(SavedClipSetting().apply {this.enabled=enabled})}
        directory.listFiles()?.forEach(::removeFile)
        publish()
    }
    suspend fun clear()=locked {
        val old=dao.all().filterNot {it.pinned}
        database.runInTransaction {old.forEach {removeFiles(it);dao.delete(it.id)}}
        publish()
    }
    /** False only when a new pin would exceed the limit. */
    suspend fun setPinned(id:String,pinned:Boolean):Boolean=locked {
        val items=dao.all();val item=items.find {it.id==id} ?: return@locked true
        if(item.pinned==pinned)return@locked true
        if(pinned && items.count {it.pinned}>=PIN_CAPACITY)return@locked false
        item.pinned=pinned
        item.pinPosition=if(pinned)(items.maxOfOrNull {it.pinPosition} ?: 0)+1 else 0
        database.runInTransaction {dao.put(item);evict(except=id)}
        sweepImages();publish();true
    }
    private fun evict(except:String?=null) {
        val recent=dao.all().filterNot {it.pinned}
        recent.asReversed().filter {it.id!=except}.take((recent.size-CAPACITY).coerceAtLeast(0)).forEach {dao.delete(it.id)}
    }
    private fun sweepImages() {
        val retained=dao.all().flatMap {listOf(it.image,it.thumbnail)}.toSet()
        directory.listFiles()?.filter {it.name !in retained}?.forEach(::removeFile)
    }
    suspend fun reattribute(old:String,fresh:String,name:String)=locked {
        database.runInTransaction {dao.reattribute(old,fresh,name)};publish()
    }
    suspend fun delete(id:String)=locked {val item=dao.all().find {it.id==id} ?: return@locked;database.runInTransaction {removeFiles(item);dao.delete(id)};publish()}
    private fun save(item:SavedClip,arrived:Boolean=true) {
        dao.all().find {it.id==item.id}?.let {item.pinned=it.pinned;item.pinPosition=it.pinPosition}
        item.copiedAt=now();item.position=(dao.all().maxOfOrNull {it.position} ?: 0)+1
        database.runInTransaction {
            dao.put(item)
            evict()
        }
        val retained=dao.all().flatMap {listOf(it.image,it.thumbnail)}.toSet()
        directory.listFiles()?.filter {it.name !in retained}?.forEach(::removeFile)
        publish(if(arrived)item.id else null)
    }
    suspend fun recordText(text:String,sender:String="",sensitive:Boolean=false,source:String="")=locked {
        if(!current.value.enabled || sensitive || text.isBlank() || text.toByteArray().size>MAX_TEXT)return@locked
        save(SavedClip().apply {id="text:"+sha256(text.toByteArray());this.text=text;this.sender=sender;this.source=source})
    }
    /** The bounded source is staged or app-owned received data, never the system clipboard. */
    suspend fun recordImage(mime:String,sender:String="",sensitive:Boolean=false,source:String="",open:()->InputStream?)=locked {
        if(!current.value.enabled || sensitive)return@locked
        val bytes=open()?.use {it.readNBytes(MAX_IMAGE+1)} ?: return@locked
        if(bytes.isEmpty() || bytes.size>MAX_IMAGE)return@locked
        ImageWorkBudget.run {
            val bounds=BitmapFactory.Options().apply {inJustDecodeBounds=true}
            BitmapFactory.decodeByteArray(bytes,0,bytes.size,bounds)
            if(bounds.outWidth<=0 || bounds.outHeight<=0)return@run
            var sample=1
            while(maxOf(bounds.outWidth,bounds.outHeight)/sample>128)sample*=2
            val bitmap=ImageOrientation.apply(bytes,BitmapFactory.decodeByteArray(bytes,0,bytes.size,BitmapFactory.Options().apply {inSampleSize=sample}) ?: return@run)
            try {
                // Hash the bounded encoded payload; dedupe never needs full-size pixels.
                val hash=sha256(bytes)
                val extension=android.webkit.MimeTypeMap.getSingleton().getExtensionFromMimeType(mime) ?: "png"
                val image="$hash.$extension";val thumb="$hash.thumb.png"
                val item=SavedClip().apply {id="image:$hash";this.image=image;thumbnail=thumb;this.mime=mime;this.sender=sender;this.source=source}
                // Any failure after the first write sweeps files no row references.
                try {
                    val temporary=File.createTempFile("history-",".tmp",directory)
                    try {
                        temporary.outputStream().use {it.write(bytes);it.fd.sync()}
                        check(temporary.renameTo(file(image)))
                    } finally {temporary.delete()}
                    val scale=minOf(1f,128f/maxOf(bitmap.width,bitmap.height))
                    val small=Bitmap.createScaledBitmap(bitmap,maxOf(1,(bitmap.width*scale).toInt()),maxOf(1,(bitmap.height*scale).toInt()),true)
                    try {file(thumb).outputStream().use {check(small.compress(Bitmap.CompressFormat.PNG,100,it))}}
                    finally {if(small!==bitmap)small.recycle()}
                    save(item)
                } catch(e:Throwable) {reconcile();throw e}
            } finally {bitmap.recycle()}
        }
    }
    /** Updates storage order; the screen separately retains its displayed order. */
    suspend fun copy(id:String,apply:suspend (ClipData)->Unit):Boolean {
        ready.await()
        return withContext(Dispatchers.IO) {mutex.withLock {
            if(!current.value.enabled)return@withLock false
            val item=dao.all().find {it.id==id} ?: return@withLock false
            val clip=if(item.image.isEmpty())ClipData.newPlainText(context.getString(io.github.stantonmatt.pneu.R.string.app_name),item.text) else {
                if(!file(item.image).isFile) {database.runInTransaction {removeFiles(item);dao.delete(id)};publish();return@withLock false}
                // Clipboard ownership is independent of history retention. Only
                // another history image copy replaces this durable single slot.
                clipboardDirectory.mkdirs()
                val snapshot=File(clipboardDirectory,"image.${file(item.image).extension}")
                val temporary=File.createTempFile("clipboard-",".tmp",clipboardDirectory)
                try {
                    temporary.outputStream().use {output->file(item.image).inputStream().use {it.copyTo(output)};output.fd.sync()}
                    check(temporary.renameTo(snapshot))
                } finally {temporary.delete()}
                clipboardDirectory.listFiles()?.filter {it!=snapshot}?.forEach(::removeFile)
                ClipData(context.getString(io.github.stantonmatt.pneu.R.string.app_name),arrayOf(item.mime),ClipData.Item(FileProvider.getUriForFile(context,"${context.packageName}.history",snapshot)))
            }
            if(item.source.isNotEmpty()) {
                val hash=if(item.image.isEmpty())sha256(item.text.toByteArray()) else {
                    val digest=MessageDigest.getInstance("SHA-256")
                    file(item.image).inputStream().use {input->val buffer=ByteArray(8192);while(true) {val n=input.read(buffer);if(n<0)break;digest.update(buffer,0,n)}}
                    digest.digest().joinToString("") {"%02x".format(it)}
                }
                clip.description.extras=android.os.PersistableBundle().apply {
                    putBoolean(io.github.stantonmatt.pneu.features.ClipboardDelivery.OWN_CLIP,true)
                    putString(io.github.stantonmatt.pneu.features.ClipboardDelivery.OWN_SOURCE,item.source)
                    putString(io.github.stantonmatt.pneu.features.ClipboardDelivery.OWN_HASH,hash)
                }
            }
            withContext(NonCancellable) {apply(clip);save(item,arrived=false);true}
        }}
    }
    suspend fun ownsImage(uri:android.net.Uri):Boolean=locked {
        clipboardDirectory.listFiles()?.any {it.isFile &&
            FileProvider.getUriForFile(context,"${context.packageName}.history",it)==uri}==true
    }
    fun thumbnail(item:SavedClip)=file(item.thumbnail)
    companion object {const val CAPACITY=50;const val PIN_CAPACITY=25;const val MAX_TEXT=1024*1024;const val MAX_IMAGE=20*1024*1024}
}
