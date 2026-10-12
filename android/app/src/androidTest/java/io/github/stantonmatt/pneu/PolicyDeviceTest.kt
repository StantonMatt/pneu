package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import android.graphics.Bitmap
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.PersistableBundle
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.connection.RoutePolicy
import io.github.stantonmatt.pneu.connection.Connectivity
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.io.RandomAccessFile

@RunWith(AndroidJUnit4::class)
class PolicyDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    private val instrumentation=InstrumentationRegistry.getInstrumentation()
    private val context=instrumentation.targetContext
    private val sources=mutableListOf<android.net.Uri>()
    @Test fun activeNetworkMeterednessIncludesVpnAndHotspots() {
        for((transport,notMetered) in listOf(
            NetworkCapabilities.TRANSPORT_WIFI to true,
            NetworkCapabilities.TRANSPORT_CELLULAR to false,
            // VPN need not expose any underlay transport to classify cost correctly.
            NetworkCapabilities.TRANSPORT_VPN to true,
            NetworkCapabilities.TRANSPORT_VPN to false,
            NetworkCapabilities.TRANSPORT_WIFI to false
        )) {
            // NetworkCapabilities has no public setters; use Android's public
            // request builder for capability fixtures without hidden APIs.
            val builder=NetworkRequest.Builder().clearCapabilities().addTransportType(transport)
            if(notMetered)builder.addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
            val caps=builder.build()
            val connectivity=Connectivity.from(caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED))
            val mobile=connectivity.mobile
            assertEquals(!notMetered,mobile)
            assertEquals(if(notMetered)"Wi-Fi" else "Mobile data",RoutePolicy.label(false,mobile))
            assertEquals("Tailscale",RoutePolicy.label(true,mobile))
            assertEquals(!notMetered,DeliveryPolicy.optimize("photo","image/png",mobile,false,false))
            assertEquals(!notMetered,DeliveryPolicy.hold("recording",true,false,mobile,false,false,false))
            assertEquals(notMetered,DeliveryPolicy.transport(true,true,true,connectivity,false))
        }
        val missing=RoutePolicy.capabilities(null)
        assertEquals(Connectivity.NONE,missing.connectivity)
        assertFalse(missing.available);assertFalse(missing.mobile);assertFalse(missing.vpn)
        assertEquals("No network",missing.label(false))
        assertFalse(DeliveryPolicy.transport(true,true,true,missing.connectivity,true))
        val cm=context.getSystemService(ConnectivityManager::class.java)
        val caps=cm.activeNetwork?.let(cm::getNetworkCapabilities)
        assertEquals(caps?.hasTransport(NetworkCapabilities.TRANSPORT_VPN)==true,RoutePolicy.active(cm).vpn)
        assertEquals(Connectivity.from(caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)).mobile,context.graph.connection.cellular())
    }
    private fun home() {context.startActivity(Intent(context,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK));Thread.sleep(700)}
    private fun screen(name:String) {
        Thread.sleep(2400) // Let Android clipboard overlays and toast animations finish.
        val file=File(context.filesDir,"feature-screens/$name.png").apply {parentFile!!.mkdirs()}
        instrumentation.uiAutomation.takeScreenshot().let {b->file.outputStream().use {b.compress(Bitmap.CompressFormat.PNG,100,it)};b.recycle()}
    }
    private suspend fun sent(id:String) {withTimeout(10000) {while(context.graph.queue.find(id)?.state!="sent")delay(50)}}
    private suspend fun row(uri:android.net.Uri,video:Boolean=false):Outgoing=withTimeout(8000) {
        val id="external_primary:${MediaStore.getVersion(context,"external_primary")}:${if(video)"video" else "images"}:${ContentUris.parseId(uri)}"
        while(context.graph.queue.find(id)==null)delay(50);context.graph.queue.find(id)!!
    }
    private fun media(folder:String,video:Boolean=false,rotated:Boolean=false,pending:Boolean=false):android.net.Uri {
        val collection=if(video)MediaStore.Video.Media.getContentUri("external_primary") else MediaStore.Images.Media.getContentUri("external_primary")
        val values=ContentValues().apply {put("_display_name","Pneu_policy_${System.nanoTime()}.${if(video)"mp4" else if(rotated)"jpg" else "png"}");put("mime_type",if(video)"video/mp4" else if(rotated)"image/jpeg" else "image/png");put("relative_path",folder);put("is_pending",1)}
        val uri=context.contentResolver.insert(collection,values)!!;sources.add(uri)
        context.contentResolver.openOutputStream(uri)!!.use {output ->
            if(video)instrumentation.context.assets.open("recording.mp4").use {it.copyTo(output)}
            else Bitmap.createBitmap(48,if(rotated)24 else 48,Bitmap.Config.ARGB_8888).apply {eraseColor(android.graphics.Color.CYAN);compress(if(rotated)Bitmap.CompressFormat.JPEG else Bitmap.CompressFormat.PNG,100,output);recycle()}
        }
        if(rotated)context.contentResolver.openFileDescriptor(uri,"rw")!!.use {fd->
            androidx.exifinterface.media.ExifInterface(fd.fileDescriptor).apply {setAttribute(androidx.exifinterface.media.ExifInterface.TAG_ORIENTATION,"6");saveAttributes()}
        }
        if(!pending)context.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null);return uri
    }
    private suspend fun receipt(ident:String) {withTimeout(7000) {while(context.graph.queue.receipt("d".repeat(32),ident.repeat(32))?.state !in setOf("complete","applied"))delay(50)}}
    @Test fun mobileRecordingsCapacityAndClipboardPolicy() = runBlocking<Unit> {
        val pin=InstrumentationRegistry.getArguments().getString("pcPin") ?: error("pcPin required")
        val port=InstrumentationRegistry.getArguments().getString("pcPort") ?: "47839"
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.CAMERA))instrumentation.uiAutomation.grantRuntimePermission(context.packageName,permission)
        val app=context.graph
        context.stopService(Intent(context,PneuService::class.java));app.connection.stop();delay(300);PeerFixture.preservePreparedIdentity(app)
        app.preferences.toggle("setup",true);app.preferences.toggle("photos",true);app.preferences.toggle("recordings",true);app.preferences.toggle("originals",false);app.preferences.toggle("mobile",true)
        app.preferences.toggle("queue",true);app.preferences.toggle("shareSheet",true);app.preferences.toggle("clipboardFromPc",true)
        val invitation="pneu://pair?v=1&name=Policy%20PC&fp=$pin&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        context.startActivity(Intent(Intent.ACTION_VIEW,android.net.Uri.parse(invitation),context,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        val pairButton=androidx.test.uiautomator.UiDevice.getInstance(instrumentation).findObject(androidx.test.uiautomator.UiSelector().text("Pair"))
        assertTrue(pairButton.waitForExists(5000));pairButton.click()
        withTimeout(18000) {while(!app.connection.connected)delay(50)}
        assertTrue("This emulator's observed default route is metered",app.connection.cellular())
        val quota=File(context.filesDir,"outgoing/quota-test.bin")
        try {
            app.preferences.toggle("mobile",false);app.prefs.first {!it.mobile}
            withTimeout(5000) {app.connection.state.first {it.status=="Waiting for Wi-Fi"}}
            app.preferences.toggle("queue",false);app.prefs.first {!it.queue}
            val automatic=DeviceFixture.changePolicyDuringCapture({media("DCIM/Camera/")}) {app.preferences.toggle("originals",true)}
            val queued=row(automatic)
            assertEquals("waiting",queued.state)
            val a=media("Pictures/Pneu policy/");val b=media("Pictures/Pneu policy/")
            context.startActivity(Intent(context,ShareActivity::class.java).setAction(Intent.ACTION_SEND_MULTIPLE).setType("image/png").putParcelableArrayListExtra(Intent.EXTRA_STREAM,arrayListOf(a,b)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_GRANT_READ_URI_PERMISSION))
            withTimeout(6000) {while(app.queue.pending().count {it.kind=="file"}<2)delay(50)}
            val shared=app.queue.pending().filter {it.kind=="file"};assertEquals(2,shared.size)
            home();screen("mobile-disabled-queued-shares")
            app.preferences.toggle("originals",false)
            app.preferences.toggle("mobile",true);app.prefs.first {it.mobile}
            withTimeout(8000) {while(!app.connection.connected)delay(50)}
            sent(queued.mediaId);for(item in shared)sent(item.mediaId)
            assertEquals("image/jpeg",app.queue.find(queued.mediaId)!!.mime)
            assertTrue(shared.all {app.queue.find(it.mediaId)!!.mime=="image/png"})
            for(item in shared) {
                val retained=app.queue.find(item.mediaId)!!
                assertEquals("",retained.uri)
                assertTrue(File(retained.path).length()<=64*1024)
                assertEquals(File(context.cacheDir,"thumbnails"),File(retained.path).parentFile)
            }
            val firstRecording=media("DCIM/Screen recordings/",true);val secondRecording=media("Movies/Screen recordings/",true)
            val recordings=listOf(row(firstRecording,true),row(secondRecording,true));delay(500)
            assertTrue(recordings.all {app.queue.find(it.mediaId)!!.state=="waiting"})
            home();screen("recordings-held-for-wifi")
            app.preferences.toggle("originals",true);app.prefs.first {it.originals}
            for(item in recordings)sent(item.mediaId)
            assertTrue(recordings.all {app.queue.find(it.mediaId)!!.kind=="recording"})
            home();screen("recordings-sent")
            app.preferences.toggle("originals",false)
            val rotated=media("Pictures/Pneu policy/",rotated=true)
            app.preferences.toggle("paused",true);app.prefs.first {it.paused}
            var rotatedRow=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();uri=rotated.toString();kind="photo";this.automatic=false;nextAttempt=Long.MAX_VALUE;fileName="rotated.jpg";mime="image/jpeg";createdAt=System.currentTimeMillis()}
            app.staging.capture(rotatedRow);rotatedRow=app.staging.optimize(rotatedRow)
            val decoded=android.graphics.BitmapFactory.decodeFile(rotatedRow.path)!!;assertEquals(24,decoded.width);assertEquals(48,decoded.height);decoded.recycle()
            val encodedId=rotatedRow.transferId;val encodedHash=rotatedRow.hash
            assertTrue(rotatedRow.optimized);assertFalse(rotatedRow.attempted)
            // Recovery after encoding publication must send that exact representation.
            app.recoverOutgoing()
            rotatedRow.nextAttempt=0;assertTrue(app.updateDelivery(rotatedRow))
            app.preferences.toggle("paused",false);app.prefs.first {!it.paused}
            sent(rotatedRow.mediaId)
            val delivered=app.queue.find(rotatedRow.mediaId)!!
            assertEquals(encodedId,delivered.transferId);assertEquals(encodedHash,delivered.hash)
            assertTrue(delivered.optimized);assertTrue(delivered.attempted)
            // URI clipboard images negotiate their capability and transfer through the file lane.
            home();instrumentation.runOnMainSync {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newUri(context.contentResolver,"fixture",a))}
            context.startActivity(Intent(context,SendClipboardActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            val image=withTimeout(6000) {app.recent.first {rows->rows.any {it.kind=="clipboard_image"}}.first {it.kind=="clipboard_image"}}
            sent(image.mediaId);assertEquals("image/jpeg",app.queue.find(image.mediaId)!!.mime)
            home();val before=app.queue.nextOrder()
            context.startActivity(Intent(context,SendClipboardActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));delay(700)
            assertTrue(app.queue.nextOrder()>before) // Deliberate image sends can repeat the same content
            val afterRepeat=app.queue.nextOrder()
            home();instrumentation.runOnMainSync {
                val sensitive=ClipData.newPlainText("fixture","Sensitive fixture")
                sensitive.description.extras=PersistableBundle().apply {putBoolean(ClipDescription.EXTRA_IS_SENSITIVE,true)}
                context.getSystemService(ClipboardManager::class.java).setPrimaryClip(sensitive)
            }
            context.startActivity(Intent(context,SendClipboardActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));delay(500)
            assertEquals(afterRepeat,app.queue.nextOrder());screen("sensitive-clipboard-blocked")
            home();instrumentation.runOnMainSync {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("fixture","Keep this clipboard"))}
            app.preferences.toggle("paused",true);app.prefs.first {it.paused}
            val latePending=media("Pictures/Screenshots/",pending=true)
            app.preferences.toggle("paused",false);app.prefs.first {!it.paused}
            context.contentResolver.update(latePending,ContentValues().apply {put("is_pending",0)},null,null)
            delay(600);val lateId="external_primary:${MediaStore.getVersion(context,"external_primary")}:images:${ContentUris.parseId(latePending)}";assertNull(app.queue.find(lateId))
            app.preferences.toggle("paused",true);app.prefs.first {it.paused}
            val receiptCount=app.queue.receiptCount();val watermark=app.queue.lastApplied("d".repeat(32));val receivedHash=app.prefs.value.lastReceivedHash
            val paused=ManualEntries.text(app,"Request paused receive fixture","share");sent(paused.mediaId);PeerFixture.acknowledged("f".repeat(32))
            assertEquals(receiptCount,app.queue.receiptCount());assertEquals(watermark,app.queue.lastApplied("d".repeat(32)));assertEquals(receivedHash,app.prefs.value.lastReceivedHash);assertNull(app.queue.receipt("d".repeat(32),"f".repeat(32)))
            home();instrumentation.runOnMainSync {assertEquals("Keep this clipboard",context.getSystemService(ClipboardManager::class.java).primaryClip!!.getItemAt(0).text.toString())}
            app.preferences.toggle("paused",false);app.preferences.toggle("clipboardFromPc",false);app.prefs.first {!it.paused && !it.clipboardFromPc}
            val disabled=ManualEntries.text(app,"Request disabled receive fixture","share");sent(disabled.mediaId);PeerFixture.acknowledged("c".repeat(32))
            assertEquals(receiptCount,app.queue.receiptCount());assertEquals(watermark,app.queue.lastApplied("d".repeat(32)));assertEquals(receivedHash,app.prefs.value.lastReceivedHash);assertNull(app.queue.receipt("d".repeat(32),"c".repeat(32)))
            home();instrumentation.runOnMainSync {assertEquals("Keep this clipboard",context.getSystemService(ClipboardManager::class.java).primaryClip!!.getItemAt(0).text.toString())}
            app.preferences.toggle("clipboardFromPc",true);app.prefs.first {it.clipboardFromPc}
            val enabled=ManualEntries.text(app,"Request enabled receive fixture","share");sent(enabled.mediaId);receipt("9")
            home();instrumentation.runOnMainSync {assertEquals("Enabled PC clipboard",context.getSystemService(ClipboardManager::class.java).primaryClip!!.getItemAt(0).text.toString())}
            // Sparse quota fixture reserves no large RAM-backed file or network payload.
            RandomAccessFile(quota,"rw").use {it.setLength(512L*1024*1024)}
            val capacity=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();uri=a.toString();kind="file";this.automatic=false;fileName="fixture.png";mime="image/png";createdAt=System.currentTimeMillis()}
            try {app.staging.capture(capacity);fail("Expected quota rejection")} catch(_: LocalCapacityException) {}
            app.prefs.first {it.localCapacity};home();screen("local-staging-capacity")
            quota.delete();app.staging.capture(capacity);sent(capacity.mediaId);app.prefs.first {!it.localCapacity}
            home();screen("policy-complete")
        } finally {
            quota.delete();sources.forEach {context.contentResolver.delete(it,null,null)}
            app.connection.unpair();context.stopService(Intent(context,PneuService::class.java))
        }
    }
}
