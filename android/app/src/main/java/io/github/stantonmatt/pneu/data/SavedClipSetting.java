package io.github.stantonmatt.pneu.data;
import androidx.room.*;
@Entity(tableName="history_setting")
public class SavedClipSetting {
    @PrimaryKey public int id=0;
    public boolean enabled;
}
