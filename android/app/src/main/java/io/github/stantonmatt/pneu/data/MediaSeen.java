package io.github.stantonmatt.pneu.data;
import androidx.room.Entity;
import androidx.room.PrimaryKey;
import androidx.annotation.NonNull;
/** Compact permanent media-ID tombstone; no user payload or provider metadata. */
@Entity(tableName="media_seen")
public class MediaSeen {
    @PrimaryKey @NonNull public String mediaId="";
}
