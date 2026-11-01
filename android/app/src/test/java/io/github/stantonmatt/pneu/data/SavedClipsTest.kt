package io.github.stantonmatt.pneu.data

import android.app.Application
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Color
import androidx.exifinterface.media.ExifInterface
import androidx.room.Room
import io.github.stantonmatt.pneu.historyDisplayOrder
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class SavedClipsTest {
    private lateinit var scope:CoroutineScope
    private lateinit var db:SavedClipsDatabase
    private lateinit var store:SavedClips
    private val context:Context get()=RuntimeEnvironment.getApplication()
    private var time=1000L
    private val directory get()=File(context.filesDir,"clipboard-history/images")
    @Before fun start() {
        // Attach against this test's data directory instead of a cached prior root.
        val provider=context.packageManager.resolveContentProvider("${context.packageName}.history",android.content.pm.PackageManager.GET_META_DATA)!!
        org.robolectric.Robolectric.buildContentProvider(androidx.core.content.FileProvider::class.java).create(provider)
        scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        db=Room.inMemoryDatabaseBuilder(context,SavedClipsDatabase::class.java).build()
        store=SavedClips(context,scope,db) {time++}
    }
    @After fun stop() {scope.cancel();db.close();directory.deleteRecursively();File(context.filesDir,"clipboard-snapshot").deleteRecursively()}
    private val items get()=store.state.value.items
    private fun image(format:Bitmap.CompressFormat=Bitmap.CompressFormat.PNG):ByteArray {
        val bitmap=Bitmap.createBitmap(12,10,Bitmap.Config.ARGB_8888).apply {eraseColor(Color.BLUE)}
        return ByteArrayOutputStream().also {bitmap.compress(format,100,it);bitmap.recycle()}.toByteArray()
    }
    @Test fun optInStartsEmptyAndNeverImportsAnExistingClipboard()=runBlocking {
        context.getSystemService(android.content.ClipboardManager::class.java).setPrimaryClip(android.content.ClipData.newPlainText("existing","private"))
        store.recordText("off");assertTrue(items.isEmpty());assertFalse(store.state.value.enabled)
        store.setEnabled(true);assertTrue(items.isEmpty())
        store.recordText("next");assertEquals("next",items.single().text)
        store.setEnabled(false);assertTrue(items.isEmpty());store.recordText("still off");store.setEnabled(true);assertTrue(items.isEmpty())
    }
    @Test fun exactDuplicatesMoveToTopRefreshTimeAndOriginAndCapAtFifty()=runBlocking {
        store.setEnabled(true)
        repeat(50) {store.recordText("text $it","PC")}
        val first=items.last();val old=first.copiedAt
        store.recordText(first.text);assertEquals(50,items.size);assertEquals(first.id,items.first().id)
        assertTrue(items.first().copiedAt>old);assertEquals("",items.first().sender)
        store.recordText("new","MacBook Air");assertEquals(50,items.size)
        assertFalse(items.any {it.text=="text 1"});assertEquals("MacBook Air",items.first().sender)
        store.recordText("new ");assertEquals("new ",items.first().text);assertTrue(items.any {it.text=="new"})
    }
    @Test fun copyPreservesOriginAndKeepsTheDisplayedOrderUntilReopen()=runBlocking {
        store.setEnabled(true);store.recordText("older","PC",source="pc-id");store.recordText("newer")
        val display=items.map {it.id};val old=items.last().copiedAt
        var copied=""
        var source=""
        assertTrue(store.copy(display.last()) {copied=it.getItemAt(0).text.toString();source=it.description.extras!!.getString(io.github.stantonmatt.pneu.features.ClipboardDelivery.OWN_SOURCE).orEmpty()})
        assertEquals("pc-id",source);assertEquals("older",copied);assertEquals("PC",items.first().sender);assertTrue(items.first().copiedAt>old)
        assertEquals(display,historyDisplayOrder(display,items))
        assertEquals(display.reversed(),items.map {it.id})
        store.recordText("incoming","Mac");assertEquals(listOf(items.first().id)+display,historyDisplayOrder(display,items))
    }
    @Test fun liveDuplicateArrivalMovesToTopButHistorySelectionDoesNot()=runBlocking {
        store.setEnabled(true);store.recordText("older","PC");store.recordText("newer")
        val display=items.map {it.id};val initial=store.state.value.arrivals
        store.copy(display.last()) {}
        assertEquals(initial,store.state.value.arrivals)
        assertEquals(display,historyDisplayOrder(display,items,initial,store.state.value.arrivals))
        store.recordText("older","Mac")
        assertEquals(display.reversed(),historyDisplayOrder(display,items,initial,store.state.value.arrivals))
        assertEquals("Mac",items.first().sender)
    }
    @Test fun deleteAndClearDoNotTouchTheSystemClipboard()=runBlocking {
        store.setEnabled(true);store.recordText("one");store.recordText("two")
        val clipboard=context.getSystemService(android.content.ClipboardManager::class.java)
        clipboard.setPrimaryClip(android.content.ClipData.newPlainText("outside","keep"))
        store.delete(items.last().id);assertEquals(listOf("two"),items.map {it.text});store.clear();assertTrue(items.isEmpty())
        assertEquals("keep",clipboard.primaryClip!!.getItemAt(0).text.toString());assertTrue(store.state.value.enabled)
    }
    @Test fun sizeBlankAndSensitiveLimitsApplyBeforeOpeningImages()=runBlocking {
        store.setEnabled(true)
        for(text in listOf(""," \t\r\n","a".repeat(SavedClips.MAX_TEXT+1)))store.recordText(text)
        store.recordText("secret",sensitive=true);assertTrue(items.isEmpty())
        store.recordText("a".repeat(SavedClips.MAX_TEXT));assertEquals(1,items.size)
        store.recordImage("image/png",sensitive=true) {fail("opened sensitive payload");null}
        store.recordImage("image/png") {ByteArrayInputStream(ByteArray(SavedClips.MAX_IMAGE+1))}
        assertEquals(1,items.size)
        val boundary=image().copyOf(SavedClips.MAX_IMAGE)
        store.recordImage("image/png") {boundary.inputStream()};assertEquals(2,items.size)
        val before=items.first().copiedAt
        store.recordImage("image/png") {boundary.copyOf(SavedClips.MAX_IMAGE+1).inputStream()}
        assertEquals(before,items.first().copiedAt)
    }
    @Test fun imageDedupeUsesEncodedBytesAndEvictionDeleteClearAndTurnOffRemoveFiles()=runBlocking {
        store.setEnabled(true)
        val png=image();store.recordImage("image/png","PC") {png.inputStream()}
        assertEquals(2,directory.listFiles()!!.size);val id=items.single().id
        store.recordImage("image/png") {png.inputStream()};assertEquals(1,items.size);assertEquals(id,items.single().id);assertEquals("",items.single().sender)
        // PNG permits trailing data: same pixels, distinct encoded payloads.
        val otherEncoding=png+byteArrayOf(0)
        store.recordImage("image/png") {otherEncoding.inputStream()};assertEquals(2,items.size);assertNotEquals(id,items.first().id)
        repeat(50) {store.recordText("$it")};assertFalse(items.any {it.id==id});assertEquals(0,directory.listFiles()!!.size)
        store.recordImage("image/png") {png.inputStream()};store.delete(items.first().id);assertEquals(0,directory.listFiles()!!.size)
        store.recordImage("image/png") {png.inputStream()};store.clear();assertEquals(0,directory.listFiles()!!.size)
        store.recordImage("image/png") {png.inputStream()};store.setEnabled(false);assertFalse(store.state.value.enabled);assertTrue(items.isEmpty());assertEquals(0,directory.listFiles()!!.size)
    }
    @Test @GraphicsMode(GraphicsMode.Mode.NATIVE)
    fun imageThumbnailAppliesExifOrientationAndPreservesEncodedOriginal()=runBlocking {
        val jpeg=File.createTempFile("history-exif-",".jpg",context.cacheDir)
        val bytes=try {
            jpeg.writeBytes(image(Bitmap.CompressFormat.JPEG))
            ExifInterface(jpeg.path).apply {
                setAttribute(ExifInterface.TAG_ORIENTATION,ExifInterface.ORIENTATION_ROTATE_90.toString())
                saveAttributes()
            }
            jpeg.readBytes()
        } finally {jpeg.delete()}
        store.setEnabled(true)
        store.recordImage("image/jpeg") {bytes.inputStream()}
        val item=items.single()
        val thumbnail=BitmapFactory.decodeFile(store.thumbnail(item).path)!!
        try {assertEquals(10,thumbnail.width);assertEquals(12,thumbnail.height)}
        finally {thumbnail.recycle()}
        assertArrayEquals(bytes,directory.resolve(item.image).readBytes())
        assertEquals("image:"+io.github.stantonmatt.pneu.protocol.sha256(bytes),item.id)
        assertTrue(store.copy(item.id) {clip->
            assertArrayEquals(bytes,context.contentResolver.openInputStream(clip.getItemAt(0).uri!!)!!.use {it.readBytes()})
        })
    }
    @Test fun clipboardSnapshotSurvivesCleanupAndRestartAndOnlyTheNextImageCopyReplacesIt()=runBlocking {
        store.setEnabled(true)
        val png=image();store.recordImage("image/png","PC",source="pc-id") {png.inputStream()}
        lateinit var clip:android.content.ClipData
        store.copy(items.single().id) {clip=it}
        val uri=clip.getItemAt(0).uri!!
        val extras=clip.description.extras!!
        assertTrue(extras.getBoolean(io.github.stantonmatt.pneu.features.ClipboardDelivery.OWN_CLIP))
        assertEquals("pc-id",extras.getString(io.github.stantonmatt.pneu.features.ClipboardDelivery.OWN_SOURCE))
        assertEquals(io.github.stantonmatt.pneu.protocol.sha256(png),extras.getString(io.github.stantonmatt.pneu.features.ClipboardDelivery.OWN_HASH))
        assertFalse(extras.getBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE))
        store.setEnabled(false)
        val restored=SavedClips(context,scope,db)
        assertTrue(restored.ownsImage(uri)) // Also awaits startup reconciliation with history off.
        assertArrayEquals(png,context.contentResolver.openInputStream(uri)!!.use {it.readBytes()})
        restored.setEnabled(true);restored.recordText("text");restored.copy(restored.state.value.items.single().id) {}
        assertArrayEquals(png,context.contentResolver.openInputStream(uri)!!.use {it.readBytes()})
        val webp=image(Bitmap.CompressFormat.WEBP_LOSSLESS)
        restored.recordImage("image/webp") {webp.inputStream()}
        lateinit var next:android.net.Uri
        restored.copy(restored.state.value.items.first().id) {next=it.getItemAt(0).uri!!}
        assertNotEquals(uri,next);assertFalse(restored.ownsImage(uri));assertTrue(restored.ownsImage(next))
        assertArrayEquals(webp,context.contentResolver.openInputStream(next)!!.use {it.readBytes()})
        assertEquals("image/webp",context.contentResolver.getType(next))
        assertEquals(1,File(context.filesDir,"clipboard-snapshot").listFiles()!!.size)
        // Only the clipboard slot is exported, never retained history images.
        assertTrue(runCatching {androidx.core.content.FileProvider.getUriForFile(context,"${context.packageName}.history",directory.resolve(restored.state.value.items.first().image))}.isFailure)
    }
    @Test fun settingTextAndImagesSurviveClosingAndReopeningTheRoomDatabase()=runBlocking {
        store.recordText(" ") // Complete the independent default fixture's startup.
        val name="saved-clips-restart.db";context.deleteDatabase(name)
        fun open()=Room.databaseBuilder(context,SavedClipsDatabase::class.java,name).build()
        val first=open()
        try {
            val saved=SavedClips(context,scope,first)
            saved.setEnabled(true);saved.recordText("durable text","Studio PC",source="pc-id")
            val png=image();saved.recordImage("image/png","Mac") {png.inputStream()}
            saved.copy(saved.state.value.items.first().id) {}
        } finally {first.close()}
        val second=open()
        try {
            val restored=SavedClips(context,scope,second);restored.recordText(" ")
            assertTrue(restored.state.value.enabled);assertEquals(2,restored.state.value.items.size)
            assertEquals("Mac",restored.state.value.items.first().sender)
            assertEquals("pc-id",restored.state.value.items.last().source)
            assertEquals(2,directory.listFiles()!!.size)
            val snapshot=File(context.filesDir,"clipboard-snapshot").listFiles()!!.single()
            val uri=androidx.core.content.FileProvider.getUriForFile(context,"${context.packageName}.history",snapshot)
            assertTrue(restored.ownsImage(uri))
            assertArrayEquals(image(),context.contentResolver.openInputStream(uri)!!.use {it.readBytes()})
        } finally {second.close();context.deleteDatabase(name)}
    }
    @Test fun restartRetainsItemsAndDropsMissingImageDataAndOrphanFiles()=runBlocking {
        store.setEnabled(true);store.recordText("durable","PC")
        val png=image();store.recordImage("image/png") {png.inputStream()}
        directory.resolve(items.first().image).delete();directory.resolve("orphan").writeText("leftover")
        val reload=SavedClips(context,scope,db)
        reload.recordText(" ") // Wait for its startup reconciliation.
        assertTrue(reload.state.value.enabled);assertEquals(listOf("durable"),reload.state.value.items.map {it.text})
        assertEquals(0,directory.listFiles()!!.size)
    }
}
