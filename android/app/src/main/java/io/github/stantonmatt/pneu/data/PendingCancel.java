package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.*;
/** Survives a torn frame or process death; scoped to the exact trust binding. */
@Entity(tableName="pending_cancels",primaryKeys={"peerId","transferId"})
public class PendingCancel {
    @NonNull public String peerId="";
    @NonNull public String transferId="";
    @NonNull public String pin="";
    public long generation;
}
