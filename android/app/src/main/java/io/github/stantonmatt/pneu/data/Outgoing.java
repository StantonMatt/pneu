package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.Entity;
import androidx.room.PrimaryKey;
import androidx.room.Index;
import androidx.room.ColumnInfo;
@Entity(tableName = "outgoing", indices = {@Index(value = {"state", "createdAt"}), @Index(value = {"state", "orderId"})})
public class Outgoing {
    @PrimaryKey @NonNull public String mediaId = "";
    @NonNull public String transferId = "";
    @NonNull public String uri = "";
    @NonNull public String path = "";
    @NonNull public String fileName = "";
    @NonNull public String mime = "image/png";
    @NonNull public String hash = "";
    @NonNull public String state = "waiting";
    @NonNull public String error = "";
    @NonNull @ColumnInfo(defaultValue = "'screenshot'") public String kind = "screenshot";
    @NonNull @ColumnInfo(defaultValue = "'clipboard'") public String origin = "clipboard";
    @NonNull @ColumnInfo(defaultValue = "''") public String text = "";
    @ColumnInfo(defaultValue = "1") public boolean automatic = true;
    @ColumnInfo(defaultValue = "0") public boolean attempted;
    @ColumnInfo(defaultValue = "0") public boolean optimized;
    @ColumnInfo(defaultValue = "0") public long orderId;
    @NonNull @ColumnInfo(defaultValue = "''") public String sourceHash = "";
    @ColumnInfo(defaultValue = "0") public long revision;
    @NonNull @ColumnInfo(defaultValue = "''") public String rejectedCapabilities = "";
    public long size;
    public long createdAt;
    public long nextAttempt;
    public int mismatches;
}
