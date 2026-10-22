package io.github.stantonmatt.pneu.data;

import androidx.annotation.NonNull;
import androidx.room.Entity;
import androidx.room.PrimaryKey;
import com.google.gson.Gson;

/** Discovery journal, before opening a media provider. Never a new operation on retry. */
@Entity(tableName="pending_media_admissions")
public class PendingMediaAdmission {
    @PrimaryKey @NonNull public String mediaId="";
    @NonNull public String snapshot="";
    public static PendingMediaAdmission from(Outgoing row) {
        PendingMediaAdmission pending=new PendingMediaAdmission();
        pending.mediaId=row.mediaId;pending.snapshot=new Gson().toJson(row);return pending;
    }
    public Outgoing row() {return new Gson().fromJson(snapshot,Outgoing.class);}
}
