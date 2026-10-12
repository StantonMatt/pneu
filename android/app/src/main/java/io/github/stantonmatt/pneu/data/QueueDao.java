package io.github.stantonmatt.pneu.data;
import androidx.room.*;
import java.util.List;
import kotlinx.coroutines.flow.Flow;
@Dao public interface QueueDao {
    @Insert(onConflict = OnConflictStrategy.IGNORE) long insert(Outgoing row);
    @Update void update(Outgoing row);
    @Query("SELECT * FROM outgoing WHERE mediaId = :id") Outgoing find(String id);
    @Query("SELECT * FROM outgoing WHERE state IN ('waiting', 'sending', 'failed') ORDER BY orderId ASC, mediaId ASC") List<Outgoing> pending();
    // Choose active rows first, then fill the bounded projection; display in send order.
    @Query("SELECT * FROM (SELECT * FROM outgoing WHERE state IN ('waiting','sending','failed','sent') ORDER BY CASE WHEN state='sending' THEN 0 WHEN state='sent' THEN 2 ELSE 1 END, CASE WHEN state='sent' THEN -orderId ELSE orderId END, mediaId LIMIT 50) ORDER BY CASE WHEN state='sent' THEN 1 ELSE 0 END, CASE WHEN state='sent' THEN -orderId ELSE orderId END, mediaId") Flow<List<Outgoing>> recent();
    @Query("SELECT COUNT(*) FROM outgoing WHERE state IN ('waiting','sending','failed')") Flow<Integer> waitingCount();
    @Query("UPDATE outgoing SET state='waiting' WHERE state='sending'") void recoverSending();
    @Query("SELECT COUNT(*) FROM outgoing WHERE state='sending' AND (kind='text')=:text") int sendingInLane(boolean text);
    @Query("UPDATE outgoing SET state='waiting' WHERE state='sending' AND (kind='text')=:text") void recoverSendingLane(boolean text);
    @Transaction default void recoverLane(boolean text) {
        recoverSendingLane(text);
        if(text) supersedeOlderText();
    }
    @Query("UPDATE outgoing SET uri='' WHERE automatic=0 AND kind!='text' AND uri NOT LIKE :privateUris") void forgetManualSources(String privateUris);
    @Query("UPDATE outgoing SET state='superseded',revision=revision+1 WHERE kind='text' AND state IN ('waiting','failed') AND orderId<(SELECT MAX(orderId) FROM outgoing WHERE kind='text' AND state IN ('waiting','failed'))") void supersedeOlderText();
    @Transaction default void recover() { recoverSending();supersedeOlderText(); }
    @Query("SELECT COUNT(*) FROM outgoing WHERE path=:path") int anyPath(String path);
    @Query("SELECT COUNT(*) FROM outgoing WHERE path = :path AND state IN ('waiting','sending','failed')") int hasPath(String path);
    @Query("SELECT COUNT(*) FROM outgoing WHERE state IN ('waiting','sending','failed')") int pendingCount();
    @Query("SELECT COALESCE((SELECT textTime FROM outbound_clock WHERE id=0),0)") long latestTextTime();
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertOutboundClock(OutboundClock clock);
    @Query("UPDATE outbound_clock SET textTime=MAX(textTime,:timestamp),lastOrder=MAX(lastOrder,:order) WHERE id=0") void advanceOutboundClock(long timestamp,long order);
    @Transaction default void rememberOutgoing(long timestamp,long order) { insertOutboundClock(new OutboundClock());advanceOutboundClock(timestamp,order); }
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertSeen(MediaSeen seen);
    @Query("SELECT COUNT(*) FROM media_seen WHERE mediaId=:id") int seen(String id);
    @Query("SELECT * FROM outgoing WHERE state NOT IN ('waiting','sending','failed') AND mediaId NOT IN (SELECT mediaId FROM outgoing WHERE state NOT IN ('waiting','sending','failed') ORDER BY orderId DESC LIMIT 50)") List<Outgoing> evictedTerminal();
    @Query("DELETE FROM outgoing WHERE mediaId=:id") void deleteOutgoing(String id);
    @Query("SELECT COUNT(*) FROM outgoing WHERE kind='text' AND orderId>:order AND state IN ('waiting','sending','failed')") int newerText(long order);
    @Query("SELECT COALESCE((SELECT lastOrder FROM outbound_clock WHERE id=0),0)+1") long nextOrder();
    @Query("SELECT * FROM outgoing WHERE state IN ('waiting','sending','failed') ORDER BY orderId ASC, mediaId ASC") Flow<List<Outgoing>> work();
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertReceipt(Receipt receipt);
    @Update void updateReceipt(Receipt receipt);
    @Query("SELECT * FROM receipts WHERE peerId=:peer AND transferId=:id") Receipt receipt(String peer,String id);
    @Query("SELECT COALESCE((SELECT lastApplied FROM clipboard_clock WHERE peerId=:peer),-1)") long lastApplied(String peer);
    @Insert(onConflict=OnConflictStrategy.IGNORE) long insertClock(ClipboardClock clock);
    @Query("UPDATE clipboard_clock SET lastApplied=MAX(lastApplied,:timestamp) WHERE peerId=:peer") void advanceClock(String peer,long timestamp);
    @Query("DELETE FROM receipts WHERE peerId=:peer AND transferId=:id") void deleteReceipt(String peer,String id);
    @Query("DELETE FROM receipts WHERE peerId=:peer AND rowid NOT IN (SELECT rowid FROM receipts WHERE peerId=:peer ORDER BY createdAt DESC,rowid DESC LIMIT 64)") void pruneReceipts(String peer);
    @Transaction default void completeReceipt(Receipt receipt) {
        ClipboardClock clock=new ClipboardClock();clock.peerId=receipt.peerId;clock.lastApplied=receipt.createdAt;
        insertClock(clock);advanceClock(receipt.peerId,receipt.createdAt);
        insertReceipt(receipt);pruneReceipts(receipt.peerId);
    }
    @Query("UPDATE outgoing SET state='superseded', revision=revision+1 WHERE kind='text' AND state IN ('waiting','failed')") void supersedeText();
    @Query("SELECT * FROM outgoing") List<Outgoing> all();
    @Query("SELECT COUNT(*) FROM receipts") int receiptCount();
    @Query("DELETE FROM receipts") void clearReceiptRows();
    @Query("DELETE FROM clipboard_clock") void clearClipboardClocks();
    @Transaction default void clearReceipts() { clearReceiptRows();clearClipboardClocks(); }
    @Query("DELETE FROM outgoing") void clearOutgoing();
    @Query("DELETE FROM media_seen") void clearSeen();
    @Query("DELETE FROM outbound_clock") void clearOutboundClock();
    @Transaction default void clear() {clearOutgoing();clearSeen();clearOutboundClock();}
}
