package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.*

/** The capacity decision and recipient-specific superseding share the same
 * transaction. Only artifacts actually retired by admission free capacity. */
internal object QueueAdmission {
    /** A suppressed observation is complete even though it creates no delivery. */
    fun retireAutomatic(database:QueueDatabase,row:Outgoing) {
        require(row.automatic)
        val queue=database.queue()
        database.runInTransaction {
            queue.insertSeen(MediaSeen().apply {mediaId=row.mediaId})
            queue.deletePendingAdmission(row.mediaId)
        }
        row.state="skipped"
    }
    fun admit(database:QueueDatabase,row:Outgoing):Boolean {
        val queue=database.queue()
        var inserted=false
        database.runInTransaction {
            row.orderId=queue.nextOrder()
            for(target in requireNotNull(row.recipients)) {
                row.peerId=target.pcId;row.bindingGeneration=target.generation
                row.sessionId=if(row.kind in setOf("text","clipboard_image"))target.sessionId.orEmpty() else ""
                inserted=queue.admit(row)!=-1L || inserted
            }
            // Shared text remains active until every active delivery retires;
            // sending and unrelated recipients never create fictitious space.
            if(row.state=="waiting" && queue.pendingCount()>2048)throw LocalCapacityException()
            if(inserted) {
                queue.rememberOutgoing(if(row.kind=="text")row.createdAt else 0,row.orderId)
                if(row.automatic) {queue.insertSeen(MediaSeen().apply {mediaId=row.mediaId});queue.deletePendingAdmission(row.mediaId)}
            }
        }
        return inserted
    }
}
