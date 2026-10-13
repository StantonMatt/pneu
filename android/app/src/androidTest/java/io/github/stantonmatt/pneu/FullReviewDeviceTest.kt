package io.github.stantonmatt.pneu

import android.Manifest
import android.app.PendingIntent
import android.content.*
import android.graphics.Bitmap
import android.net.Uri
import android.provider.MediaStore
import androidx.sqlite.db.*
import androidx.sqlite.db.framework.FrameworkSQLiteOpenHelperFactory
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class FullReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation()
    private val c=i.targetContext
    private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private val provider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
    private val media=mutableListOf<Uri>()
    private fun control(method:String)=c.contentResolver.call(provider,method,null,null)!!
    private fun uri(path:String)=provider.buildUpon().appendPath(path).build()
    private fun invitation(pin:String="a".repeat(64),name:String="Old")="pneu://pair?v=1&name=$name&fp=$pin&addr=10.0.2.2%3A47861&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private suspend fun seed(pin:String="a".repeat(64),name:String="Old") {
        val raw=invitation(pin,name)
        assertTrue(app.requestPairing(raw,app.preferences.flow.first().peer?.pin))
        assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        for(key in listOf("setup","shareSheet","queue","screenshots","wifi","tailscale","mobile","clipboardFromPc"))app.preferences.toggle(key,true)
        for(key in listOf("paused","photos","recordings"))app.preferences.toggle(key,false)
        seed();control("reset")
    }
    @After fun cleanup()=runBlocking<Unit> {
        control("release");device.pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
        media.forEach {c.contentResolver.delete(it,null,null)}
    }
    private fun caller(items:List<Uri>) {
        c.startActivity(Intent().setComponent(ComponentName(c.packageName+".test",FixtureCallerActivity::class.java.name))
            .putStringArrayListExtra("uris",ArrayList(items.map {it.toString()})).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }
    @Test(timeout=30000) fun wholeShareKeepsOriginalRecipientAcrossDelayedMetadataAndReplacement()=runBlocking<Unit> {
        val old=DeliveryBinding.from(app.preferences.flow.first())!!
        caller(listOf(uri("slow-query"),uri("image")))
        assertTrue(withContext(Dispatchers.IO) {control("entered").getBoolean("entered")})
        seed("c".repeat(64),"New");control("release")
        withTimeout(5000) {app.staging.idle {Unit}}
        delay(600);assertTrue(app.queue.pending().isEmpty())
        assertEquals("c".repeat(64),app.preferences.flow.first().peer!!.pin)
        assertEquals(1,control("counts").getInt("types")) // second item rejected before provider lookup
        val received=ClipboardDelivery(app) {_,_->fail("Obsolete session applied to replacement")}
        try {received.receive(old.pcId,incoming(900),old);fail("Accepted obsolete session")}
        catch(_:java.io.IOException) {}
        assertFalse(app.enqueue(Outgoing().apply {mediaId=randomId();transferId=randomId()},old))
        try {ManualEntries.text(app,"Old clipboard or shared text","clipboard",old);fail("Retargeted old text")}
        catch(e:ShareFailure) {assertEquals("binding_mismatch",e.reason)}
    }
    @Test(timeout=30000) fun defaultFileAdmissionPrecedesEvenMimeLookupAndSamePcRepairRevokesIt()=runBlocking<Unit> {
        val old=app.preferences.flow.first()
        val worker=async(Dispatchers.IO) {runCatching {ManualEntries.file(app,uri("slow-type"),"file")}}
        assertTrue(withContext(Dispatchers.IO) {control("entered").getBoolean("entered")})
        app.forgetBinding();seed() // same pin and PC id, different phone identity
        control("release");assertTrue(worker.await().isFailure);assertTrue(app.queue.pending().isEmpty())
        assertFalse(app.loseBinding(old.peer,old.deviceId));app.forgetBinding(old.peer,old.deviceId)
        assertNotNull(app.preferences.flow.first().peer)
    }
    @Test(timeout=30000) fun clipboardPendingIntentsKeepTheirAdmissionAcrossReplacement()=runBlocking<Unit> {
        val old=app.preferences.flow.first();val intent=ManualEntries.clipboardIntent(c,old)
        val token=ManualEntries.admission(intent,old)!!
        val pending=PendingIntent.getActivity(c,451,intent,PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        seed("c".repeat(64),"New")
        val new=app.preferences.flow.first();val replacement=ManualEntries.clipboardIntent(c,new)
        val next=PendingIntent.getActivity(c,451,replacement,PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        try {
            assertNotEquals(pending,next)
            assertEquals(token,ManualEntries.admission(intent,new));assertFalse(token.matches(new))
            pending.send();delay(500);assertTrue(app.queue.pending().isEmpty())
        } finally {pending.cancel();next.cancel()}
    }
    private fun insert(video:Boolean,folder:String):Uri {
        val collection=if(video)MediaStore.Video.Media.getContentUri("external_primary") else MediaStore.Images.Media.getContentUri("external_primary")
        val uri=c.contentResolver.insert(collection,ContentValues().apply {
            put("_display_name","Pneu_review_${System.nanoTime()}.${if(video)"mp4" else "png"}");put("mime_type",if(video)"video/mp4" else "image/png");put("relative_path",folder);put("is_pending",1)
        })!!;media.add(uri)
        c.contentResolver.openOutputStream(uri)!!.use {out->
            if(video)i.context.assets.open("recording.mp4").use {it.copyTo(out)}
            else Bitmap.createBitmap(24,24,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,out);recycle()}
        }
        return uri
    }
    private fun key(uri:Uri,video:Boolean)="external_primary:${MediaStore.getVersion(c,"external_primary")}:${if(video)"video" else "images"}:${ContentUris.parseId(uri)}"
    @Test(timeout=30000) fun publicationAndMovesAreDetectedForEveryWatchedFolderWithoutBacklogOrDuplicates()=runBlocking<Unit> {
        val old=insert(false,"Pictures/Pneu review/")
        app.preferences.toggle("photos",true);app.preferences.toggle("recordings",true)
        val observer=ScreenshotObserver(app);observer.scanOnce();observer.scanOnce()
        for((video,folder) in listOf(false to "Pictures/Screenshots/",false to "DCIM/Screenshots/",false to "DCIM/Camera/",true to "DCIM/Screen recordings/",true to "Movies/Screen recordings/")) {
            val source=insert(video,if(video)"Movies/Pneu review/" else "Pictures/Pneu review/")
            observer.scanOnce();assertNull(app.queue.find(key(source,video)))
            c.contentResolver.update(source,ContentValues().apply {put("relative_path",folder);put("is_pending",0)},null,null)
            observer.scanOnce();val row=app.queue.find(key(source,video))!!
            assertEquals("waiting",row.state)
            val transfer=row.transferId
            c.contentResolver.update(source,ContentValues().apply {put("_display_name","Moved_again_${System.nanoTime()}.${if(video)"mp4" else "png"}")},null,null)
            observer.scanOnce();assertEquals(transfer,app.queue.find(row.mediaId)!!.transferId)
        }
        c.contentResolver.update(old,ContentValues().apply {put("relative_path","DCIM/Camera/");put("is_pending",0)},null,null)
        observer.scanOnce();assertNull(app.queue.find(key(old,false)))
    }
    @Test(timeout=30000) fun incompleteAndErroredProviderRangesNeverCommit()=runBlocking<Unit> {
        for(path in listOf("short","error","error-full")) {
            val row=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="file";automatic=false;fileName="Fixture.bin";mime="application/octet-stream";this.uri=uri(path).toString()}
            try {app.staging.capture(row);fail("Accepted $path")}
            catch(_:Exception) {}
            assertNull(app.queue.find(row.mediaId));assertFalse(File(c.filesDir,"outgoing/${row.transferId}.bin").exists());assertFalse(File(c.filesDir,"outgoing/${row.transferId}.part").exists())
        }
        val valid=ManualEntries.file(app,uri("complete"),"file");assertEquals(4L,valid.size)
        assertEquals(sha256(byteArrayOf(1,2,3,4)),valid.hash)
    }
    @Test(timeout=30000) fun providerMetadataIsBoundedAndPreviewsNeverReopenExternalUris()=runBlocking<Unit> {
        val stalled=async(Dispatchers.IO) {runCatching {ProviderReads.call(timeout=100) {c.contentResolver.getType(uri("slow-type"))}}}
        assertTrue(withContext(Dispatchers.IO) {control("entered").getBoolean("entered")})
        assertTrue(stalled.await().isFailure);control("release")
        val row=ManualEntries.file(app,uri("image"),"file")
        row.uri=uri("slow-type").toString();control("reset")
        val bitmaps=(0 until 12).map {async(Dispatchers.IO) {MediaPreview.load(c,row)}}.awaitAll()
        bitmaps.forEach {assertNotNull(it);it!!.recycle()}
        val counts=control("counts");assertEquals(0,counts.getInt("types"));assertEquals(0,counts.getInt("queries"));assertEquals(0,counts.getInt("opens"))
        row.path=File(c.filesDir,"outgoing/missing.bin").path;assertNull(MediaPreview.load(c,row))
    }
    private fun incoming(n:Int,time:Long=n.toLong())=json("transfer_id" to n.toString(16).padStart(32,'0'),"text" to "Fixture $n","created_at" to time,"origin" to "clipboard")
    @Test(timeout=30000) fun clipboardHistoryPrunesWithoutLosingWatermarkAndFailuresRetainNothing()=runBlocking<Unit> {
        val peer=app.preferences.flow.first().peer!!.deviceId
        val failed=ClipboardDelivery(app) {_,_->throw SecurityException("Synthetic unavailable clipboard")}
        assertEquals("clipboard_unavailable",failed.receive(peer,incoming(1,1000)).body["code"].asString)
        assertEquals(0,app.queue.receiptCount());assertEquals(-1L,app.queue.lastApplied(peer))
        var applied=0;val delivery=ClipboardDelivery(app) {_,_->applied++}
        assertEquals(Type.ACK,delivery.receive(peer,incoming(1,0)).type);assertEquals(0L,app.queue.lastApplied(peer))
        repeat(100) {n->assertEquals(Type.ACK,delivery.receive(peer,incoming(n+2,1001L+n)).type)}
        assertEquals(101,applied);assertEquals(64,app.queue.receiptCount());assertNull(app.queue.receipt(peer,incoming(1)["transfer_id"].asString));assertEquals(1100L,app.queue.lastApplied(peer))
        repeat(100) {n->assertTrue(delivery.receive(peer,incoming(n+200,0)).body["deduplicated"].asBoolean)}
        assertEquals(64,app.queue.receiptCount()) // stale traffic cannot displace applied IDs
        assertTrue(delivery.receive(peer,incoming(1,0)).body["deduplicated"].asBoolean);assertEquals(101,applied)
        val recent=incoming(101,1100);assertEquals("id_conflict",delivery.receive(peer,recent.apply {addProperty("text","Conflicting retained ID")}).body["code"].asString)
        app.preferences.toggle("paused",true);delivery.receive(peer,incoming(302,1200));assertEquals(64,app.queue.receiptCount());assertEquals(1100L,app.queue.lastApplied(peer))
        app.preferences.toggle("paused",false)
        assertEquals(Type.ACK,delivery.receive(peer,incoming(102,1101)).type);assertEquals(102,applied)
        app.forgetBinding();assertEquals(-1L,app.queue.lastApplied(peer));assertEquals(0,app.queue.receiptCount())
    }
    @Test(timeout=30000) fun migrationKeepsAppliedWatermarkAndBoundsEachPeersHistory()=runBlocking<Unit> {
        val helper=FrameworkSQLiteOpenHelperFactory().create(SupportSQLiteOpenHelper.Configuration.builder(c).name("review-migration.db").callback(object:SupportSQLiteOpenHelper.Callback(1) {
            override fun onCreate(db:SupportSQLiteDatabase) {
                db.execSQL("CREATE TABLE receipts (peerId TEXT NOT NULL,transferId TEXT NOT NULL,hash TEXT NOT NULL,origin TEXT NOT NULL,state TEXT NOT NULL,createdAt INTEGER NOT NULL,PRIMARY KEY(peerId,transferId))")
                for(peer in listOf("a","b"))repeat(1000) {n->db.execSQL("INSERT INTO receipts VALUES (?,?,?,'clipboard',?,?)",arrayOf<Any>(peer,n.toString(),"hash",if(n==999)"pending" else if(n==998)"complete" else "applied",if(n==0)5000L else n.toLong()))}
                QueueMigrations.clipboardWindow.migrate(db)
            }
            override fun onUpgrade(db:SupportSQLiteDatabase,oldVersion:Int,newVersion:Int) {}
        }).build())
        try {
            val db=helper.writableDatabase
            db.query("SELECT peerId,COUNT(*) FROM receipts GROUP BY peerId").use {cursor->var count=0;while(cursor.moveToNext()){assertEquals(64,cursor.getInt(1));count++};assertEquals(2,count)}
            db.query("SELECT lastApplied FROM clipboard_clock").use {cursor->var count=0;while(cursor.moveToNext()){assertEquals(5000L,cursor.getLong(0));count++};assertEquals(2,count)}
            db.query("SELECT COUNT(*) FROM receipts WHERE transferId='0'").use {assertTrue(it.moveToFirst());assertEquals(2,it.getInt(0))}
            db.query("SELECT COUNT(*) FROM receipts WHERE state='pending'").use {assertTrue(it.moveToFirst());assertEquals(0,it.getInt(0))}
        } finally {helper.close();c.deleteDatabase("review-migration.db")}
    }
    @Test(timeout=30000) fun revokedCompletionCannotPublishThumbnail()=runBlocking<Unit> {
        val row=ManualEntries.file(app,uri("image"),"file").apply {state="sent"}
        app.forgetBinding()
        assertFalse(app.staging.completeFile(row))
        assertTrue(File(c.cacheDir,"thumbnails").listFiles().orEmpty().isEmpty())
        assertTrue(app.queue.all().isEmpty())
    }
}
