package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.ReceivedFile
import java.io.File
import java.nio.channels.FileChannel
import java.nio.file.Files
import java.nio.file.LinkOption.NOFOLLOW_LINKS
import java.nio.file.StandardOpenOption.*
import java.nio.file.attribute.BasicFileAttributes
import java.nio.ByteBuffer

/** Private, seekable receive stages. Room commits an offset only after force().
 * No user name or peer-supplied path is ever used as a filesystem authority. */
internal class ReceiveSpool(val root:File,private val wall:()->Long=System::currentTimeMillis,private val monotonic:()->Long={System.nanoTime()/1_000_000},private val beforeDelete:(ReceivedFile)->Unit={}) {
    private val progress=java.util.concurrent.ConcurrentHashMap<String,Pair<Long,Long>>()
    fun markProgress(row:ReceivedFile) {progress[row.stagePath]=row.checkpoint to monotonic()}
    fun expired(row:ReceivedFile):Boolean {
        val now=wall()
        if(expired(row,now))return true
        val previous=progress.compute(row.stagePath) {_,old->
            if(old?.first==row.checkpoint)old else row.checkpoint to (monotonic()-(now-row.progressWall).coerceAtLeast(0))
        }!!
        return monotonic()-previous.second>=TTL
    }
    init {check(root.isDirectory || root.mkdirs());check(!Files.isSymbolicLink(root.toPath()))}
    fun create(row:ReceivedFile) {
        val file=File.createTempFile("receive-",".part",root)
        row.stagePath=file.absolutePath;row.stageIdentity=identity(file)
        row.progressWall=wall();row.wallHighWater=row.progressWall;row.checkpointHash=io.github.stantonmatt.pneu.protocol.sha256(byteArrayOf());markProgress(row)
        FileChannel.open(root.toPath(),READ).use {it.force(true)}
    }
    private fun path(row:ReceivedFile):File?=File(row.stagePath).takeIf {
        it.parentFile==root && it.name.matches(Regex("receive-[a-zA-Z0-9-]+\\.part"))
    }
    private fun identity(file:File):String {
        val attributes=Files.readAttributes(file.toPath(),BasicFileAttributes::class.java,NOFOLLOW_LINKS)
        check(attributes.isRegularFile)
        return requireNotNull(attributes.fileKey()).toString()
    }
    fun matches(row:ReceivedFile):Boolean=try {path(row)?.let {identity(it)==row.stageIdentity} ?: false}catch(_:Exception) {false}
    fun reconcile(row:ReceivedFile):Boolean {
        if(expired(row))return false
        val file=path(row) ?: return false
        return try {
            if(identity(file)!=row.stageIdentity || file.length()<row.checkpoint || row.checkpoint !in 0..row.size)return false
            open(row).use {it.truncate(row.checkpoint);it.force(true)}
            row.wallHighWater=maxOf(row.wallHighWater,wall());true
        }catch(_:Exception) {false}
    }
    fun open(row:ReceivedFile):FileChannel {
        val file=path(row) ?: throw ReceiveStorageException("storage_failed")
        if(identity(file)!=row.stageIdentity)throw ReceiveStorageException("storage_failed")
        return FileChannel.open(file.toPath(),READ,WRITE,NOFOLLOW_LINKS).also {channel->
            if(identity(file)!=row.stageIdentity) {channel.close();throw ReceiveStorageException("storage_failed")}
        }
    }
    fun delete(row:ReceivedFile) {beforeDelete(row);progress.remove(row.stagePath);path(row)?.let {file->
        // Deleting our private directory entry never follows a symlink.
        Files.deleteIfExists(file.toPath())
    }}
    fun clean(retained:Set<String>) {root.listFiles()?.filter {it.absolutePath !in retained && it.name.matches(Regex("receive-[a-zA-Z0-9-]+\\.part"))}?.forEach {Files.deleteIfExists(it.toPath())}}
    companion object {
        const val TTL=24L*60*60*1000
        const val BYTE_LIMIT=2L*1024*1024*1024
        const val CHECKPOINT_BYTES=4L*1024*1024
        fun expired(row:ReceivedFile,now:Long)=now<row.wallHighWater || now<row.progressWall || now-row.progressWall>=TTL
        fun write(channel:FileChannel,bytes:ByteArray) {val buffer=ByteBuffer.wrap(bytes);while(buffer.hasRemaining())channel.write(buffer)}
    }
}
