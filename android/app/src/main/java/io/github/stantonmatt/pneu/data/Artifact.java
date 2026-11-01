package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.*;
/** One staged payload, shared by the frozen recipient set. */
@Entity(tableName="artifacts")
public class Artifact {
    @PrimaryKey @NonNull public String mediaId = "";
    @NonNull public String transferId = "";
    @NonNull public String uri = "";
    @NonNull public String path = "";
    @NonNull public String fileName = "";
    @NonNull public String mime = "image/png";
    @NonNull public String hash = "";
    @NonNull @ColumnInfo(defaultValue = "'screenshot'") public String kind = "screenshot";
    @NonNull @ColumnInfo(defaultValue = "'clipboard'") public String origin = "clipboard";
    @NonNull @ColumnInfo(defaultValue = "''") public String text = "";
    @ColumnInfo(defaultValue = "1") public boolean automatic = true;
    @ColumnInfo(defaultValue = "0") public boolean optimized;
    @ColumnInfo(defaultValue = "0") public long orderId;
    @NonNull @ColumnInfo(defaultValue = "''") public String sourceHash = "";
    @androidx.room.ColumnInfo(defaultValue="0") public boolean sensitive;
    public long size;
    public long createdAt;
    @NonNull @ColumnInfo(defaultValue="''") public String hlc="";
    @NonNull @ColumnInfo(defaultValue="''") public String sourceDevice="";
    public static Artifact from(Outgoing row) {
        Artifact artifact=new Artifact();
        try { for(java.lang.reflect.Field field:Artifact.class.getFields())field.set(artifact,Outgoing.class.getField(field.getName()).get(row)); }
        catch(ReflectiveOperationException e) {throw new IllegalStateException(e);}
        return artifact;
    }
}
