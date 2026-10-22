package io.github.stantonmatt.pneu.data

import androidx.room.migration.Migration
import androidx.sqlite.db.SupportSQLiteDatabase

object QueueMigrations {
    val pendingAdmissions=object : Migration(9,10) {
        override fun migrate(db:SupportSQLiteDatabase) {
            db.execSQL("CREATE TABLE IF NOT EXISTS pending_media_admissions (mediaId TEXT NOT NULL, snapshot TEXT NOT NULL, PRIMARY KEY(mediaId))")
            db.execSQL("ALTER TABLE deliveries ADD COLUMN sessionId TEXT NOT NULL DEFAULT ''")
            // An upgrade has no surviving authenticated session for legacy rows.
            // They cannot acquire authority merely because a computer reconnects.
            db.execSQL("UPDATE deliveries SET state='superseded',revision=revision+1 WHERE state IN ('waiting','sending','failed') AND mediaId IN (SELECT mediaId FROM artifacts WHERE kind IN ('text','clipboard_image'))")
            db.execSQL("DROP VIEW outgoing")
            db.execSQL("CREATE VIEW `outgoing` AS ${OutgoingView.SQL}")
        }
    }
    val peerWatermarks=object:Migration(8,9) {
        override fun migrate(db:SupportSQLiteDatabase) {
            db.execSQL("CREATE TABLE peer_outbound_clock (peerId TEXT NOT NULL PRIMARY KEY,textTime INTEGER NOT NULL)")
            db.execSQL("INSERT INTO peer_outbound_clock SELECT peerId,MAX(MAX(CASE WHEN kind='text' THEN createdAt ELSE 0 END),COALESCE((SELECT textTime FROM outbound_clock WHERE id=0),0)) FROM outgoing GROUP BY peerId")
            db.execSQL("CREATE INDEX IF NOT EXISTS index_receipts_peerId_origin ON receipts(peerId,origin)")
        }
    }

    /** Room owns the SQL transaction. The unknown owner is quarantined until
     * DataStore supplies the ORIGINAL legacy binding, never the current first PC. */
    val recipients=object:Migration(7,8) {
        override fun migrate(db:SupportSQLiteDatabase) {
            db.execSQL("ALTER TABLE outgoing RENAME TO legacy_outgoing")
            db.execSQL("CREATE TABLE artifacts (mediaId TEXT NOT NULL PRIMARY KEY,transferId TEXT NOT NULL,uri TEXT NOT NULL,path TEXT NOT NULL,fileName TEXT NOT NULL,mime TEXT NOT NULL,hash TEXT NOT NULL,kind TEXT NOT NULL DEFAULT 'screenshot',origin TEXT NOT NULL DEFAULT 'clipboard',text TEXT NOT NULL DEFAULT '',automatic INTEGER NOT NULL DEFAULT 1,optimized INTEGER NOT NULL DEFAULT 0,orderId INTEGER NOT NULL DEFAULT 0,sourceHash TEXT NOT NULL DEFAULT '',size INTEGER NOT NULL,createdAt INTEGER NOT NULL,hlc TEXT NOT NULL DEFAULT '',sourceDevice TEXT NOT NULL DEFAULT '')")
            db.execSQL("CREATE TABLE deliveries (mediaId TEXT NOT NULL,peerId TEXT NOT NULL,bindingGeneration INTEGER NOT NULL DEFAULT 0,state TEXT NOT NULL,error TEXT NOT NULL,attempted INTEGER NOT NULL DEFAULT 0,revision INTEGER NOT NULL DEFAULT 0,rejectedCapabilities TEXT NOT NULL DEFAULT '',nextAttempt INTEGER NOT NULL,mismatches INTEGER NOT NULL,PRIMARY KEY(mediaId,peerId),FOREIGN KEY(mediaId) REFERENCES artifacts(mediaId) ON UPDATE NO ACTION ON DELETE CASCADE)")
            db.execSQL("CREATE INDEX index_deliveries_peerId_state ON deliveries(peerId,state)")
            db.execSQL("INSERT INTO artifacts SELECT mediaId,transferId,uri,path,fileName,mime,hash,kind,origin,text,automatic,optimized,orderId,sourceHash,size,createdAt,'','' FROM legacy_outgoing")
            db.execSQL("INSERT INTO deliveries SELECT mediaId,'',0,state,error,attempted,revision,rejectedCapabilities,nextAttempt,mismatches FROM legacy_outgoing")
            db.execSQL("DROP TABLE legacy_outgoing")
            db.execSQL("ALTER TABLE receipts ADD COLUMN hlc TEXT NOT NULL DEFAULT ''")
            db.execSQL("ALTER TABLE receipts ADD COLUMN sourceDevice TEXT NOT NULL DEFAULT ''")
            db.execSQL("CREATE VIEW `outgoing` AS SELECT a.mediaId AS mediaId, a.transferId AS transferId, a.uri AS uri, a.path AS path, a.fileName AS fileName, a.mime AS mime, a.hash AS hash, a.kind AS kind, a.origin AS origin, a.text AS text, a.automatic AS automatic, a.optimized AS optimized, a.orderId AS orderId, a.sourceHash AS sourceHash, a.size AS size, a.createdAt AS createdAt, a.hlc AS hlc, a.sourceDevice AS sourceDevice, EXISTS(SELECT 1 FROM deliveries used WHERE used.mediaId=a.mediaId AND used.attempted=1) AS payloadAttempted, d.peerId AS peerId, d.bindingGeneration AS bindingGeneration, d.state AS state, d.error AS error, d.attempted AS attempted, d.revision AS revision, d.rejectedCapabilities AS rejectedCapabilities, d.nextAttempt AS nextAttempt, d.mismatches AS mismatches FROM artifacts a JOIN deliveries d ON a.mediaId=d.mediaId")
        }
    }


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
