package io.github.stantonmatt.pneu.features

import com.google.gson.Gson
import com.google.gson.JsonArray
import com.google.gson.JsonObject
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import java.io.Closeable
import java.io.IOException
import java.security.MessageDigest

internal enum class TreeKind { FILE, DIRECTORY, LINK, SPECIAL }
internal data class TreeNode(val id:String,val name:String,val kind:TreeKind,val size:Long,val modified:Long)
internal interface TreeRead:Closeable {
    suspend fun read(bytes:ByteArray):Int
    /** Must position without reading content. False restarts a nonseekable source at zero. */
    suspend fun seek(offset:Long):Boolean=false
}
internal interface FolderTree {
    suspend fun stat(id:String):TreeNode
    suspend fun children(id:String):List<TreeNode>
    suspend fun open(id:String):TreeRead
}
internal class FolderChanged(val fileId:String?=null):IOException("Folder changed after Preparing. Send the folder again.")
internal fun TreeNode.sameDirectory(other:TreeNode)=id==other.id && kind==TreeKind.DIRECTORY && other.kind==TreeKind.DIRECTORY
internal class FolderInvalid(val reason:Reason):IOException() { enum class Reason { PATH, DEPTH, COUNT, UNREADABLE, CHANGING, METADATA } }
internal class FolderReadFailed(val fileId:String?,cause:Exception):IOException("Folder source read failed",cause)
internal class FolderUnsafe(val link:Boolean):IOException()
internal class FolderEmpty(val folder:String):IOException()
internal class FolderTooLarge(val folder:String):IOException()
internal data class FolderSkipped(val unreadable:Int=0,val names:List<String> = emptyList(),val links:Int=0,val special:Int=0,val directories:Int=0,val directoryNames:List<String> = emptyList()) {
    val count get()=unreadable+directories+links+special
}
internal data class FolderSummary(val manifestLen:Long,val count:Int,val skipped:FolderSkipped) {
    val unsent get()=skipped.count
}

internal data class FolderFile(val node:TreeNode,val path:String,val hash:String)
internal data class FolderCapture(val root:TreeNode,val manifest:String,val files:List<FolderFile>,val summary:FolderSummary,val size:Long,val hash:String) {
    val bytes get()=manifest.toByteArray(Charsets.UTF_8)
    fun begin()=json("transfer_id" to "0".repeat(32),"file_name" to safeFilename(root.name),"mime" to FOLDER_MIME,"size" to size,"sha256" to hash,"kind" to "file","created_at" to 0,"resume" to true,"folder" to Folder(summary.manifestLen))
    fun validate() {
        val parsed=FolderManifest.parse(bytes,begin())
        val entries=parsed.entries.filter {it.type==FolderEntryType.FILE}
        require(root.kind==TreeKind.DIRECTORY && summary.count==files.size)
        require(files.map {it.node.id}.toSet().size==files.size)
        require(files.all {it.node.kind==TreeKind.FILE})
        require(entries.size==files.size && entries.zip(files).all {(e,f)->e.path==f.path && e.size==f.node.size && e.sha256==f.hash})
    }
    fun encode():String=Gson().toJson(this)
    companion object {
        fun decode(text:String)=Gson().fromJson(text,FolderCapture::class.java).also {it.validate()}
    }
}
internal data class FolderPosition(val manifestOffset:Int?,val fileIndex:Int,val fileOffset:Long)
/** Offsets refer to the concatenation, including the exact frozen manifest bytes. */
internal fun folderPosition(capture:FolderCapture,offset:Long):FolderPosition {
    require(offset in 0..capture.size)
    val m=capture.summary.manifestLen
    if(offset<m)return FolderPosition(offset.toInt(),0,0)
    var start=m
    capture.files.forEachIndexed {i,f->
        if(offset<start+f.node.size)return FolderPosition(null,i,offset-start)
        start+=f.node.size
    }
    return FolderPosition(null,capture.files.size,0)
}

