package io.github.stantonmatt.pneu.features

import android.app.Application
import android.database.sqlite.SQLiteDatabase
import androidx.room.Room
import kotlinx.coroutines.*
import io.github.stantonmatt.pneu.data.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ResumeCatalogueTest {
    @Test fun versionTwelveMigrationRetainsReceiptsAndSpoolsAndCancelsSurviveDatabaseReopen()=runBlocking<Unit>(Dispatchers.IO) {
        val context=RuntimeEnvironment.getApplication();val name="resume-migration.db";context.deleteDatabase(name)
        fun open()=Room.databaseBuilder(context,QueueDatabase::class.java,name).addMigrations(QueueMigrations.fileResume).build()
        try {
            val first=open();first.queue().insert(Outgoing().apply {mediaId="payload";peerId="peer";transferId="id";size=42;automatic=false})
            first.queue().insertReceivedFile(ReceivedFile().apply {peerId="peer";transferId="done";state="complete";uri="public"})
            val view=first.openHelper.writableDatabase.query("SELECT sql FROM sqlite_master WHERE name='outgoing'").use {it.moveToFirst();it.getString(0)}
            first.close()
            SQLiteDatabase.openDatabase(context.getDatabasePath(name).absolutePath,null,SQLiteDatabase.OPEN_READWRITE).use {db->
                db.execSQL("DROP VIEW outgoing")
                // Robolectric's bundled SQLite predates DROP COLUMN. Rebuild
                // the two tables with their exact prior definitions instead.
                fun remove(table:String,removed:Set<String>) {
                    var sql=db.rawQuery("SELECT sql FROM sqlite_master WHERE name=?",arrayOf(table)).use {it.moveToFirst();it.getString(0)}
                    val columns=db.rawQuery("PRAGMA table_info($table)",null).use {cursor->buildList {while(cursor.moveToNext())if(cursor.getString(1) !in removed)add(cursor.getString(1))}}.joinToString(",")
                    val indexes=db.rawQuery("SELECT sql FROM sqlite_master WHERE type='index' AND tbl_name=? AND sql IS NOT NULL",arrayOf(table)).use {cursor->buildList {while(cursor.moveToNext())add(cursor.getString(0))}}
                    removed.forEach {column->sql=sql.replace(Regex("`$column` (TEXT|INTEGER) NOT NULL DEFAULT (''|0), "),"")}
                    db.execSQL("ALTER TABLE $table RENAME TO old_$table")
                    db.execSQL(sql);db.execSQL("INSERT INTO $table ($columns) SELECT $columns FROM old_$table")
                    db.execSQL("DROP TABLE old_$table");indexes.forEach {db.execSQL(it)}
                }
                remove("received_files",setOf("stagePath","stageIdentity","checkpointHash","checkpoint","progressWall","wallHighWater"))
                remove("deliveries",setOf("sentBytes","resumable"));db.execSQL("DROP TABLE pending_cancels")
                db.execSQL(view.replace("d.sentBytes AS sentBytes, d.resumable AS resumable, ",""));db.version=12
            }
            val second=open()
            try {
                val q=second.queue();val row=q.delivery("payload","peer")
                assertEquals(42L,row.size);assertEquals(0L,row.sentBytes);assertFalse(row.resumable)
                assertEquals("public",q.receivedFile("peer","done").uri)
                q.insertPendingCancel(PendingCancel().apply {peerId="peer";transferId="cancel";pin="pin";generation=7})
                q.insertReceivedFile(ReceivedFile().apply {peerId="peer";transferId="partial";stagePath="private";stageIdentity="inode";checkpoint=17;progressWall=100;wallHighWater=101})
            }finally {second.close()}
            val third=open()
            try {
                val q=third.queue();assertEquals(7L,q.pendingCancels("peer").single().generation)
                assertEquals(17L,q.receivedFile("peer","partial").checkpoint)
                q.clearPeer("peer");assertTrue(q.pendingCancels("peer").isEmpty());assertNull(q.receivedFile("peer","partial"))
            }finally {third.close()}
        }finally {context.deleteDatabase(name)}
    }
    @Test fun killedSenderRetainsProgressAndHonorsOfflineQueueWithoutTouchingOtherBindings()=runBlocking<Unit>(Dispatchers.IO) {
        val db=Room.inMemoryDatabaseBuilder(RuntimeEnvironment.getApplication(),QueueDatabase::class.java).build()
        try {
            val q=db.queue()
            for(peer in listOf("a","b"))q.insert(Outgoing().apply {mediaId="item";peerId=peer;bindingGeneration=7;state="sending";resumable=true;sentBytes=17})
            q.recoverResumable("a",7,false)
            assertEquals("failed",q.delivery("item","a").state);assertEquals("connection_lost",q.delivery("item","a").error)
            assertEquals(17L,q.delivery("item","a").sentBytes);assertEquals(Long.MAX_VALUE,q.delivery("item","a").nextAttempt)
            assertEquals("sending",q.delivery("item","b").state)
            q.recoverResumable("b",7,true);assertEquals("waiting",q.delivery("item","b").state);assertEquals(0L,q.delivery("item","b").nextAttempt)
        }finally {db.close()}
    }

    @Test fun staleCheckpointCannotRewriteReplacementBindingOrSpool()=runBlocking<Unit>(Dispatchers.IO) {
        val db=Room.inMemoryDatabaseBuilder(RuntimeEnvironment.getApplication(),QueueDatabase::class.java).build()
        try {
            val q=db.queue()
            val old=ReceivedFile().apply {peerId="peer";transferId="transfer";generation=7;stagePath="old";stageIdentity="inode"}
            q.insertReceivedFile(old)
            fun checkpoint(row:ReceivedFile)=q.checkpointReceivedFile(row.peerId,row.transferId,row.generation,row.stagePath,row.stageIdentity,17,"checksum",100,100)
            for(replace in listOf<(ReceivedFile)->Unit>({it.generation++},{it.stagePath="new"},{it.stageIdentity="new"})) {
                val replacement=q.receivedFile("peer","transfer");replace(replacement);q.updateReceivedFile(replacement)
                assertEquals(0,checkpoint(old));assertEquals(0L,q.receivedFile("peer","transfer").checkpoint)
                assertEquals(1,checkpoint(replacement));assertEquals(17L,q.receivedFile("peer","transfer").checkpoint)
                q.updateReceivedFile(old)
            }
        }finally {db.close()}
    }

}
