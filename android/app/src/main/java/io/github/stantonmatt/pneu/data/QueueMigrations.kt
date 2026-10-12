package io.github.stantonmatt.pneu.data

import androidx.room.migration.Migration
import androidx.sqlite.db.SupportSQLiteDatabase

object QueueMigrations {
    val outboundWindow=object : Migration(6,7) {
        override fun migrate(db:SupportSQLiteDatabase) {
            db.execSQL("CREATE TABLE IF NOT EXISTS outbound_clock (id INTEGER NOT NULL, textTime INTEGER NOT NULL, lastOrder INTEGER NOT NULL, PRIMARY KEY(id))")
            db.execSQL("INSERT INTO outbound_clock SELECT 0,COALESCE(MAX(CASE WHEN kind='text' THEN createdAt ELSE 0 END),0),COALESCE(MAX(orderId),0) FROM outgoing")
            db.execSQL("CREATE TABLE IF NOT EXISTS media_seen (mediaId TEXT NOT NULL, PRIMARY KEY(mediaId))")
            db.execSQL("INSERT INTO media_seen SELECT mediaId FROM outgoing WHERE automatic=1")
            // Keep private file paths until AppGraph startup prunes them with bytes.
        }
    }

    val clipboardWindow=object : Migration(5,6) {
        override fun migrate(db:SupportSQLiteDatabase) {
            db.execSQL("CREATE TABLE IF NOT EXISTS clipboard_clock (peerId TEXT NOT NULL, lastApplied INTEGER NOT NULL, PRIMARY KEY(peerId))")
            // Save the watermark before evicting IDs; ignored receipts do not advance it.
            db.execSQL("INSERT INTO clipboard_clock SELECT peerId,MAX(createdAt) FROM receipts WHERE state='applied' GROUP BY peerId")
            db.execSQL("DELETE FROM receipts WHERE state!='applied'")
            db.execSQL("DELETE FROM receipts WHERE rowid NOT IN (SELECT rid FROM (SELECT rowid AS rid,ROW_NUMBER() OVER (PARTITION BY peerId ORDER BY createdAt DESC,rowid DESC) AS position FROM receipts) WHERE position<=64)")
        }
    }
}
