package io.github.stantonmatt.pneu.data

import android.app.Application
import android.database.sqlite.SQLiteDatabase
import androidx.room.Room
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class SavedClipsMigrationTest {
    @Test fun versionOneRowsAndSettingSurviveAndPinsPersistAfterReopen() {
        val context=RuntimeEnvironment.getApplication();val name="saved-clips-v1-test.db"
        context.deleteDatabase(name)
        val path=context.getDatabasePath(name);path.parentFile!!.mkdirs()
        SQLiteDatabase.openOrCreateDatabase(path,null).use {db->
            db.execSQL("CREATE TABLE saved_clips (id TEXT NOT NULL PRIMARY KEY, text TEXT NOT NULL, image TEXT NOT NULL, thumbnail TEXT NOT NULL, mime TEXT NOT NULL, sender TEXT NOT NULL, source TEXT NOT NULL, copiedAt INTEGER NOT NULL, position INTEGER NOT NULL)")
            db.execSQL("CREATE TABLE history_setting (id INTEGER NOT NULL PRIMARY KEY, enabled INTEGER NOT NULL)")
            db.execSQL("INSERT INTO history_setting VALUES (0,1)")
            db.execSQL("INSERT INTO saved_clips VALUES ('text','saved text','','','','Studio PC','source',123,7)")
            db.execSQL("INSERT INTO saved_clips VALUES ('image','','payload.png','thumb.png','image/png','Mac','origin',124,8)")
            db.version=1
        }
        fun open()=Room.databaseBuilder(context,SavedClipsDatabase::class.java,name).addMigrations(SavedClipsDatabase.MIGRATION_1_2).allowMainThreadQueries().build()
        try {
            val db=open()
            try {
                val rows=db.clips().all();assertEquals(listOf("image","text"),rows.map {it.id})
                assertTrue(db.clips().setting().enabled);assertTrue(rows.none {it.pinned})
                assertEquals("payload.png",rows.first().image);assertEquals("thumb.png",rows.first().thumbnail)
                assertEquals("Studio PC",rows.last().sender);assertEquals("source",rows.last().source)
                assertEquals(123L,rows.last().copiedAt);assertEquals(7L,rows.last().position)
                db.clips().put(rows.last().apply {pinned=true;pinPosition=1})
            } finally {db.close()}
            val reopened=open()
            try {assertEquals("text",reopened.clips().all().first().id);assertTrue(reopened.clips().all().first().pinned)}
            finally {reopened.close()}
        } finally {context.deleteDatabase(name)}
    }
}
