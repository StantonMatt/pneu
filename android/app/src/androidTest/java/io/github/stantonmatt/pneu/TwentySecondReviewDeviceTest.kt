package io.github.stantonmatt.pneu

import android.Manifest
import android.content.ContentUris
import android.content.ContentValues
import android.graphics.Bitmap
import android.graphics.Color
import android.net.Uri
import android.provider.MediaStore
import androidx.exifinterface.media.ExifInterface
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.Connectivity
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.features.MediaPreview
import io.github.stantonmatt.pneu.media.ScreenshotObserver
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class TwentySecondReviewDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val sources=mutableListOf<Uri>()
    @Before fun reset()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        val raw="pneu://pair?v=1&name=Offline%20PC&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"c".repeat(32)))
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.connection.stop();app.forgetBinding()
        sources.forEach {c.contentResolver.delete(it,null,null)}
        app.staging.cleanup()
    }
    private fun screenshot():Uri {
        val uri=c.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
            put("_display_name","Pneu_offline_${randomId()}.png");put("mime_type","image/png");put("relative_path","Pictures/Screenshots/");put("is_pending",1)
        })!!;sources.add(uri)
        c.contentResolver.openOutputStream(uri)!!.use {output->
            Bitmap.createBitmap(16,16,Bitmap.Config.ARGB_8888).apply {compress(Bitmap.CompressFormat.PNG,100,output);recycle()}
        }
        c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)
        return uri
    }
    @Test(timeout=30000) fun offlineScreenshotsObeyQueueWhileOnlyMeteredMobileOffWaitsForWifi()=runBlocking<Unit> {
        var network=Connectivity.NONE
        val observer=ScreenshotObserver(app,connectivity={network})
        observer.scanOnce();observer.scanOnce()
        assertFalse(app.connection.connected)
        for(route in Connectivity.entries)for(queue in listOf(false,true))for(mobile in listOf(false,true)) {
            network=route;app.preferences.toggle("queue",queue);app.preferences.toggle("mobile",mobile)
            val uri=screenshot();observer.scanOnce()
            val key="external_primary:${MediaStore.getVersion(c,"external_primary")}:images:${ContentUris.parseId(uri)}"
            val row=app.queue.find(key) ?: error("Observer did not record $route queue=$queue mobile=$mobile")
            val expected=if(queue || (route==Connectivity.METERED && !mobile)) "waiting" else "skipped"
            assertEquals("$route queue=$queue mobile=$mobile",expected,row.state)
            assertEquals(expected=="waiting",File(row.path).isFile)
        }
    }
    private val colors=listOf(Color.RED,Color.GREEN,Color.BLUE,Color.YELLOW)
    private fun jpeg(orientation:Int):Outgoing {
        val file=File(c.filesDir,"outgoing/${randomId()}.bin").apply {parentFile!!.mkdirs()}
        Bitmap.createBitmap(80,48,Bitmap.Config.ARGB_8888).apply {
            for(y in 0 until height)for(x in 0 until width)setPixel(x,y,colors[(if(y<24)0 else 2)+(if(x<40)0 else 1)])
            file.outputStream().use {assertTrue(compress(Bitmap.CompressFormat.JPEG,100,it))};recycle()
        }
        ExifInterface(file.path).apply {setAttribute(ExifInterface.TAG_ORIENTATION,orientation.toString());saveAttributes()}
        return Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();path=file.path;kind="photo";automatic=false;mime="image/jpeg";fileName="Oriented.jpg";size=file.length();hash=sha256(file.readBytes());sourceHash=hash;createdAt=System.currentTimeMillis()}
    }
    private fun checkPixels(bitmap:Bitmap,orientation:Int) {
        try {
            assertEquals(if(orientation>=5)48 else 80,bitmap.width)
            assertEquals(if(orientation>=5)80 else 48,bitmap.height)
            // Independent expected display corners: TL, TR, BL, BR. Rotation
            // and mirroring use different colors, so dimensions alone cannot pass.
            val corners=listOf(listOf(0,1,2,3),listOf(1,0,3,2),listOf(3,2,1,0),listOf(2,3,0,1),
                listOf(0,2,1,3),listOf(2,0,3,1),listOf(3,1,2,0),listOf(1,3,0,2))[orientation-1]
            val positions=listOf(1 to 1,3 to 1,1 to 3,3 to 3)
            for((index,position) in positions.withIndex()) {
                val pixel=bitmap.getPixel(bitmap.width*position.first/4,bitmap.height*position.second/4)
                val expected=colors[corners[index]]
                assertTrue("EXIF $orientation corner $index",listOf(Color.red(pixel)-Color.red(expected),Color.green(pixel)-Color.green(expected),Color.blue(pixel)-Color.blue(expected)).all {kotlin.math.abs(it)<40})
            }
        }finally {bitmap.recycle()}
    }
    @Test(timeout=30000) fun everyExifOrientationMatchesAcrossPreviewCompletedThumbnailAndOptimization()=runBlocking<Unit> {
        for(orientation in 1..8) {
            val row=jpeg(orientation);assertTrue(app.enqueueFixture(row))
            checkPixels(MediaPreview.load(c,row) ?: error("Missing source preview"),orientation)
            val optimized=app.staging.optimize(row)
            checkPixels(MediaPreview.load(c,optimized) ?: error("Missing optimized preview"),orientation)
            optimized.state="sent";assertTrue(app.staging.completeFile(optimized))
            assertTrue(File(optimized.path).length() in 1..65536)
            checkPixels(MediaPreview.load(c,optimized) ?: error("Missing optimized thumbnail"),orientation)
            // Original (unoptimized) completion must also bake the orientation
            // into thumbnail pixels, which have no EXIF transform to apply again.
            val original=jpeg(orientation);assertTrue(app.enqueueFixture(original))
            original.state="sent";assertTrue(app.staging.completeFile(original))
            assertTrue(File(original.path).length() in 1..65536)
            checkPixels(MediaPreview.load(c,original) ?: error("Missing original thumbnail"),orientation)
        }
    }
}
