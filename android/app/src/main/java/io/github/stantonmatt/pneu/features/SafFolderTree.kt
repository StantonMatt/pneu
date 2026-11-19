package io.github.stantonmatt.pneu.features

import android.content.Context
import android.net.Uri
import android.os.CancellationSignal
import android.provider.DocumentsContract
import android.system.Os
import android.system.OsConstants
import io.github.stantonmatt.pneu.protocol.MAX_MANIFEST_ENTRIES
import java.io.IOException

/** Provider IDs, never display names, are the authority for tree traversal. */
internal class SafFolderTree(private val context:Context,private val tree:Uri):FolderTree {
    val rootId:String=DocumentsContract.getTreeDocumentId(tree)
    private val resolver=context.contentResolver
    private val columns=arrayOf(DocumentsContract.Document.COLUMN_DOCUMENT_ID,DocumentsContract.Document.COLUMN_DISPLAY_NAME,
        DocumentsContract.Document.COLUMN_MIME_TYPE,DocumentsContract.Document.COLUMN_SIZE,DocumentsContract.Document.COLUMN_LAST_MODIFIED,DocumentsContract.Document.COLUMN_FLAGS)
    private fun uri(id:String)=DocumentsContract.buildDocumentUriUsingTree(tree,id)
    private fun node(c:android.database.Cursor):TreeNode {
        val mime=c.getString(2).orEmpty()
        val id=c.getString(0);require(id.toByteArray(Charsets.UTF_8).size<=8192) {"Document identity is too long"}
        val kind=when {
            mime==DocumentsContract.Document.MIME_TYPE_DIR->TreeKind.DIRECTORY
            mime in setOf("inode/symlink","application/x-symlink")->TreeKind.LINK
            mime.startsWith("inode/") || c.getLong(5) and DocumentsContract.Document.FLAG_VIRTUAL_DOCUMENT.toLong()!=0L->TreeKind.SPECIAL
            else->TreeKind.FILE
        }
        // Unknown size is unsafe for an immutable, declared-length stream.
        return TreeNode(id,c.getString(1),kind,if(c.isNull(3))-1 else c.getLong(3),if(c.isNull(4))0 else c.getLong(4))
    }
    override suspend fun stat(id:String):TreeNode=ProviderReads.call {signal->
        resolver.query(uri(id),columns,null,null,null,signal)?.use {c->if(c.moveToFirst())node(c) else null} ?: throw IOException("Document disappeared")
    }
    override suspend fun children(id:String):List<TreeNode> = ProviderReads.call {signal->
        resolver.query(DocumentsContract.buildChildDocumentsUriUsingTree(tree,id),columns,null,null,null,signal)?.use {c->
            buildList {while(c.moveToNext()) {signal.throwIfCanceled();if(size>=MAX_MANIFEST_ENTRIES)throw FolderInvalid(FolderInvalid.Reason.COUNT);add(node(c))}}
        } ?: throw IOException("Couldn't read directory")
    }
    override suspend fun open(id:String):TreeRead=ProviderReads.call(discard={it.close()}) {signal->
        val asset=resolver.openAssetFileDescriptor(uri(id),"r",signal) ?: throw IOException("Couldn't open document")
        try {
            val fd=asset.fileDescriptor;val stat=Os.fstat(fd)
            if(OsConstants.S_ISREG(stat.st_mode) && stat.st_nlink>1)throw FolderUnsafe(true)
            Os.fcntlInt(fd,OsConstants.F_SETFL,Os.fcntlInt(fd,OsConstants.F_GETFL,0) or OsConstants.O_NONBLOCK)
            if(asset.startOffset>0)Os.lseek(fd,asset.startOffset,OsConstants.SEEK_SET)
            object:TreeRead {
                private var remaining=asset.declaredLength
                private var closed=false
                override suspend fun seek(offset:Long):Boolean=ProviderReads.call {
                    try {
                        Os.lseek(fd,asset.startOffset+offset,OsConstants.SEEK_SET)
                        remaining=if(asset.declaredLength<0)-1 else asset.declaredLength-offset
                        true
                    }catch(e:android.system.ErrnoException) {if(e.errno==OsConstants.ESPIPE)false else throw e}
                }
                override suspend fun read(bytes:ByteArray):Int=ProviderReads.copy {cancel,progress->
                    progress();cancel.setOnCancelListener {ProviderReads.closeLater {asset.close()}}
                    if(closed)throw IOException("Closed folder source")
                    if(remaining==0L) {asset.parcelFileDescriptor.checkError();return@copy -1}
                    while(true) {
                        cancel.throwIfCanceled()
                        val n=try {Os.read(fd,bytes,0,if(remaining<0)bytes.size else minOf(bytes.size.toLong(),remaining).toInt())}
                        catch(e:android.system.ErrnoException) {
                            if(e.errno!=OsConstants.EAGAIN)throw e
                            Os.poll(arrayOf(android.system.StructPollfd().apply {this.fd=fd;events=OsConstants.POLLIN.toShort()}),100);continue
                        }
                        progress();asset.parcelFileDescriptor.checkError()
                        if(n==0)return@copy -1
                        if(remaining>=0)remaining-=n
                        return@copy n
                    }
                    @Suppress("UNREACHABLE_CODE") -1
                }
                override fun close() {closed=true;ProviderReads.closeLater {asset.close()}}
            }
        }catch(e:Exception) {asset.close();throw e}
    }
}
