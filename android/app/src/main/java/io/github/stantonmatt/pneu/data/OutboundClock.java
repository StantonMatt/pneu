package io.github.stantonmatt.pneu.data;
import androidx.room.Entity;
import androidx.room.PrimaryKey;
@Entity(tableName="outbound_clock")
public class OutboundClock {
    @PrimaryKey public int id=0;
    public long textTime;
    public long lastOrder;
}
