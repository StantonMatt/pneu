package io.github.stantonmatt.pneu

import io.github.stantonmatt.pneu.data.*

import android.database.sqlite.SQLiteDatabase
import androidx.room.Room
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Test
import org.junit.Assert.*
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ClipboardHistoryMigrationDeviceTest {
    @Test fun versionElevenQueueRetainsPayloadsAndReceiptsAndDefaultsSensitivityToFalse() {
        val context=InstrumentationRegistry.getInstrumentation().targetContext
        val name="history-migration-test.db";context.deleteDatabase(name)
        fun open()=Room.databaseBuilder(context,QueueDatabase::class.java,name).addMigrations(QueueMigrations.sensitiveClips).build()
        try {
            val db=open()
            db.queue().insert(Outgoing().apply {mediaId="fixture";transferId="a".repeat(32);kind="text";text="preserved";automatic=false})
            db.queue().insertReceipt(Receipt().apply {peerId="PC";transferId="b".repeat(32);hash="hash";state="applied"})
            val viewSql=db.openHelper.writableDatabase.query("SELECT sql FROM sqlite_master WHERE type='view' AND name='outgoing'").use {cursor->check(cursor.moveToFirst());cursor.getString(0)}
            db.close()
            // Construct the previous schema with real SQLite. Room then validates
            // the migrated schema against its generated version-12 declaration.
            SQLiteDatabase.openDatabase(context.getDatabasePath(name).absolutePath,null,SQLiteDatabase.OPEN_READWRITE).use {old->
                old.execSQL("DROP VIEW outgoing")
                old.execSQL("ALTER TABLE artifacts DROP COLUMN sensitive")
                old.execSQL("ALTER TABLE receipts DROP COLUMN sensitive")
                old.execSQL(viewSql.replace(" a.sensitive AS sensitive,",""))
                old.version=11
            }
            val upgraded=open()
            try {
                val row=upgraded.queue().find("fixture");assertEquals("preserved",row.text);assertFalse(row.sensitive)
                assertFalse(upgraded.queue().receipt("PC","b".repeat(32)).sensitive)
                row.sensitive=true;upgraded.queue().replacePayload(row)
                assertTrue(upgraded.queue().find("fixture").sensitive)
            } finally {upgraded.close()}
        } finally {context.deleteDatabase(name)}
    }
}