internal object FolderBuilder {
    suspend fun prepare(tree:FolderTree,rootId:String,progress:(Int,Long,Long?)->Unit={_,_,_->}):FolderCapture {
        val root=tree.stat(rootId);require(root.kind==TreeKind.DIRECTORY)
        val found=mutableListOf<Pair<String,TreeNode>>()
        val visited=mutableSetOf(root.id)
        var links=0;var special=0;var unreadable=0
        val names=mutableListOf<String>();val directoryNames=mutableListOf<String>();var unreadableDirectories=0
        fun skipped(node:TreeNode) {
            val name=node.name.replace('\n',' ').replace('\r',' ').take(128)
            if(node.kind==TreeKind.DIRECTORY) {unreadableDirectories++;if(directoryNames.size<3)directoryNames.add(name)}
            else {unreadable++;if(names.size<3)names.add(name)}
        }
        var count=0;var declared=0L;var inspected=0
        val paths=mutableSetOf<String>()
        suspend fun walk(dir:TreeNode,prefix:String):Boolean {
            currentCoroutineContext().ensureActive()
            val children=try {tree.children(dir.id)}catch(e:kotlinx.coroutines.CancellationException) {throw e}catch(e:SecurityException) {throw e}catch(e:FolderInvalid) {throw e}catch(_:Exception) {if(dir==root)throw FolderInvalid(FolderInvalid.Reason.UNREADABLE);skipped(dir);return false}
            for(node in children) {
                currentCoroutineContext().ensureActive()
                if(++inspected>MAX_MANIFEST_ENTRIES)throw FolderInvalid(FolderInvalid.Reason.COUNT)
                if(node.kind==TreeKind.FILE && (node.name in setOf(".DS_Store","Thumbs.db","desktop.ini") || node.name.startsWith("._")))continue
                if(!visited.add(node.id)) {links++;continue}
                if(node.kind==TreeKind.LINK) {links++;continue}
                if(node.kind==TreeKind.SPECIAL) {special++;continue}
                if('/' in node.name)throw FolderInvalid(FolderInvalid.Reason.PATH)
                val path=if(prefix.isEmpty())node.name else "$prefix/${node.name}"
                if(path.split('/').size>MAX_FOLDER_DEPTH)throw FolderInvalid(FolderInvalid.Reason.DEPTH)
                try {validateFolderPath(path)}catch(_:IllegalArgumentException) {throw FolderInvalid(FolderInvalid.Reason.PATH)}
                if(!paths.add(path))throw FolderInvalid(FolderInvalid.Reason.PATH)
                if(node.kind==TreeKind.DIRECTORY) {
                    val start=found.size;val before=count
                    found.add(path to node);val enumerated=walk(node,path)
                    // A failed enumeration or replaced directory excludes the
                    // whole subtree; do not invent an empty replacement folder.
                    val unchanged=if(enumerated) {
                        try {node.sameDirectory(tree.stat(node.id))}
                        catch(e:kotlinx.coroutines.CancellationException) {throw e}catch(e:SecurityException) {throw e}catch(_:Exception) {false}
                    }else false
                    if(!unchanged) {
                        found.subList(start,found.size).clear();count=before;if(enumerated)skipped(node)
                    }
                }else {
                    if(node.size<0) {skipped(node);continue}
                    declared=minOf(MAX_FILE+1,declared+minOf(node.size,MAX_FILE+1));count++;found.add(path to node)
                }
                progress(count,0,null)
            }
            return true
        }
        walk(root,"")
        if(!root.sameDirectory(tree.stat(root.id)))throw FolderChanged()
        declared=found.filter {it.second.kind==TreeKind.FILE}.fold(0L) {sum,entry->minOf(MAX_FILE+1,sum+minOf(entry.second.size,MAX_FILE+1))}
        if(declared>MAX_FILE)throw FolderTooLarge(root.name)
        val entries=mutableListOf<FolderEntry>();val files=mutableListOf<FolderFile>()
        var done=0L;var captured=0L
        for((path,node) in found) {
            currentCoroutineContext().ensureActive()
            if(node.kind==TreeKind.DIRECTORY)entries.add(FolderEntry(path,FolderEntryType.DIRECTORY))
            else try {
                val digest=MessageDigest.getInstance("SHA-256");var read=0L
                require(tree.stat(node.id)==node)
                tree.open(node.id).use {input->
                    if(node.size>MAX_FILE-captured)throw FolderTooLarge(root.name)
                    val b=ByteArray(CHUNK_SIZE)
                    while(true) {currentCoroutineContext().ensureActive();val n=input.read(b);if(n<0)break;if(n==0)throw IOException("Empty provider read")
                        read+=n;if(read>node.size)throw FolderChanged();digest.update(b,0,n);done+=n;progress(count,done/2,declared)}
                }
                if(read!=node.size || tree.stat(node.id)!=node)throw FolderChanged()
                val hash=digest.digest().hex()
                entries.add(FolderEntry(path,FolderEntryType.FILE,node.size,hash));files.add(FolderFile(node,path,hash));captured+=node.size
            }catch(e:kotlinx.coroutines.CancellationException) {throw e}catch(e:SecurityException) {throw e}catch(e:FolderTooLarge) {throw e}catch(e:FolderUnsafe) {if(e.link)links++ else special++}catch(_:Exception) {skipped(node)}
        }
        var restarts=0
        while(true) {
            if(files.isEmpty()) {if(unreadable+unreadableDirectories>0)throw FolderInvalid(FolderInvalid.Reason.UNREADABLE);throw FolderEmpty(root.name)}
            // Reflection field order differs between ART and the JVM. Choose
            // the capture's byte representation explicitly, then freeze it.
            val encodedEntries=JsonArray()
            for(entry in entries)encodedEntries.add(JsonObject().apply {
                addProperty("path",entry.path)
                addProperty("type",if(entry.type==FolderEntryType.DIRECTORY)"directory" else "file")
                if(entry.type==FolderEntryType.FILE) {
                    addProperty("size",requireNotNull(entry.size));addProperty("sha256",requireNotNull(entry.sha256))
                }
            })
            val manifest=JsonObject().apply {add("entries",encodedEntries)}.toString()
            val m=manifest.toByteArray(Charsets.UTF_8).size.toLong()
            if(m>MAX_MANIFEST_BYTES)throw FolderInvalid(FolderInvalid.Reason.METADATA)
            var size=m
            for(file in files) {if(file.node.size>MAX_FILE-size)throw FolderTooLarge(root.name);size+=file.node.size}
            val capture=FolderCapture(root,manifest,files.toList(),FolderSummary(m,files.size,FolderSkipped(unreadable,names.toList(),links,special,unreadableDirectories,directoryNames.toList())),size,"0".repeat(64))
            capture.validate()
            // The manifest contains file hashes, so reading in place needs a
            // second pass to calculate SHA(manifest || content).
            try {
                progress(files.size,size/2,size)
                val hash=FolderStream(tree,capture).use {stream->stream.seek(0);val digest=MessageDigest.getInstance("SHA-256");val b=ByteArray(CHUNK_SIZE);var streamDone=0L
                    while(true) {val n=stream.read(b);if(n<0)break;digest.update(b,0,n);streamDone+=n;progress(files.size,(size+streamDone)/2,size)}
                    digest.digest().hex()
                }
                if(!root.sameDirectory(tree.stat(root.id)))throw FolderChanged()
                return capture.copy(hash=hash)
            }catch(e:IOException) {
                val id=when(e) {is FolderChanged->e.fileId;is FolderReadFailed->e.fileId;else->throw e}
                val failed=files.find {it.node.id==id} ?: throw e
                if(restarts++>=1)throw FolderInvalid(FolderInvalid.Reason.CHANGING)
                skipped(failed.node);files.remove(failed);entries.removeAll {it.path==failed.path}
            }
        }
    }
}

