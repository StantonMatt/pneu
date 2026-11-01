package io.github.stantonmatt.pneu.data;
import androidx.room.*;
@Database(entities={SavedClip.class,SavedClipSetting.class},version=1,exportSchema=false)
public abstract class SavedClipsDatabase extends RoomDatabase {
    public abstract SavedClipsDao clips();
}
