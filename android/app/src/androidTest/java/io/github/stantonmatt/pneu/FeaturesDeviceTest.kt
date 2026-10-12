package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import android.graphics.Bitmap
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

/** Independent TLS peer required. Exercises real exported activities and real SystemUI tiles. */
@RunWith(AndroidJUnit4::class)
class FeaturesDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    private val instrumentation=InstrumentationRegistry.getInstrumentation()
    private val context=instrumentation.targetContext
    private val device=UiDevice.getInstance(instrumentation)
    private fun screen(name:String) {
        Thread.sleep(2400) // Let Android clipboard overlays and toast animations finish.
        val dir=File(context.filesDir,"feature-screens").apply {mkdirs()}
        instrumentation.uiAutomation.takeScreenshot().let {b->File(dir,"$name.png").outputStream().use {b.compress(Bitmap.CompressFormat.PNG,100,it)};b.recycle()}
    }
    private fun shell(command:String) {instrumentation.uiAutomation.executeShellCommand(command).use {android.os.ParcelFileDescriptor.AutoCloseInputStream(it).readBytes()}}
    private fun home() {context.startActivity(Intent(context,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK));Thread.sleep(700)}
    private suspend fun sent(id:String) {withTimeout(12000) {while(context.graph.queue.find(id)?.state!="sent")delay(50)}}
    private suspend fun latest(kind:String,origin:String?=null):Outgoing = withTimeout(10000) {while(true) {
        val row=context.graph.recent.value.firstOrNull {it.kind==kind && (origin==null || it.origin==origin)}
        if(row!=null)return@withTimeout row;delay(50)
    };error("unreachable")}
    private fun image(folder:String):android.net.Uri {
        val values=ContentValues().apply {put("_display_name","Pneu_feature_${System.currentTimeMillis()}.png");put("mime_type","image/png");put("relative_path",folder);put("is_pending",1);put("datetaken",System.currentTimeMillis())}
        val uri=context.contentResolver.insert(MediaStore.Images.Media.getContentUri("external_primary"),values)!!
        val bitmap=Bitmap.createBitmap(48,48,Bitmap.Config.ARGB_8888).apply {eraseColor(android.graphics.Color.rgb(15,110,104))}
        context.contentResolver.openOutputStream(uri)!!.use {bitmap.compress(Bitmap.CompressFormat.PNG,100,it)};bitmap.recycle()
        context.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null);return uri
    }
    @Test fun allManualAndAutomaticFeatures() = runBlocking<Unit> {
        val pin=InstrumentationRegistry.getArguments().getString("pcPin") ?: error("pcPin required")
        val port=InstrumentationRegistry.getArguments().getString("pcPort") ?: "47839"
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.CAMERA))instrumentation.uiAutomation.grantRuntimePermission(context.packageName,permission)
        context.stopService(Intent(context,PneuService::class.java));context.graph.connection.stop();delay(300)
        val app=context.graph;PeerFixture.preservePreparedIdentity(app);app.preferences.toggle("setup",true);app.preferences.toggle("screenshots",true);app.preferences.toggle("photos",false);app.preferences.toggle("recordings",false);app.preferences.toggle("queue",true)
        app.preferences.toggle("shareSheet",true);app.preferences.toggle("quickSettings",true);app.preferences.toggle("clipboardFromPc",true);app.preferences.toggle("clipboardButton",true)
        val expires=System.currentTimeMillis()+35000
        PeerFixture.evidence(expires)
        val slowPorts=requireNotNull(InstrumentationRegistry.getArguments().getString("slowPorts")).split(',')
        assertEquals(2,slowPorts.size)
        val slow=slowPorts.joinToString("") {"&addr=10.0.2.2%3A$it"}
        val uri="pneu://pair?v=1&name=Feature%20PC&fp=$pin&addr=pneu-unresolvable.invalid%3A$port$slow&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=$expires"
        context.startActivity(Intent(Intent.ACTION_VIEW,android.net.Uri.parse(uri),context,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        val pairButton=androidx.test.uiautomator.UiDevice.getInstance(instrumentation).findObject(androidx.test.uiautomator.UiSelector().text("Pair"))
        assertTrue(pairButton.waitForExists(5000));pairButton.click()
        withTimeout(90000) {while(!app.connection.connected)delay(50)}
        // Peer consumed the token then deliberately lost PAIR_RESULT. Expiry must not block recovery.
        assertTrue(System.currentTimeMillis()>expires);assertNull(app.preferences.flow.first().pending)
        val attempts=PeerFixture.evidence().filter {it.asJsonObject["event"].asString=="PAIR"}.map {it.asJsonObject}
        assertEquals(2,attempts.size);assertTrue(attempts[0]["at"].asLong<expires);assertTrue(attempts[1]["at"].asLong>expires)
        // Two silent TLS candidates cross the UI's 15 s timer before the reachable fallback.
        assertEquals(2,PeerFixture.evidence().count {it.asJsonObject["event"].asString=="HELLO"})
        delay(1100);screen("paired-recovered")
        // Home and notification actions use the same focused, invisible clipboard entry point.
        home();instrumentation.runOnMainSync {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("fixture","Home clipboard fixture"))}
        device.findObject(UiSelector().text("Send clipboard")).click()
        val homeSend=latest("text","clipboard");sent(homeSend.mediaId);assertEquals("Home clipboard fixture",homeSend.text)
        home();instrumentation.runOnMainSync {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("fixture","Notification clipboard fixture"))}
        context.getSystemService(android.app.NotificationManager::class.java).activeNotifications.single {it.id==1}.notification.actions.single {it.title.toString()=="Send clipboard"}.actionIntent.send()
        withTimeout(8000) {while(app.queue.pending().any {it.kind=="text"})delay(50);app.recent.first {rows->rows.any {it.text=="Notification clipboard fixture" && it.state=="sent"}}}
        val source=image("Pictures/Pneu tests/")
        context.startActivity(Intent(context,ShareActivity::class.java).setAction(Intent.ACTION_SEND).setType("image/png").putExtra(Intent.EXTRA_STREAM,source).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_GRANT_READ_URI_PERMISSION))
        val shared=latest("file");sent(shared.mediaId)
        home();screen("shared-image")
        context.startActivity(Intent(context,ShareActivity::class.java).setAction(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT,"Share text fixture").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        val text=latest("text","share");sent(text.mediaId)
        withTimeout(8000) {app.prefs.first {it.lastReceivedHash==sha256("PC clipboard fixture".toByteArray())}}
        home()
        instrumentation.runOnMainSync {assertEquals("PC clipboard fixture",context.getSystemService(ClipboardManager::class.java).primaryClip?.getItemAt(0)?.text?.toString())}
        screen("shared-text-and-received-clipboard")
        // A manual share is independent of Pause; automatic screenshots must instead be skipped.
        app.preferences.toggle("paused",true);app.prefs.first {it.paused}
        val skipped=image("Pictures/Screenshots/");val mediaId="external_primary:${MediaStore.getVersion(context,"external_primary")}:images:${ContentUris.parseId(skipped)}"
        delay(500);assertNull(app.queue.find(mediaId))
        home();screen("paused")
        // Real SystemUI Send clipboard tile opens the focus-acquiring activity.
        instrumentation.runOnMainSync {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("fixture","Tile clipboard fixture"))}
        shell("cmd statusbar add-tile io.github.stantonmatt.pneu/.features.PneuTile")
        shell("cmd statusbar add-tile io.github.stantonmatt.pneu/.features.ClipboardTile")
        device.openQuickSettings();delay(1200);device.waitForIdle(1000)
        shell("cmd statusbar click-tile io.github.stantonmatt.pneu/.features.PneuTile")
        app.prefs.first {!it.paused}
        shell("cmd statusbar click-tile io.github.stantonmatt.pneu/.features.PneuTile")
        app.prefs.first {it.paused};delay(500);screen("quick-settings")
        shell("cmd statusbar click-tile io.github.stantonmatt.pneu/.features.ClipboardTile")
        val clipboard=withTimeout(8000) {app.recent.first {rows->rows.any {it.text=="Tile clipboard fixture"}}.first {it.text=="Tile clipboard fixture"}};sent(clipboard.mediaId)
        assertEquals("Tile clipboard fixture",clipboard.text)
        shell("cmd statusbar collapse");home();screen("manual-send-while-paused")
        // An explicit resend of a user-owned clipboard creates another logical operation.
        val before=app.queue.nextOrder()
        context.startActivity(Intent(context,SendClipboardActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));delay(700)
        assertTrue(app.queue.nextOrder()>before)
        app.preferences.toggle("paused",false);app.prefs.first {!it.paused}
        val oldPhoto=image("DCIM/Camera/");delay(500)
        app.preferences.toggle("photos",true);app.prefs.first {it.photos}
        val newPhoto=DeviceFixture.changePolicyDuringCapture({image("DCIM/Camera/")}) {app.preferences.toggle("queue",false)}
        val newId="external_primary:${MediaStore.getVersion(context,"external_primary")}:images:${ContentUris.parseId(newPhoto)}"
        sent(newId);assertEquals("photo",app.queue.find(newId)!!.kind)
        app.preferences.toggle("queue",true)
        val oldId="external_primary:${MediaStore.getVersion(context,"external_primary")}:images:${ContentUris.parseId(oldPhoto)}"
        assertTrue(app.queue.find(oldId)?.state in listOf(null,"skipped"))
        home();screen("camera-photo")
        val first=ManualEntries.text(app,"Rejected older text fixture","share")
        withTimeout(6000) {while(app.queue.find(first.mediaId)?.state!="failed")delay(50)}
        val second=ManualEntries.text(app,"Latest text fixture","share")
        val independent=ManualEntries.file(app,source,"file");sent(independent.mediaId)
        sent(second.mediaId);assertEquals("superseded",app.queue.find(first.mediaId)!!.state)
        instrumentation.runOnMainSync {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("fixture","Keep current clipboard"))}
        val replay=ManualEntries.text(app,"Request replay fixture","share");sent(replay.mediaId);delay(500);home()
        instrumentation.runOnMainSync {assertEquals("Keep current clipboard",context.getSystemService(ClipboardManager::class.java).primaryClip?.getItemAt(0)?.text?.toString())}
        screen("latest-text-and-receipt-dedupe")
        // Stage an explicit manual encoding fixture while paused. Its retry deadline
        // keeps the live sender from racing this deliberately invoked optimiser.
        var optimized=Outgoing().apply {this.mediaId="manual:${randomId()}";transferId=randomId();this.uri=source.toString();kind="photo";fileName="fixture.png";mime="image/png";automatic=false;nextAttempt=Long.MAX_VALUE;createdAt=System.currentTimeMillis()}
        app.preferences.toggle("paused",true);app.prefs.first {it.paused};app.staging.capture(optimized)
        val originalId=optimized.transferId;optimized=app.staging.optimize(optimized)
        assertNotEquals(originalId,optimized.transferId);assertEquals("image/jpeg",optimized.mime)
        assertEquals(optimized.hash,File(optimized.path).inputStream().use {input ->sha256(input.readBytes())})
        val bitmap=android.graphics.BitmapFactory.decodeFile(optimized.path)!!;assertTrue(maxOf(bitmap.width,bitmap.height)<=2560);bitmap.recycle()
        assertTrue(app.discard(ConfirmedQueue.from(app.preferences.read(),listOf(optimized))))
        context.contentResolver.delete(source,null,null);context.contentResolver.delete(skipped,null,null);context.contentResolver.delete(oldPhoto,null,null);context.contentResolver.delete(newPhoto,null,null)
        shell("cmd statusbar remove-tile io.github.stantonmatt.pneu/.features.PneuTile");shell("cmd statusbar remove-tile io.github.stantonmatt.pneu/.features.ClipboardTile")
        app.connection.unpair();context.stopService(Intent(context,PneuService::class.java))
    }
}