/** Reopens in place. Fresh files are checked inline; a resumed partial file is
 * validated by the receiver, without any unbounded reads after READY. */
internal class FolderStream(private val tree:FolderTree,private val capture:FolderCapture,private val cancelled:()->Boolean={false}):Closeable {
    private val manifest=capture.bytes
    private var manifestAt=0;private var index=0;private var read=0L
    private var partial=false
    private var input:TreeRead?=null;private var digest=MessageDigest.getInstance("SHA-256")
    private suspend fun checkActive() {currentCoroutineContext().ensureActive();if(cancelled())throw IOException("Folder send cancelled")}
    suspend fun verify() {
        try {
            if(!capture.root.sameDirectory(tree.stat(capture.root.id)))throw FolderChanged()
            val whole=MessageDigest.getInstance("SHA-256").apply {update(manifest)}
            for(file in capture.files) {
                if(tree.stat(file.node.id)!=file.node)throw FolderChanged()
                val d=MessageDigest.getInstance("SHA-256");var size=0L
                tree.open(file.node.id).use {source->val b=ByteArray(CHUNK_SIZE)
                    while(true) {checkActive();val n=source.read(b);if(n<0)break;if(n==0)throw IOException("Empty provider read");size+=n;if(size>file.node.size)throw FolderChanged();d.update(b,0,n);whole.update(b,0,n)}
                }
                if(size!=file.node.size || d.digest().hex()!=file.hash || tree.stat(file.node.id)!=file.node)throw FolderChanged()
            }
            if(whole.digest().hex()!=capture.hash)throw FolderChanged()
        }catch(e:kotlinx.coroutines.CancellationException) {throw e}
    }
    suspend fun seek(offset:Long) {
        close();val p=folderPosition(capture,offset)
        manifestAt=p.manifestOffset ?: manifest.size;index=p.fileIndex;read=0;digest=MessageDigest.getInstance("SHA-256")
        partial=p.fileOffset>0
        if(index<capture.files.size && partial) {
            enter()
            if(!requireNotNull(input).seek(p.fileOffset))throw IOException("Provider cannot seek; restart from zero")
            read=p.fileOffset
        }
    }

