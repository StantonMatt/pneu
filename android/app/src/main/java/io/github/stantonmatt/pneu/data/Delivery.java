package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.*;
@Entity(tableName="deliveries",primaryKeys={"mediaId","peerId"},indices={@Index(value={"peerId","state"})},
    foreignKeys=@ForeignKey(entity=Artifact.class,parentColumns="mediaId",childColumns="mediaId",onDelete=ForeignKey.CASCADE))
public class Delivery {
    @NonNull public String mediaId="";
    @NonNull public String peerId="";
    @ColumnInfo(defaultValue="0") public long bindingGeneration;
    @NonNull @ColumnInfo(defaultValue="''") public String sessionId="";
    @NonNull public String state = "waiting";
    @NonNull public String error = "";
    @ColumnInfo(defaultValue = "0") public boolean attempted;
    @ColumnInfo(defaultValue = "0") public long revision;
    @NonNull @ColumnInfo(defaultValue = "''") public String rejectedCapabilities = "";
    public long nextAttempt;
    public int mismatches;
    public static Delivery from(Outgoing row) {
        Delivery delivery=new Delivery();
        try { for(java.lang.reflect.Field field:Delivery.class.getFields())field.set(delivery,Outgoing.class.getField(field.getName()).get(row)); }
        catch(ReflectiveOperationException e) {throw new IllegalStateException(e);}
        return delivery;
    }
}
