package io.github.stantonmatt.pneu.data;
import androidx.room.*;
@Database(entities={SavedClip.class,SavedClipSetting.class},version=2,exportSchema=false)
public abstract class SavedClipsDatabase extends RoomDatabase {
    public static final androidx.room.migration.Migration MIGRATION_1_2 = new androidx.room.migration.Migration(1,2) {
        @Override public void migrate(@androidx.annotation.NonNull androidx.sqlite.db.SupportSQLiteDatabase db) {
            db.execSQL("ALTER TABLE saved_clips ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0");
            db.execSQL("ALTER TABLE saved_clips ADD COLUMN pinPosition INTEGER NOT NULL DEFAULT 0");
        }
    };
    public abstract SavedClipsDao clips();
}
