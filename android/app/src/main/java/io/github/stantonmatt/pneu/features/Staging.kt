package io.github.stantonmatt.pneu.features

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.data.DeliveryBinding
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.File
import java.io.FileOutputStream
import java.security.MessageDigest

class LocalCapacityException : java.io.IOException()
class Staging(private val app: AppGraph, private val deleteCommittedSource:(File)->Unit={it.delete()}, private val sourceTimeout:Long=10000) {
    private val mutex=Mutex()
    private val sources=java.util.concurrent.ConcurrentHashMap.newKeySet<android.content.res.AssetFileDescriptor>()
    private val activeParts=java.util.concurrent.ConcurrentHashMap.newKeySet<File>()
    private val thumbnails=Any()
    private val activeThumbnails=mutableSetOf<File>()
    internal var beforeThumbnailWrite:(File)->Unit={}
    // Both sweeps use this short monitor; creation/registration is atomic with
    // deletion, without holding staging/queue locks across decoding or encoding.
    private fun sweepThumbnails(remove:(File)->Boolean)=synchronized(thumbnails) {
        File(app.context.cacheDir,"thumbnails").listFiles()?.filter {it !in activeThumbnails && remove(it)}?.forEach {it.delete()}
    }
    internal fun clearThumbnails()=sweepThumbnails {true}
    internal var beforePayloadWrite:(Long)->Unit={}
    @Volatile internal var beforeAutomaticPayloadWrite:(Outgoing)->Unit={}
    @Volatile internal var afterAutomaticCaptureDrop:(Outgoing)->Unit={}
    private val space=Any()
    private var reserved=0L
    fun abortSources() {sources.forEach {source->ProviderReads.closeLater {source.close()}}}
    private val dir get()=File(app.context.filesDir,"outgoing").apply {mkdirs()}
    private fun capacity(extra: Long)=synchronized(space) {
        if(extra + reserved + (dir.listFiles()?.sumOf {it.length()} ?: 0) > 512L*1024*1024 || dir.usableSpace < 16L*1024*1024+extra+reserved) throw LocalCapacityException()
    }
    private fun writer(output:java.io.OutputStream,row:Outgoing?=null)=ReservingOutputStream(output,
        reserve={length->synchronized(space) {capacity(length);reserved+=length}},
        release={length->synchronized(space) {reserved-=length}},beforeWrite={length->beforePayloadWrite(length);if(row?.automatic==true)beforeAutomaticPayloadWrite(row)})
    @androidx.annotation.VisibleForTesting
    suspend fun capture(row:Outgoing):Boolean {
        val p=app.preferences.read()
        return capture(row,DeliveryBinding.from(p),if(row.automatic)io.github.stantonmatt.pneu.data.AdmissionToken.from(p,row.kind=="recording") else null)
    }
    /** False means an automatic snapshot was invalidated, cleaned and scheduled
     * for observation again. Storage/provider errors and caller cancellation retain
     * their normal failure boundaries. Manual captures cannot silently disappear. */
    suspend fun capture(row: Outgoing, binding: DeliveryBinding?,observation:io.github.stantonmatt.pneu.data.AdmissionToken?=null):Boolean = withContext(Dispatchers.IO) {
        var part:File?=null
        val target=File(dir,"${row.transferId}.bin")
        try {
            val admitted=StagedAdmission.publish(row.automatic,
                discard={part?.let {it.delete();target.delete()}},
                rescan={app.requestMediaScan();android.util.Log.d("PneuMedia","Dropped stale automatic capture; rescanning");afterAutomaticCaptureDrop(row)}) publication@{
                if(binding==null || !binding.matches(app.preferences.read()))return@publication false
                // Register before releasing the lock so cleanup cannot remove a live copy.
                val temp=mutex.withLock {capacity(0);File.createTempFile("capture-",".part",dir).also {part=it;activeParts.add(it)}}
                // Open private output before launching the abandonable worker. On a
                // timeout it is closed/unlinked here; a late provider cannot recreate it.
                val (count,hash)=FileOutputStream(temp).use {fileOutput->val output=writer(fileOutput,row);ProviderReads.copy(sourceTimeout) {signal,progress->
                    progress()
                    val asset=app.context.contentResolver.openAssetFileDescriptor(Uri.parse(row.uri),"r",signal) ?: throw java.io.FileNotFoundException()
                    sources.add(asset)
                    try {asset.use {source->
                    progress()
                    val digest=MessageDigest.getInstance("SHA-256");var count=0L
                    val fd=source.fileDescriptor
                    android.system.Os.fcntlInt(fd,android.system.OsConstants.F_SETFL,android.system.Os.fcntlInt(fd,android.system.OsConstants.F_GETFL,0) or android.system.OsConstants.O_NONBLOCK)
                    if(source.startOffset>0)android.system.Os.lseek(fd,source.startOffset,android.system.OsConstants.SEEK_SET)
                    val range=StreamRange(source.declaredLength)
                        val buffer=ByteArray(CHUNK_SIZE)
                        while(range.remaining!=0L) {
                            if(signal.isCanceled)throw java.io.IOException("Copy abandoned")
                            val n=try {android.system.Os.read(fd,buffer,0,range.limit(buffer.size))}
                            catch(e:android.system.ErrnoException) {
                                if(e.errno!=android.system.OsConstants.EAGAIN)throw e
                                val poll=android.system.StructPollfd().apply {this.fd=fd;events=android.system.OsConstants.POLLIN.toShort()}
                                android.system.Os.poll(arrayOf(poll),100);continue
                            }
                            if(!range.consume(n))break
                            progress();count+=n;require(count<=MAX_FILE)
                            output.write(buffer,0,n);digest.update(buffer,0,n)
                        }
                        source.parcelFileDescriptor.checkError()
                        fileOutput.fd.sync()
                        progress();count to digest.digest().hex()
                    }} finally {sources.remove(asset)}
                }}
                mutex.withLock {
                    if(!binding.matches(app.preferences.read()))return@withLock false
                    capacity(0)
                    check(temp.renameTo(target));syncDir()
                    row.path=target.absolutePath;row.size=count;row.hash=hash;row.sourceHash=row.hash
                    // Manual sources are never reopened after admission, including Recent.
                    if(!row.automatic)row.uri=""
                    Protocol.validate(Type.FILE_BEGIN,metadata(row))
                    app.enqueue(row,binding,observation)
                }
            }
            // This is outside the capture owner: cancellation or housekeeping
            // failure after commit must not delete an admitted snapshot.
            if(admitted)PostCommit.maintainSuspending {app.preferences.toggle("localCapacity",false)}
            admitted
        } catch(e:LocalCapacityException) {app.preferences.toggle("localCapacity",true);throw e}
        finally {part?.let {activeParts.remove(it)}}
    }
    suspend fun optimize(row:Outgoing)=withContext(Dispatchers.IO) {
        // An interrupted pre-send preparation may already have published a JPEG.
        // Its ID, bytes and metadata are immutable on every subsequent selection.
        if(row.optimized)return@withContext row
        check(!row.attempted)
        var committed=false
        val old=File(row.path);val newId=randomId()
        val part=try {mutex.withLock {capacity(0);File.createTempFile("encode-",".part",dir).also {activeParts.add(it)}}}
        catch(e:LocalCapacityException) {app.preferences.toggle("localCapacity",true);throw e}
        val target=File(dir,"$newId.bin")
        var bitmap:Bitmap?=null;var rendered:Bitmap?=null
        val candidate=Outgoing().also {copy->Outgoing::class.java.fields.forEach {it.set(copy,it.get(row))}}
        try {
            val bounds=BitmapFactory.Options().apply {inJustDecodeBounds=true};BitmapFactory.decodeFile(old.path,bounds)
            require(bounds.outWidth>0 && bounds.outHeight>0)
            var sample=1;while(maxOf(bounds.outWidth,bounds.outHeight)/sample>2560)sample*=2
            val decoded=ImageOrientation.apply(old,BitmapFactory.decodeFile(old.path,BitmapFactory.Options().apply {inSampleSize=sample;inPreferredConfig=Bitmap.Config.ARGB_8888}) ?: throw java.io.IOException())
            bitmap=decoded;var image=decoded
            if(maxOf(image.width,image.height)>2560) {
                val ratio=2560.0/maxOf(image.width,image.height)
                val scaled=Bitmap.createScaledBitmap(image,(image.width*ratio).toInt().coerceAtLeast(1),(image.height*ratio).toInt().coerceAtLeast(1),true)
                if(image!==decoded)image.recycle();image=scaled
            }
            rendered=image
            FileOutputStream(part).use {fileOutput->
                val output=writer(fileOutput)
                val encoded=image.compress(Bitmap.CompressFormat.JPEG,85,output)
                output.failure?.let {throw it};check(encoded);fileOutput.fd.sync()
            }
            val hash=part.inputStream().use {input->val digest=MessageDigest.getInstance("SHA-256");val b=ByteArray(CHUNK_SIZE);while(true){val n=input.read(b);if(n<0)break;digest.update(b,0,n)};digest.digest().hex()}
            candidate.transferId=newId;candidate.path=target.path;candidate.hash=hash;candidate.size=part.length();candidate.mime="image/jpeg"
            candidate.fileName=safeFilename(row.fileName,true);candidate.optimized=true
            Protocol.validate(Type.FILE_BEGIN,metadata(candidate))
            mutex.withLock {app.queueMutex.withLock {
                val current=app.queue.find(row.mediaId)
                if(current?.state !in setOf("waiting","failed") || current?.revision!=row.revision || current.transferId!=row.transferId || current.attempted || current.optimized || app.preferences.read().deliveryReset)throw java.io.IOException("Changed queue entry")
                check(part.renameTo(target));syncDir()
                app.queue.update(candidate);committed=true
                PostCommit.maintain {deleteCommittedSource(old)}
            }}
            candidate
        } catch(e:Exception) {part.delete();if(!committed)target.delete();if(e is LocalCapacityException)app.preferences.toggle("localCapacity",true);throw e}
        finally {activeParts.remove(part);if(rendered!==bitmap)rendered?.recycle();bitmap?.recycle()}
    }
    /** Completed rows retain only metadata and a small private thumbnail. */
    suspend fun completeFile(row:Outgoing):Boolean=withContext(Dispatchers.IO) {
        val target=try {synchronized(thumbnails) {
            File.createTempFile("thumb-",".jpg",File(app.context.cacheDir,"thumbnails").apply {mkdirs()}).also {activeThumbnails.add(it)}
        }}catch(_:Exception) {null}
        var retained=false
        try {
            val thumbnail=target?.let {MediaPreview.makeThumbnail(app.context,row,it,beforeThumbnailWrite)}
            if(thumbnail==null)target?.delete()
            mutex.withLock {app.queueMutex.withLock commit@{
                val current=app.queue.find(row.mediaId)
                if(current==null || current.state !in setOf("waiting","sending","failed") || current.revision!=row.revision || current.transferId!=row.transferId || app.preferences.read().deliveryReset) {thumbnail?.delete();return@commit false}
                val source=File(row.path)
                val candidate=Outgoing().also {copy->Outgoing::class.java.fields.forEach {it.set(copy,it.get(row))}}
                if(!row.automatic)candidate.uri=""
                candidate.path=thumbnail?.path.orEmpty()
                try {app.queue.update(candidate)}catch(e:Exception) {thumbnail?.delete();throw e}
                retained=true;row.path=candidate.path;row.uri=candidate.uri
                PostCommit.maintain {deleteCommittedSource(source)};app.maintainTerminalLocked()
                true
            }}
        }finally {synchronized(thumbnails) {
            if(!retained)target?.delete()
            target?.let {activeThumbnails.remove(it)}
        }}
    }
    suspend fun <T> idle(block: suspend ()->T): T=mutex.withLock {block()}
    suspend fun cleanup() = mutex.withLock {
        sweepThumbnails {app.queue.anyPath(it.path)==0}
        dir.listFiles()?.filter {it !in activeParts && (it.extension=="part" || (it.extension=="bin" && app.queue.hasPath(it.path)==0))}?.forEach {it.delete()}
    }
    private fun syncDir() {
        val fd=android.system.Os.open(dir.path,android.system.OsConstants.O_RDONLY,0)
        try {android.system.Os.fsync(fd)} finally {android.system.Os.close(fd)}
    }
    companion object {
        fun metadata(row: Outgoing)=json("transfer_id" to row.transferId,"file_name" to row.fileName,"mime" to row.mime,"size" to row.size,"sha256" to row.hash,"kind" to row.kind,"created_at" to row.createdAt,"source_media_id" to row.mediaId)
    }
}
