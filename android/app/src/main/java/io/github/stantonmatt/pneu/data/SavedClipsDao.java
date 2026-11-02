package io.github.stantonmatt.pneu.data;
import androidx.room.*;
import java.util.List;
@Dao public interface SavedClipsDao {
    @Query("SELECT * FROM saved_clips ORDER BY pinned DESC, CASE WHEN pinned THEN pinPosition ELSE position END DESC") List<SavedClip> all();
    @Query("SELECT * FROM history_setting WHERE id=0") SavedClipSetting setting();
    @Insert(onConflict=OnConflictStrategy.REPLACE) void put(SavedClip clip);
    @Insert(onConflict=OnConflictStrategy.REPLACE) void setting(SavedClipSetting setting);
    @Query("UPDATE saved_clips SET source=:fresh,sender=:name WHERE source=:old") void reattribute(String old,String fresh,String name);
    @Query("DELETE FROM saved_clips WHERE id=:id") void delete(String id);
    @Query("DELETE FROM saved_clips") void clear();
}
