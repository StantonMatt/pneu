package io.github.stantonmatt.pneu

import android.content.Intent
import androidx.sqlite.db.*
import androidx.sqlite.db.framework.FrameworkSQLiteOpenHelperFactory
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.TlsIdentity
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.ManualEntries
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class SecondReviewDeviceTest {
    private val c=InstrumentationRegistry.getInstrumentation().targetContext
    private val app=c.graph
    private suspend fun seed() {
        val raw="pneu://pair?v=1&name=Review&fp=${"a".repeat(64)}&addr=10.0.2.2%3A47821&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding();seed()
    }
    @After fun cleanup()=runBlocking<Unit> {app.connection.stop();app.forgetBinding()}
    @Test fun everyForgetAndLossRotatesTheWholeIdentityAndReloadPreservesIt()=runBlocking<Unit> {
        val old=app.preferences.flow.first();val cert=TlsIdentity(old.tlsAlias).certificate.encoded
        app.forgetBinding();val fresh=app.preferences.flow.first()
        assertNotEquals(old.deviceId,fresh.deviceId);assertNotEquals(old.tlsAlias,fresh.tlsAlias)
        assertFalse(cert.contentEquals(TlsIdentity(fresh.tlsAlias).certificate.encoded))
        assertEquals(TlsIdentity.aliasFor(fresh.deviceId),fresh.tlsAlias)
        Preferences(c).ensureIdentity();assertEquals(fresh.deviceId,app.preferences.flow.first().deviceId)
        assertArrayEquals(TlsIdentity(fresh.tlsAlias).certificate.encoded,TlsIdentity(Preferences(c).flow.first().tlsAlias).certificate.encoded)
        assertThrows(IllegalStateException::class.java) {TlsIdentity(old.tlsAlias)} // no silent recreation of retired keys
        seed()
        assertFalse(app.forgetBinding(null,fresh.deviceId)) // same identity gained a binding after an unpaired snapshot
        val beforeLoss=app.preferences.flow.first();val beforeLossCert=TlsIdentity(beforeLoss.tlsAlias).certificate.encoded
        assertTrue(app.loseBinding(beforeLoss.peer,beforeLoss.deviceId))
        val lost=app.preferences.flow.first();assertTrue(lost.pairingLost)
        assertNotEquals(beforeLoss.deviceId,lost.deviceId)
        assertFalse(beforeLossCert.contentEquals(TlsIdentity(lost.tlsAlias).certificate.encoded))
    }
    @Test fun legacyBoundIdentityIsPreservedButUnpairedLegacyIdentityRotates()=runBlocking<Unit> {
        val id=app.preferences.flow.first().deviceId
        val legacy=TlsIdentity(TlsIdentity.LEGACY_ALIAS,create=true).certificate.encoded
        app.preferences.setString("tlsAlias","")
        app.preferences.ensureIdentity()
        assertEquals(id,app.preferences.flow.first().deviceId);assertEquals(TlsIdentity.LEGACY_ALIAS,app.preferences.flow.first().tlsAlias)
        assertArrayEquals(legacy,TlsIdentity(app.preferences.flow.first().tlsAlias).certificate.encoded)
        app.forgetBinding();val unpaired=app.preferences.flow.first()
        app.preferences.setString("tlsAlias","") // old versions retained their fixed cert after forgetting
        app.preferences.ensureIdentity();val upgraded=app.preferences.flow.first()
        assertNotEquals(unpaired.deviceId,upgraded.deviceId)
        assertEquals(TlsIdentity.aliasFor(upgraded.deviceId),upgraded.tlsAlias)
        assertFalse(legacy.contentEquals(TlsIdentity(upgraded.tlsAlias).certificate.encoded))
        TlsIdentity.retire(TlsIdentity.aliasFor(id));TlsIdentity.retire(unpaired.tlsAlias)
    }
    @Test fun inflightTextIsNotSupersededButOnlyTheLatestUnsentValueSurvives()=runBlocking<Unit> {
        val sent=ManualEntries.text(app,"In flight","share")
        sent.state="sending";sent.attempted=true;assertTrue(app.updateDelivery(sent))
        val queued=ManualEntries.text(app,"Pending","share")
        val latest=ManualEntries.text(app,"Latest","clipboard")
        val revision=app.queue.find(sent.mediaId)!!.revision
        app.retry(sent.mediaId);assertEquals(revision,app.queue.find(sent.mediaId)!!.revision)
        assertEquals("sending",app.queue.find(sent.mediaId)!!.state)
        assertEquals("superseded",app.queue.find(queued.mediaId)!!.state)
        assertEquals(listOf(sent.mediaId,latest.mediaId),app.queue.pending().map {it.mediaId})
        // ACK for the old operation cannot remove the latest queued value.
        sent.state="sent";assertTrue(app.updateDelivery(sent));assertEquals("waiting",app.queue.find(latest.mediaId)!!.state)
        // A NACK/network retry for an in-flight older value drops it once newer work exists.
        latest.state="sending";latest.attempted=true;assertTrue(app.updateDelivery(latest))
        val newest=ManualEntries.text(app,"Newest","share")
        latest.state="failed";latest.error="clipboard_unavailable";assertTrue(app.updateDelivery(latest))
        assertEquals("superseded",app.queue.find(latest.mediaId)!!.state);assertEquals(listOf(newest.mediaId),app.queue.pending().map {it.mediaId})
        app.queue.recover();assertEquals(listOf(newest.mediaId),app.queue.pending().map {it.mediaId})
    }
    @Test fun terminalPayloadsAreBoundedWhileTimestampAndMediaDedupeSurvive()=runBlocking<Unit> {
        val text=ManualEntries.text(app,"x".repeat(16000),"share");text.createdAt=System.currentTimeMillis()+900000
        app.queue.rememberOutgoing(text.createdAt,text.orderId);text.state="sent";app.updateDelivery(text)
        val watermark=text.createdAt
        val paths=mutableListOf<File>();val ids=mutableListOf<String>()
        repeat(110) {index->
            val file=File(c.cacheDir,"shared-recent/retention-$index.bin").apply {parentFile!!.mkdirs();writeText("fixture")};paths+=file
            val row=Outgoing().apply {mediaId="media-fixture-$index";transferId=randomId();automatic=true;path=file.path;state=if(index%2==0)"sent" else "skipped";createdAt=index.toLong()}
            ids+=row.mediaId;assertTrue(app.enqueueFixture(row))
        }
        assertEquals(50,app.queue.all().size);assertNull(app.queue.find(text.mediaId));assertEquals(watermark,app.queue.latestTextTime())
        assertTrue(paths.take(60).all {!it.exists()});assertTrue(ids.all {app.queue.seen(it)==1})
        assertFalse(app.enqueueFixture(Outgoing().apply {mediaId=ids.first();transferId=randomId();automatic=true}))
        val next=ManualEntries.text(app,"After eviction","share");assertEquals(watermark+1,next.createdAt)
        assertEquals(112L,next.orderId)
        repeat(100) {ManualEntries.text(app,"q".repeat(16000),"clipboard")}
        assertEquals(1,app.queue.pending().count {it.kind=="text"})
        assertEquals(50,app.queue.all().count {it.state !in setOf("waiting","sending","failed")})
        app.forgetBinding();assertEquals(0,app.queue.seen(ids.first()));assertEquals(0,app.queue.latestTextTime());assertTrue(app.queue.all().isEmpty())
    }
    @Test fun recoveredTextKeepsOnlyNewestAndPruningNeverDeletesPendingFiles()=runBlocking<Unit> {
        val older=ManualEntries.text(app,"Old sent operation","share");older.state="sending";app.updateDelivery(older)
        val newer=ManualEntries.text(app,"Pending on crash","share")
        app.queue.recover();assertEquals("superseded",app.queue.find(older.mediaId)!!.state)
        assertEquals(listOf(newer.mediaId),app.queue.pending().map {it.mediaId})
        val pending=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="file";automatic=false}
        assertTrue(app.enqueueFixture(pending))
        repeat(100) {index->app.enqueueFixture(Outgoing().apply {mediaId="prune-$index";transferId=randomId();kind="file";state="discarded";automatic=false})}
        assertNotNull(app.queue.find(pending.mediaId));assertNotNull(app.queue.find(newer.mediaId));assertEquals(52,app.queue.all().size)
    }
    @Test fun migrationPreservesOutgoingWatermarksAndCompactAutomaticIds() {
        c.deleteDatabase("review-outbound-migration.db")
        val helper=FrameworkSQLiteOpenHelperFactory().create(SupportSQLiteOpenHelper.Configuration.builder(c).name("review-outbound-migration.db").callback(object:SupportSQLiteOpenHelper.Callback(1) {
            override fun onCreate(db:SupportSQLiteDatabase) {
                db.execSQL("CREATE TABLE outgoing (mediaId TEXT NOT NULL,kind TEXT NOT NULL,createdAt INTEGER NOT NULL,orderId INTEGER NOT NULL,automatic INTEGER NOT NULL)")
                db.execSQL("INSERT INTO outgoing VALUES ('text','text',999999,500,0),('media','screenshot',10,501,1)")
                QueueMigrations.outboundWindow.migrate(db)
            }
            override fun onUpgrade(db:SupportSQLiteDatabase,oldVersion:Int,newVersion:Int) {}
        }).build())
        try {
            val db=helper.writableDatabase
            db.query("SELECT textTime,lastOrder FROM outbound_clock").use {assertTrue(it.moveToFirst());assertEquals(999999L,it.getLong(0));assertEquals(501L,it.getLong(1));assertFalse(it.moveToNext())}
            db.query("SELECT mediaId FROM media_seen").use {assertTrue(it.moveToFirst());assertEquals("media",it.getString(0));assertFalse(it.moveToNext())}
            db.query("SELECT COUNT(*) FROM outgoing").use {assertTrue(it.moveToFirst());assertEquals(2,it.getInt(0))} // startup unlinks evicted paths, not migration
        } finally {helper.close();c.deleteDatabase("review-outbound-migration.db")}
    }

}
