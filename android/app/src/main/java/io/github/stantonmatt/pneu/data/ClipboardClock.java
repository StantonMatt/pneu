package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.*;
@Entity(tableName="clipboard_clock")
public class ClipboardClock {
    @PrimaryKey @NonNull public String peerId="";
    public long lastApplied=-1;
}