    private suspend fun enter() {
        if(input!=null)return
        val f=capture.files[index]
        if(tree.stat(f.node.id)!=f.node)throw FolderChanged()
        input=tree.open(f.node.id)
    }
    suspend fun read(buffer:ByteArray):Int=try {readChecked(buffer)}
        catch(e:kotlinx.coroutines.CancellationException) {throw e}
        catch(e:FolderChanged) {throw FolderChanged(capture.files.getOrNull(index)?.node?.id)}
        catch(e:SecurityException) {throw e}
        catch(e:Exception) {throw FolderReadFailed(capture.files.getOrNull(index)?.node?.id,e)}
    private suspend fun readChecked(buffer:ByteArray):Int {
        checkActive()
        if(manifestAt<manifest.size) {val n=minOf(buffer.size,manifest.size-manifestAt);manifest.copyInto(buffer,0,manifestAt,manifestAt+n);manifestAt+=n;return n}
        while(index<capture.files.size) {
            enter();val file=capture.files[index]
            val n=requireNotNull(input).read(buffer)
            if(n==0)throw IOException("Empty provider read")
            if(n>0) {read+=n;if(read>file.node.size)throw FolderChanged();digest.update(buffer,0,n);return n}
            if(read!=file.node.size || (!partial && digest.digest().hex()!=file.hash) || tree.stat(file.node.id)!=file.node)throw FolderChanged()
            close();index++;read=0;partial=false;digest=MessageDigest.getInstance("SHA-256")
        }
        return -1
    }
    /** Consume EOF/zero-length entries even when the declared byte count is sent. */
    suspend fun finish() {if(read(ByteArray(1))!=-1)throw FolderChanged()}
    override fun close() {input?.close();input=null}
}
