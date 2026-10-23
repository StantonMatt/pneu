package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.*;
/** Permanent file receipts plus the write-ahead publication journal. Never pruned
 * by the bounded text receipt window. URI ownership ends only at explicit unpair. */
@Entity(tableName="received_files",primaryKeys={"peerId","transferId"})
public class ReceivedFile {
    @NonNull public String peerId="";
    @NonNull public String transferId="";
    @NonNull public String metadata="";
    @NonNull public String uri="";
    @NonNull public String catalogVersion="";
    @NonNull public String fileName="";
    @NonNull public String mime="";
    @NonNull public String kind="";
    @NonNull public String senderName="";
    @NonNull public String state="writing";
    public long generation;
    public long size;
    public long receivedAt;
}
