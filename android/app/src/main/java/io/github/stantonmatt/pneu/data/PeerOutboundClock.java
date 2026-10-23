package io.github.stantonmatt.pneu.data;
import androidx.annotation.NonNull;
import androidx.room.*;
/** A durable watermark per recipient, retained after payload eviction. */
@Entity(tableName="peer_outbound_clock")
public class PeerOutboundClock {
    @PrimaryKey @NonNull public String peerId="";
    public long textTime;
}
