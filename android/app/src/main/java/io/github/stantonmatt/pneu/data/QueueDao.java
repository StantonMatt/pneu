package io.github.stantonmatt.pneu.data;
import androidx.room.*;
import java.util.List;
import kotlinx.coroutines.flow.Flow;
@Dao public interface QueueDao {
    @Insert(onConflict=OnConflictStrategy.ABORT) void insertReceivedFile(ReceivedFile row);
    @Update void updateReceivedFile(ReceivedFile row);
    @Query("SELECT * FROM received_files WHERE peerId=:peer AND transferId=:id") ReceivedFile receivedFile(String peer,String id);
    @Query("SELECT * FROM received_files WHERE catalogVersion=:version AND uri LIKE 'content://media/external_primary/%/' || :mediaId AND state='complete' LIMIT 1") ReceivedFile receivedFileByMediaId(String version,long mediaId);
    @Query("SELECT * FROM received_files WHERE state!='complete'") List<ReceivedFile> unfinishedReceivedFiles();
    @Query("SELECT * FROM received_files WHERE peerId=:peer AND state!='complete' AND transferId>:after ORDER BY transferId LIMIT :limit") List<ReceivedFile> unfinishedPeerReceivedFiles(String peer,String after,int limit);
    @Query("SELECT COUNT(*) FROM received_files WHERE peerId=:peer") int receivedFileCount(String peer);
    @Query("SELECT COUNT(*) FROM received_files") int receivedFileCount();
    int RECEIVE_PEER_LIMIT=4096;
    int RECEIVE_GLOBAL_LIMIT=16384;
    @Query("SELECT * FROM received_files WHERE state='complete' ORDER BY receivedAt DESC,peerId,transferId LIMIT 50") Flow<List<ReceivedFile>> recentReceivedFiles();
    @Query("DELETE FROM received_files WHERE peerId=:peer") void deletePeerReceivedFiles(String peer);
    @Query("DELETE FROM received_files") void clearReceivedFiles();
    @Query("DELETE FROM received_files WHERE peerId=:peer AND transferId=:id AND generation=:generation AND metadata=:metadata AND uri=:uri AND catalogVersion=:version AND state=:state AND state!='complete'")
    int deleteReceivedFileIfCurrent(String peer,String id,long generation,String metadata,String uri,String version,String state);
    @Transaction default boolean reserveReceivedFile(ReceivedFile row,Receipt receipt,int peerLimit,int globalLimit) {
        // Counts include permanent receipts/history and unfinished reservations.
        // Room's write transaction makes concurrent peers share one durable cap.
        if(receivedFileCount(row.peerId)>=peerLimit || receivedFileCount()>=globalLimit)return false;
        if(insertReceipt(receipt)==-1L)throw new IllegalStateException("Transfer ID already reserved");
        insertReceivedFile(row);return true;
    }
    @Transaction default void completeReceivedFile(ReceivedFile row) {
        ReceivedFile retained=receivedFile(row.peerId,row.transferId);
        Receipt receipt=receipt(row.peerId,row.transferId);
        if(!sameReceivedFile(retained,row) || !retained.state.equals("verified") || receipt==null || !receipt.origin.equals("file") || !receipt.state.equals("reserved"))throw new IllegalStateException("Missing file reservation");
        receipt.state="applied";updateReceipt(receipt);row.state="complete";updateReceivedFile(row);
    }
    static boolean sameReceivedFile(ReceivedFile retained,ReceivedFile row) {
        return retained!=null && retained.generation==row.generation && retained.metadata.equals(row.metadata) && retained.uri.equals(row.uri) && retained.catalogVersion.equals(row.catalogVersion);
    }
    @Transaction default boolean abandonReceivedFile(ReceivedFile row) {
        Receipt receipt=receipt(row.peerId,row.transferId);
        if(receipt==null || !receipt.origin.equals("file") || !receipt.state.equals("reserved"))return false;
        if(deleteReceivedFileIfCurrent(row.peerId,row.transferId,row.generation,row.metadata,row.uri,row.catalogVersion,row.state)!=1)return false;
        deleteReceipt(row.peerId,row.transferId);return true;
    }

    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertPendingAdmission(PendingMediaAdmission row);
    @Query("SELECT * FROM pending_media_admissions WHERE mediaId=:id") PendingMediaAdmission pendingAdmission(String id);
    @Query("SELECT * FROM pending_media_admissions ORDER BY rowid") List<PendingMediaAdmission> pendingAdmissions();
    @Query("DELETE FROM pending_media_admissions WHERE mediaId=:id") void deletePendingAdmission(String id);
    @Query("DELETE FROM pending_media_admissions") void clearPendingAdmissions();
    @Transaction default Outgoing freezeAutomaticAdmission(Outgoing row) {
        insertPendingAdmission(PendingMediaAdmission.from(row));return pendingAdmission(row.mediaId).row();
    }
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertArtifact(Artifact row);
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertDelivery(Delivery row);
    @Update void updateArtifact(Artifact row);
    @Update void updateRecipient(Delivery row);
    @Transaction default long insert(Outgoing row) {
        insertArtifact(Artifact.from(row));return insertDelivery(Delivery.from(row));
    }
    @Transaction default long admit(Outgoing row) {
        String state=row.state;
        try {
            if(row.kind.equals("text")) {
                if(row.createdAt<=peerTextTime(row.peerId))row.state="superseded";
                else supersedePeerText(row.peerId);
            }
            long added=insert(row);
            if(added!=-1L && row.kind.equals("text"))rememberPeerText(row.peerId,row.createdAt);
            return added;
        }finally {row.state=state;}
    }
    // State mutations never overwrite another recipient or the shared payload.
    default void update(Outgoing row) {updateRecipient(Delivery.from(row));}
    @Transaction default void replacePayload(Outgoing row) {updateArtifact(Artifact.from(row));update(row);}
    @Query("SELECT * FROM outgoing WHERE mediaId=:id AND peerId=:peer") Outgoing delivery(String id,String peer);
    @Query("SELECT * FROM outgoing WHERE mediaId=:id") List<Outgoing> deliveries(String id);
    @Query("SELECT * FROM outgoing WHERE peerId=:peer AND state IN ('waiting','sending','failed') AND error!='old_device_replaced' ORDER BY orderId,mediaId") List<Outgoing> pendingPeer(String peer);
    @Query("UPDATE deliveries SET peerId=:peer,bindingGeneration=:generation WHERE peerId=''") void bindLegacy(String peer,long generation);
    @Transaction default void resolveLegacy(Prefs p) {
        if(p.getLegacyQueuePeer().isEmpty() || p.getResetPeers().contains(p.getLegacyQueuePeer()) || p.getDeliveryReset())deletePeerDeliveries("");
        else {
            bindLegacy(p.getLegacyQueuePeer(),p.getLegacyQueueGeneration());
            if(peerTextTime("")>0)rememberPeerText(p.getLegacyQueuePeer(),peerTextTime(""));
        }
        deletePeerOutboundClock("");
    }
    @Query("DELETE FROM deliveries WHERE peerId=:peer AND error!='old_device_replaced'") void deletePeerDeliveries(String peer);
    @Query("UPDATE deliveries SET state='failed',error='old_device_replaced',nextAttempt=9223372036854775807,revision=revision+1 WHERE peerId=:peer AND bindingGeneration=:generation AND state IN ('waiting','sending','failed') AND error!='old_device_replaced'") void cancelReplacedPeer(String peer,long generation);
    @Query("DELETE FROM deliveries WHERE mediaId=:id AND peerId=:peer") void deleteDelivery(String id,String peer);
    @Query("DELETE FROM receipts WHERE peerId=:peer") void deletePeerReceipts(String peer);
    @Query("DELETE FROM clipboard_clock WHERE peerId=:peer") void deletePeerClock(String peer);
    @Transaction default void clearPeer(String peer) {deletePeerDeliveries(peer);deletePeerReceipts(peer);deletePeerClock(peer);deletePeerOutboundClock(peer);deletePeerReceivedFiles(peer);}
    @Query("SELECT * FROM artifacts WHERE mediaId NOT IN (SELECT mediaId FROM deliveries)") List<Artifact> orphanArtifacts();
    @Query("DELETE FROM artifacts WHERE mediaId=:id") void deleteArtifact(String id);
    @Query("SELECT COUNT(*) FROM outgoing WHERE mediaId=:id AND attempted=1") int anyAttempted(String id);
    @Query("SELECT COUNT(*) FROM outgoing WHERE peerId=:peer AND state='sending' AND (kind='text')=:text") int sendingPeerLane(String peer,boolean text);
    @Query("UPDATE deliveries SET state='waiting' WHERE peerId=:peer AND state='sending' AND mediaId IN (SELECT mediaId FROM artifacts WHERE (kind='text')=:text)") void recoverPeerLane(String peer,boolean text);
    @Query("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE peerId=:peer AND state IN ('waiting','failed') AND mediaId IN (SELECT mediaId FROM outgoing WHERE peerId=:peer AND kind='text' AND orderId<(SELECT MAX(orderId) FROM outgoing WHERE peerId=:peer AND kind='text' AND state IN ('waiting','failed'))) AND error!='old_device_replaced'") void supersedeOlderPeerText(String peer);
    @Transaction default void recoverPeer(String peer,boolean text) {recoverPeerLane(peer,text);if(text)supersedeOlderPeerText(peer);}
    @Query("SELECT COUNT(*) FROM outgoing WHERE peerId=:peer AND bindingGeneration=:generation AND state='sending' AND (kind='text')=:text") int sendingBoundLane(String peer,long generation,boolean text);
    @Query("UPDATE deliveries SET state='waiting' WHERE peerId=:peer AND bindingGeneration=:generation AND state='sending' AND mediaId IN (SELECT mediaId FROM artifacts WHERE (kind='text')=:text)") void recoverBoundSending(String peer,long generation,boolean text);
    @Query("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE peerId=:peer AND bindingGeneration=:generation AND state IN ('waiting','failed') AND mediaId IN (SELECT mediaId FROM artifacts WHERE kind='text') AND error!='old_device_replaced'") void supersedeBoundText(String peer,long generation);
    @Query("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE peerId=:peer AND bindingGeneration=:generation AND state IN ('waiting','failed') AND mediaId IN (SELECT mediaId FROM outgoing WHERE peerId=:peer AND bindingGeneration=:generation AND kind='text' AND orderId<(SELECT MAX(orderId) FROM outgoing WHERE peerId=:peer AND bindingGeneration=:generation AND kind='text' AND state IN ('waiting','failed'))) AND error!='old_device_replaced'") void supersedeOlderBoundText(String peer,long generation);
    @Transaction default void recoverBound(String peer,long generation,boolean text) {recoverBoundSending(peer,generation,text);if(text)supersedeOlderBoundText(peer,generation);}
    @Query("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE peerId=:peer AND mediaId IN (SELECT mediaId FROM artifacts WHERE kind='text') AND state IN ('waiting','failed') AND error!='old_device_replaced'") void supersedePeerText(String peer);
    @Query("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE peerId=:peer AND bindingGeneration=:generation AND state IN ('waiting','sending','failed') AND mediaId IN (SELECT mediaId FROM artifacts WHERE kind IN ('text','clipboard_image')) AND error!='old_device_replaced'") void dropPeerClipboard(String peer,long generation);
    @Query("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE peerId=:peer AND bindingGeneration=:generation AND sessionId!='' AND state IN ('waiting','sending','failed') AND mediaId IN (SELECT mediaId FROM artifacts WHERE kind IN ('text','clipboard_image')) AND error!='old_device_replaced'") void dropPeerSessionClipboard(String peer,long generation);
    @Query("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE peerId=:peer AND state IN ('waiting','sending','failed') AND mediaId IN (SELECT mediaId FROM artifacts WHERE kind='text') AND error!='old_device_replaced'") void dropPeerText(String peer);
    @Query("SELECT COUNT(*) FROM outgoing WHERE peerId=:peer AND kind='text' AND orderId>:order AND state IN ('waiting','sending','failed')") int newerPeerText(String peer,long order);
    @Query("SELECT * FROM outgoing WHERE mediaId = :id ORDER BY CASE WHEN state='sending' THEN 0 WHEN state='waiting' THEN 1 WHEN state='failed' THEN 2 WHEN state='sent' THEN 3 ELSE 4 END,peerId LIMIT 1") Outgoing find(String id);
    @Query("SELECT * FROM outgoing WHERE state IN ('waiting', 'sending', 'failed') AND error!='old_device_replaced' ORDER BY orderId ASC, mediaId ASC") List<Outgoing> pending();
    // Choose active rows first, then fill the bounded projection; display in send order.
    @Query("SELECT * FROM (SELECT * FROM outgoing o WHERE peerId=(SELECT peerId FROM outgoing n WHERE n.mediaId=o.mediaId ORDER BY CASE WHEN state='sending' THEN 0 WHEN state='failed' THEN 1 WHEN state='waiting' THEN 2 WHEN state='sent' THEN 3 ELSE 4 END,peerId LIMIT 1) AND state IN ('waiting','sending','failed','sent') ORDER BY CASE WHEN state='sending' THEN 0 WHEN state='sent' THEN 2 ELSE 1 END, CASE WHEN state='sent' THEN -orderId ELSE orderId END, mediaId LIMIT 50) ORDER BY CASE WHEN state='sent' THEN 1 ELSE 0 END, CASE WHEN state='sent' THEN -orderId ELSE orderId END, mediaId") Flow<List<Outgoing>> recent();
    // Limit artifacts, not recipient rows: partial failures remain visible and
    // every selected artifact carries its complete frozen recipient set.
    @Query("SELECT * FROM outgoing WHERE mediaId IN (SELECT mediaId FROM outgoing GROUP BY mediaId HAVING SUM(CASE WHEN state IN ('waiting','sending','failed','sent') THEN 1 ELSE 0 END)>0 ORDER BY MIN(CASE WHEN state='sending' THEN 0 WHEN state IN ('waiting','failed') THEN 1 ELSE 2 END), CASE WHEN SUM(CASE WHEN state IN ('waiting','sending','failed') THEN 1 ELSE 0 END)>0 THEN MIN(orderId) ELSE -MAX(orderId) END,mediaId LIMIT 50) ORDER BY orderId,mediaId,peerId") Flow<List<Outgoing>> recentDeliveries();
    @Query("SELECT COUNT(DISTINCT mediaId) FROM outgoing WHERE state IN ('waiting','sending','failed') AND error!='old_device_replaced'") Flow<Integer> waitingCount();
    @Query("UPDATE deliveries SET state='waiting' WHERE state='sending'") void recoverSending();
    @Query("SELECT COUNT(*) FROM outgoing WHERE state='sending' AND (kind='text')=:text") int sendingInLane(boolean text);
    @Query("UPDATE deliveries SET state='waiting' WHERE state='sending' AND mediaId IN (SELECT mediaId FROM artifacts WHERE (kind='text')=:text)") void recoverSendingLane(boolean text);
    @Transaction default void recoverLane(boolean text) {
        recoverSendingLane(text);
        if(text) supersedeOlderText();
    }
    @Query("UPDATE artifacts SET uri='' WHERE automatic=0 AND kind!='text' AND uri NOT LIKE :privateUris") void forgetManualSources(String privateUris);
    @Query("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE state IN ('waiting','failed') AND mediaId IN (SELECT o.mediaId FROM outgoing o WHERE o.peerId=deliveries.peerId AND o.kind='text' AND o.orderId<(SELECT MAX(n.orderId) FROM outgoing n WHERE n.peerId=o.peerId AND n.kind='text' AND n.state IN ('waiting','failed'))) AND error!='old_device_replaced'") void supersedeOlderText();
    @Transaction default void recover() { recoverSending();supersedeOlderText(); }
    @Query("SELECT COUNT(*) FROM outgoing WHERE path=:path") int anyPath(String path);
    @Query("SELECT COUNT(*) FROM outgoing WHERE path = :path AND state IN ('waiting','sending','failed')") int hasPath(String path);
    @Query("SELECT COUNT(DISTINCT mediaId) FROM outgoing WHERE state IN ('waiting','sending','failed') AND error!='old_device_replaced'") int pendingCount();
    @Query("SELECT COALESCE((SELECT textTime FROM peer_outbound_clock WHERE peerId=:peer),0)") long peerTextTime(String peer);
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertPeerOutboundClock(PeerOutboundClock clock);
    @Query("UPDATE peer_outbound_clock SET textTime=MAX(textTime,:timestamp) WHERE peerId=:peer") void advancePeerText(String peer,long timestamp);
    @Transaction default void rememberPeerText(String peer,long timestamp) {
        PeerOutboundClock clock=new PeerOutboundClock();clock.peerId=peer;insertPeerOutboundClock(clock);advancePeerText(peer,timestamp);
    }
    @Query("DELETE FROM peer_outbound_clock WHERE peerId=:peer") void deletePeerOutboundClock(String peer);
    @Query("DELETE FROM peer_outbound_clock") void clearPeerOutboundClocks();
    @Query("SELECT COUNT(*) FROM receipts WHERE peerId=:peer AND origin='peer_settings'") int settingsReceiptCount(String peer);
    @Query("SELECT COALESCE((SELECT textTime FROM outbound_clock WHERE id=0),0)") long latestTextTime();
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertOutboundClock(OutboundClock clock);
    @Query("UPDATE outbound_clock SET textTime=MAX(textTime,:timestamp),lastOrder=MAX(lastOrder,:order) WHERE id=0") void advanceOutboundClock(long timestamp,long order);
    @Transaction default void rememberOutgoing(long timestamp,long order) { insertOutboundClock(new OutboundClock());advanceOutboundClock(timestamp,order); }
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertSeen(MediaSeen seen);
    @Query("SELECT COUNT(*) FROM media_seen WHERE mediaId=:id") int seen(String id);
    @Query("SELECT * FROM outgoing WHERE state NOT IN ('waiting','sending','failed') AND mediaId NOT IN (SELECT mediaId FROM outgoing WHERE state IN ('waiting','sending','failed')) AND mediaId NOT IN (SELECT mediaId FROM outgoing GROUP BY mediaId HAVING SUM(CASE WHEN state IN ('waiting','sending','failed') THEN 1 ELSE 0 END)=0 ORDER BY MAX(orderId) DESC,mediaId LIMIT 50)") List<Outgoing> evictedTerminal();
    @Query("DELETE FROM artifacts WHERE mediaId=:id") void deleteOutgoing(String id);
    @Query("SELECT COUNT(*) FROM outgoing WHERE kind='text' AND orderId>:order AND state IN ('waiting','sending','failed')") int newerText(long order);
    @Query("SELECT COALESCE((SELECT lastOrder FROM outbound_clock WHERE id=0),0)+1") long nextOrder();
    @Query("SELECT * FROM outgoing WHERE state IN ('waiting','sending','failed') AND error!='old_device_replaced' ORDER BY orderId ASC, mediaId ASC") Flow<List<Outgoing>> work();
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertReceipt(Receipt receipt);
    @Update void updateReceipt(Receipt receipt);
    @Query("SELECT * FROM receipts WHERE peerId=:peer AND transferId=:id") Receipt receipt(String peer,String id);
    @Query("SELECT COALESCE((SELECT lastApplied FROM clipboard_clock WHERE peerId=:peer),-1)") long lastApplied(String peer);
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertClock(ClipboardClock clock);
    @Query("UPDATE clipboard_clock SET lastApplied=MAX(lastApplied,:timestamp) WHERE peerId=:peer") void advanceClock(String peer,long timestamp);
    @Query("DELETE FROM receipts WHERE peerId=:peer AND transferId=:id") void deleteReceipt(String peer,String id);
    @Query("DELETE FROM receipts WHERE peerId=:peer AND origin IN ('clipboard','share') AND state='applied' AND rowid NOT IN (SELECT rowid FROM receipts WHERE peerId=:peer AND origin IN ('clipboard','share') AND state='applied' ORDER BY createdAt DESC,rowid DESC LIMIT 64)") void pruneReceipts(String peer);
    @Transaction default void retainClipboardReceipt(Receipt receipt) {
        insertReceipt(receipt);updateReceipt(receipt);pruneReceipts(receipt.peerId);
    }
    @Transaction default void completeReceipt(Receipt receipt) {
        ClipboardClock clock=new ClipboardClock();clock.peerId=receipt.peerId;clock.lastApplied=receipt.createdAt;
        insertClock(clock);advanceClock(receipt.peerId,receipt.createdAt);
        retainClipboardReceipt(receipt);
    }
    @Query("UPDATE deliveries SET state='superseded', revision=revision+1 WHERE mediaId IN (SELECT mediaId FROM artifacts WHERE kind='text') AND state IN ('waiting','failed') AND error!='old_device_replaced'") void supersedeText();
    @Query("SELECT * FROM outgoing") List<Outgoing> all();
    @Query("DELETE FROM receipts WHERE peerId=:peer AND origin='members' AND rowid NOT IN (SELECT rowid FROM receipts WHERE peerId=:peer AND origin='members' ORDER BY rowid DESC LIMIT 4096)") void pruneMembersReceipts(String peer);
    @Query("SELECT COUNT(*) FROM receipts") int receiptCount();
    @Query("DELETE FROM receipts") void clearReceiptRows();
    @Query("DELETE FROM clipboard_clock") void clearClipboardClocks();
    @Transaction default void clearReceipts() { clearReceiptRows();clearClipboardClocks();clearPeerOutboundClocks();clearReceivedFiles(); }
    @Query("DELETE FROM artifacts") void clearOutgoing();
    @Query("DELETE FROM media_seen") void clearSeen();
    @Query("DELETE FROM outbound_clock") void clearOutboundClock();
    @Transaction default void clear() {clearOutgoing();clearSeen();clearOutboundClock();clearPendingAdmissions();}
}
