package io.github.stantonmatt.pneu.data;
import androidx.room.Database;
import androidx.room.RoomDatabase;
@Database(entities = {Artifact.class, Delivery.class, Receipt.class, ClipboardClock.class, OutboundClock.class, MediaSeen.class, PeerOutboundClock.class, PendingMediaAdmission.class, ReceivedFile.class}, views={Outgoing.class}, version = 12, exportSchema = false)
public abstract class QueueDatabase extends RoomDatabase { public abstract QueueDao queue(); }
