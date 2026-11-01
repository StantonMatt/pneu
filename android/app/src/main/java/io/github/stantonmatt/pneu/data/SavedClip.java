package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.*;
/** User-visible history, independent of the clipboard echo fingerprint window. */
@Entity(tableName="saved_clips")
public class SavedClip {
    @PrimaryKey @NonNull public String id="";
    @NonNull public String text="";
    @NonNull public String image="";
    @NonNull public String thumbnail="";
    @NonNull public String mime="";
    /** Empty for this phone; otherwise the sender's name at receipt time. */
    @NonNull public String sender="";
    @NonNull public String source="";
    public long copiedAt;
    public long position;
}
