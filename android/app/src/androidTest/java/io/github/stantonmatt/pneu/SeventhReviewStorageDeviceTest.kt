package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import android.graphics.Bitmap
import android.net.Uri
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class SeventhReviewStorageDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val provider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
    private fun control(command:String)=c.contentResolver.call(provider,command,null,null)!!
    private fun row(uri:Uri,automatic:Boolean=false)=Outgoing().apply {
        mediaId=if(automatic)"external_primary:${MediaStore.getVersion(c,"external_primary")}:images:${ContentUris.parseId(uri)}" else "manual:${randomId()}"
        transferId=randomId();this.uri=uri.toString();this.automatic=automatic;kind=if(automatic)"screenshot" else "file";fileName="Fixture.png";mime="image/png";createdAt=System.currentTimeMillis()
    }
    private suspend fun bind() {
        c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding()
        val raw="pneu://pair?v=1&name=Seventh&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
        app.staging.cleanup()
    }
    @Test(timeout=30000) fun blockedProxyReadDoesNotOwnStagingAndLateBytesCannotBeAdmitted()=runBlocking<Unit> {
        bind();i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.READ_MEDIA_IMAGES)
        control("reset")
        val uri=c.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
            put("_display_name","Pneu_staging_${randomId()}.png");put("mime_type","image/png");put("relative_path","Pictures/Screenshots/");put("is_pending",1)
        })!!
        try {
            c.contentResolver.openOutputStream(uri)!!.use {output->Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,output);recycle()}}
            c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)
            val blocked=row(provider.buildUpon().appendPath("proxy-stall").build())
            val result=async(Dispatchers.IO) {try {app.staging.capture(blocked);null}catch(e:Exception) {e}}
            assertTrue(ProviderReads.call {control("entered").getBoolean("entered")})
            val parts=File(c.filesDir,"outgoing").listFiles()!!.filter {it.extension=="part"}.toSet();assertEquals(1,parts.size)
            // Run real screenshot admission, optimization, receipt completion and
            // cleanup while FUSE is blocked, before the provider timeout expires.
            withTimeout(4000) {
                app.staging.cleanup();assertTrue(parts.all {it.exists()})
                val screenshot=row(uri,true);app.staging.capture(screenshot)
                val optimized=app.staging.optimize(screenshot);optimized.state="sent"
                assertTrue(app.staging.completeFile(optimized));assertTrue(File(optimized.path).exists())
            }
            assertFalse("Provider is still blocked",result.isCompleted)
            assertTrue(withTimeout(12000) {result.await()} is java.io.IOException)
            assertNull(app.queue.find(blocked.mediaId));assertTrue(parts.none {it.exists()})
            control("release");delay(500)
            assertNull(app.queue.find(blocked.mediaId));assertFalse(File(c.filesDir,"outgoing/${blocked.transferId}.bin").exists())
            assertTrue(File(c.filesDir,"outgoing").listFiles().orEmpty().none {it.extension=="part"})
            // The abandoned worker returns to its pool and subsequent copies work.
            val later=row(provider.buildUpon().appendPath("complete").build());app.staging.capture(later)
            assertEquals(sha256(byteArrayOf(1,2,3,4)),later.hash)
        } finally {control("release");c.contentResolver.delete(uri,null,null);app.forgetBinding();app.staging.cleanup()}
    }
    @Test(timeout=12000) fun abandonedCopyPoolCannotConsumeMetadataWorkersAndCancellationDiscardsTemps()=runBlocking<Unit> {
        bind()
        val release=CountDownLatch(1);val entered=CountDownLatch(4)
        try {
            val blockers=List(4) {async(Dispatchers.IO) {runCatching {ProviderReads.copy(timeout=300) {_,_->entered.countDown();release.await(5,TimeUnit.SECONDS)}}}}
            assertTrue(withContext(Dispatchers.IO) {entered.await(3,TimeUnit.SECONDS)})
            assertEquals("metadata remains available",ProviderReads.call {"metadata remains available"})
            assertTrue(blockers.awaitAll().all {it.isFailure})
            val rejected=row(provider.buildUpon().appendPath("complete").build())
            try {app.staging.capture(rejected);fail("Full copy pool must fail promptly")}catch(_:java.io.IOException) {}
            assertNull(app.queue.find(rejected.mediaId));assertTrue(File(c.filesDir,"outgoing").listFiles().orEmpty().none {it.extension=="part"})
            release.countDown();delay(200)
            control("reset")
            val cancelled=row(provider.buildUpon().appendPath("proxy-stall").build())
            val capture=launch(Dispatchers.IO) {app.staging.capture(cancelled)}
            assertTrue(ProviderReads.call {control("entered").getBoolean("entered")})
            withTimeout(1000) {capture.cancelAndJoin()}
            assertNull(app.queue.find(cancelled.mediaId));assertTrue(File(c.filesDir,"outgoing").listFiles().orEmpty().none {it.extension=="part"})
            control("release");delay(200)
            assertNull(app.queue.find(cancelled.mediaId))
        } finally {release.countDown();control("release");app.forgetBinding();app.staging.cleanup()}
    }
}
