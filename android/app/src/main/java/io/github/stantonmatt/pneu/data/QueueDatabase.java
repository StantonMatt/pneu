package io.github.stantonmatt.pneu.data;
import androidx.room.Database;
import androidx.room.RoomDatabase;
@Database(entities = {Outgoing.class, Receipt.class, ClipboardClock.class, OutboundClock.class, MediaSeen.class}, version = 7, exportSchema = false)
public abstract class QueueDatabase extends RoomDatabase { public abstract QueueDao queue(); }
