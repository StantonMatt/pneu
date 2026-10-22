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
    @Test fun forgetAndLossPreservePhoneIdentityAndReloadPreservesIt()=runBlocking<Unit> {
        val old=app.preferences.read();val cert=TlsIdentity(old.tlsAlias).certificate.encoded
        val confirmed=ConfirmedBinding.from(old)
        assertTrue(app.forgetBinding(confirmed=confirmed))
        val fresh=app.preferences.read()
        assertTrue(fresh.devices.isEmpty());assertEquals(old.deviceId,fresh.deviceId);assertEquals(old.tlsAlias,fresh.tlsAlias)
        assertArrayEquals(cert,TlsIdentity(fresh.tlsAlias).certificate.encoded)
        Preferences(c).ensureIdentity();assertEquals(fresh.deviceId,app.preferences.read().deviceId)
        assertArrayEquals(cert,TlsIdentity(Preferences(c).read().tlsAlias).certificate.encoded)
        seed()
        assertFalse(app.forgetBinding(confirmed=confirmed)) // Re-pairing advances this peer's generation.
        val beforeLoss=app.preferences.read()
        assertTrue(app.loseBinding(beforeLoss.peer,beforeLoss.deviceId))
        val lost=app.preferences.read();assertTrue(lost.pairingLost)
        assertTrue(lost.devices.single().pairingLost)
        assertNull(DeliveryBinding.from(lost))
        assertEquals(old.deviceId,lost.deviceId);assertEquals(old.tlsAlias,lost.tlsAlias)
        assertArrayEquals(cert,TlsIdentity(lost.tlsAlias).certificate.encoded)
    }
    @Test fun legacyIdentityAliasIsPreservedForPairedAndUnpairedPhones()=runBlocking<Unit> {
        val original=app.preferences.read()
        val legacy=TlsIdentity(TlsIdentity.LEGACY_ALIAS,create=true).certificate.encoded
        try {
            app.preferences.setString("tlsAlias","");app.preferences.ensureIdentity()
            val paired=app.preferences.read()
            assertEquals(original.deviceId,paired.deviceId);assertEquals(TlsIdentity.LEGACY_ALIAS,paired.tlsAlias)
            assertArrayEquals(legacy,TlsIdentity(paired.tlsAlias).certificate.encoded)
            app.forgetBinding();app.preferences.setString("tlsAlias","");app.preferences.ensureIdentity()
            val unpaired=app.preferences.read()
            assertTrue(unpaired.devices.isEmpty());assertEquals(original.deviceId,unpaired.deviceId)
            assertEquals(TlsIdentity.LEGACY_ALIAS,unpaired.tlsAlias)
            assertArrayEquals(legacy,TlsIdentity(unpaired.tlsAlias).certificate.encoded)
        } finally {app.preferences.setString("tlsAlias",original.tlsAlias)}
    }
    @Test fun inflightTextIsNotSupersededButOnlyTheLatestUnsentValueSurvives()=runBlocking<Unit> {
        val sent=DeviceFixture.text(app,"In flight","share")
        sent.state="sending";sent.attempted=true;assertTrue(app.updateDelivery(sent))
        val queued=DeviceFixture.text(app,"Pending","share")
        val latest=DeviceFixture.text(app,"Latest","clipboard")
        val revision=app.queue.find(sent.mediaId)!!.revision
        app.retry(sent.mediaId);assertEquals(revision,app.queue.find(sent.mediaId)!!.revision)
        assertEquals("sending",app.queue.find(sent.mediaId)!!.state)
        assertEquals("superseded",app.queue.find(queued.mediaId)!!.state)
        assertEquals(listOf(sent.mediaId,latest.mediaId),app.queue.pending().map {it.mediaId})
        // ACK for the old operation cannot remove the latest queued value.
        sent.state="sent";assertTrue(app.updateDelivery(sent));assertEquals("waiting",app.queue.find(latest.mediaId)!!.state)
        // A NACK/network retry for an in-flight older value drops it once newer work exists.
        latest.state="sending";latest.attempted=true;assertTrue(app.updateDelivery(latest))
        val newest=DeviceFixture.text(app,"Newest","share")
        latest.state="failed";latest.error="clipboard_unavailable";assertTrue(app.updateDelivery(latest))
        assertEquals("superseded",app.queue.find(latest.mediaId)!!.state);assertEquals(listOf(newest.mediaId),app.queue.pending().map {it.mediaId})
        app.queue.recover();assertEquals(listOf(newest.mediaId),app.queue.pending().map {it.mediaId})
    }
    @Test fun terminalPayloadsAreBoundedWhileTimestampAndMediaDedupeSurvive()=runBlocking<Unit> {
        val text=DeviceFixture.text(app,"x".repeat(16000),"share");text.createdAt=System.currentTimeMillis()+900000
        app.queue.rememberOutgoing(text.createdAt,text.orderId);text.state="sent";app.updateDelivery(text)
        val watermark=text.createdAt
        val paths=mutableListOf<File>();val ids=mutableListOf<String>()
        repeat(110) {index->
            val file=File(c.cacheDir,"shared-recent/retention-$index.bin")
            if(index%2==0) {file.parentFile!!.mkdirs();file.writeText("fixture");paths+=file}
            val row=Outgoing().apply {mediaId="media-fixture-$index";transferId=randomId();automatic=true;path=file.path;state=if(index%2==0)"sent" else "skipped";createdAt=index.toLong()}
            ids+=row.mediaId;assertTrue(app.enqueueFixture(row))
        }
        assertEquals(50,app.queue.all().size);assertNull(app.queue.find(text.mediaId));assertEquals(watermark,app.queue.latestTextTime())
        assertTrue(paths.take(5).all {!it.exists()});assertTrue(ids.all {app.queue.seen(it)==1})
        assertFalse(app.enqueueFixture(Outgoing().apply {mediaId=ids.first();transferId=randomId();automatic=true}))
        val next=DeviceFixture.text(app,"After eviction","share");assertEquals(watermark+1,next.createdAt)
        assertEquals(57L,next.orderId)
        repeat(100) {DeviceFixture.text(app,"q".repeat(16000),"clipboard")}
        assertEquals(1,app.queue.pending().count {it.kind=="text"})
        assertEquals(50,app.queue.all().count {it.state !in setOf("waiting","sending","failed")})
        val lastText=app.queue.latestTextTime();app.forgetBinding();assertEquals(1,app.queue.seen(ids.first()));assertEquals(lastText,app.queue.latestTextTime());assertTrue(app.queue.all().isEmpty())
    }
    @Test fun recoveredTextKeepsOnlyNewestAndPruningNeverDeletesPendingFiles()=runBlocking<Unit> {
        val older=DeviceFixture.text(app,"Old sent operation","share");older.state="sending";app.updateDelivery(older)
        val newer=DeviceFixture.text(app,"Pending on crash","share")
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
