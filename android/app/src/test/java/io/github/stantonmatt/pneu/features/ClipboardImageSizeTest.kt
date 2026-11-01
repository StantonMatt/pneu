package io.github.stantonmatt.pneu.features

import android.app.Application
import android.content.*
import android.database.MatrixCursor
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import io.github.stantonmatt.pneu.data.SavedClips
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowContentResolver

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class ClipboardImageSizeTest {
    private class Source(var size:Long?):ContentProvider() {
        var opens=0
        override fun onCreate()=true
        override fun query(uri:Uri,projection:Array<out String>?,selection:String?,args:Array<out String>?,sort:String?)=
            MatrixCursor(arrayOf(OpenableColumns.SIZE)).apply {addRow(arrayOf<Any?>(size))}
        override fun openFile(uri:Uri,mode:String):ParcelFileDescriptor {opens++;throw java.io.FileNotFoundException("Payload must only be read by staging")}
        override fun getType(uri:Uri)="image/png"
        override fun insert(uri:Uri,values:ContentValues?):Uri?=null
        override fun delete(uri:Uri,selection:String?,args:Array<out String>?)=0
        override fun update(uri:Uri,values:ContentValues?,selection:String?,args:Array<out String>?)=0
    }
    @Test fun metadataRejectsKnownOversizeAndNeverReadsUnknownPayloads()=runBlocking {
        val source=Source(null);val uri=Uri.parse("content://clipboard-size/image")
        ShadowContentResolver.registerProviderInternal(uri.authority!!,source)
        val context:Context=RuntimeEnvironment.getApplication()
        // Four unknown-size requests must not leave payload reads in metadata workers.
        repeat(4) {ClipboardImageSize.check(context,uri)}
        assertEquals("metadata remains available",ProviderReads.call {"metadata remains available"})
        source.size=SavedClips.MAX_IMAGE.toLong();ClipboardImageSize.check(context,uri)
        source.size=SavedClips.MAX_IMAGE+1L
        try {ClipboardImageSize.check(context,uri);fail("Oversized image accepted")}
        catch(e:ShareFailure) {assertEquals("image_too_large",e.reason)}
        assertEquals(0,source.opens)
    }
    @Test fun stagingReadStopsAtExactlyTheLimitPlusOneByte() {
        var count=0L
        try {
            // An unknown-length stream keeps supplying complete buffers.
            while(true) {
                val n=ClipboardImageSize.readLimit(count,65536)
                assertTrue(n>0);count+=n;ClipboardImageSize.requireAllowed(count)
            }
        } catch(e:ShareFailure) {assertEquals("image_too_large",e.reason)}
        assertEquals(SavedClips.MAX_IMAGE+1L,count)
        ClipboardImageSize.requireAllowed(SavedClips.MAX_IMAGE.toLong())
    }
}
