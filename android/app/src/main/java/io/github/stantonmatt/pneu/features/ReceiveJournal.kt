package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.*

internal enum class FileAdmission { RETAINED_REPLAY, RETAINED_CONFLICT, NEW, UNFINISHED }
/** Files and text/settings reserve the same sender-scoped ID namespace. Full
 * semantic metadata lives in received_files; the shared reservation prevents a
 * text/settings race without putting file IDs in clipboard pruning. */
internal class ReceiveJournal(private val queue:QueueDao,private val storage:ReceiveStorage,private val peerLimit:Int=QueueDao.RECEIVE_PEER_LIMIT,private val globalLimit:Int=QueueDao.RECEIVE_GLOBAL_LIMIT) {
    fun admission(peer:String,metadata:ReceivedMetadata):FileAdmission {
        val receipt=queue.receipt(peer,metadata.id)
        val file=queue.receivedFile(peer,metadata.id)
        if(receipt==null && file==null)return FileAdmission.NEW
        if(receipt?.origin!="file" || file==null || file.metadata!=metadata.canonical)return FileAdmission.RETAINED_CONFLICT
        return if(file.state=="complete" && receipt.state=="applied")FileAdmission.RETAINED_REPLAY else FileAdmission.UNFINISHED
    }
    fun reserve(row:ReceivedFile,metadata:ReceivedMetadata) {
        val reserved=queue.reserveReceivedFile(row,Receipt().apply {
            peerId=row.peerId;transferId=row.transferId;hash=metadata.hash;origin="file";state="reserved";
            createdAt=metadata.createdAt;hlc=metadata.hlc.orEmpty();sourceDevice=metadata.source.orEmpty()
        },peerLimit,globalLimit)
        if(!reserved)throw ReceiveStorageException("storage_failed")
    }
    fun verified(row:ReceivedFile) {row.state="verified";queue.updateReceivedFile(row)}
    /** The verified record and destination URI precede IS_PENDING=0. Recovery can
     * query that exact URI and finish the receipt after a crash in either window. */
    private fun pending(row:ReceivedFile)=if(row.catalogVersion==storage.version())storage.pending(row.uri) else null
    fun deletePartial(row:ReceivedFile) {if(row.state!="complete" && row.uri.isNotEmpty() && row.catalogVersion==storage.version())storage.deletePending(row.uri)}
    fun publish(row:ReceivedFile) {
        check(row.state=="verified")
        when(pending(row)) {
            true->storage.publish(row.uri)
            false->Unit
            null->throw ReceiveStorageException("storage_failed")
        }
        queue.completeReceivedFile(row)
    }
    fun recover(row:ReceivedFile,allowPublication:Boolean=true) {
        if(row.state=="verified")when(pending(row)) {
            false->publish(row) // The effect happened: finish its receipt despite policy changes.
            true->if(allowPublication)publish(row) else {
                deletePartial(row);queue.abandonReceivedFile(row)
            }
            null->queue.abandonReceivedFile(row)
        } else if(row.state!="complete")abandon(row)
    }
    fun abandon(row:ReceivedFile) {
        // A verified row may already be visible. Never remove it or its reserved
        // ID on a failed completion; replay must first recover this exact URI.
        val retained=queue.receivedFile(row.peerId,row.transferId) ?: return
        if(!QueueDao.sameReceivedFile(retained,row))return
        if(retained.state=="verified" || retained.state=="complete")return
        deletePartial(retained)
        queue.abandonReceivedFile(retained)
    }
    fun forget(peer:String) {
        // Explicit unpair preserves public user files; only partial rows go away.
        var after=""
        while(true) {
            val page=queue.unfinishedPeerReceivedFiles(peer,after,CLEANUP_PAGE)
            if(page.isEmpty())return
            page.forEach(::deletePartial)
            after=page.last().transferId
        }
    }
    companion object {internal const val CLEANUP_PAGE=64}
}
