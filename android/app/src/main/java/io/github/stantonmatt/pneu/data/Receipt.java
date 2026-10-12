package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.*;
@Entity(tableName="receipts", primaryKeys={"peerId","transferId"})
public class Receipt {
    @NonNull public String peerId="";
    @NonNull public String transferId="";
    @NonNull public String hash="";
    @NonNull public String origin="clipboard";
    @NonNull public String state="pending";
    public long createdAt;
}
