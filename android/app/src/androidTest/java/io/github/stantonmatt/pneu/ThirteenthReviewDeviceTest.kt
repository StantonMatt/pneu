package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Application
import android.content.*
import android.net.Uri
import android.provider.MediaStore
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import kotlinx.coroutines.flow.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import com.google.gson.JsonParser

/** Host force-stops between these phases; instrumentation must not warm the app
 * before SEND. The host witnesses the Room row before starting verification. */
@RunWith(AndroidJUnit4::class)
class ThirteenthReviewDeviceTest {
    @get:Rule val fixtureTestName=org.junit.rules.TestName()
    @Before fun resetDeviceFixture()=runBlocking<Unit> {
        if(fixtureTestName.methodName=="verifyColdShareAndCleanUp")DeviceFixture.resumeColdShare()
        else DeviceFixture.reset()
    }
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val record get()=File(c.filesDir,"cold-share.json")
    @Test(timeout=15000) fun prepareColdShare()=runBlocking<Unit> {
        UiDevice.getInstance(i).pressHome();app.initialized.await();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
        for(permission in listOf(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        for(key in listOf("screenshots","photos","recordings","paused"))app.preferences.toggle(key,false)
        for(key in listOf("queue","shareSheet","wifi","tailscale","setup"))app.preferences.toggle(key,true)
        val raw="pneu://pair?v=1&name=Cold%20PC&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"c".repeat(32)))
        val mode=InstrumentationRegistry.getArguments().getString("mode") ?: "single"
        var uri="";var hash=""
        if(mode=="single") {
            val source=c.contentResolver.insert(MediaStore.Downloads.getContentUri("external_primary"),ContentValues().apply {
                put("_display_name","Pneu_cold_${randomId()}.bin");put("mime_type","application/octet-stream");put("is_pending",1)
            })!!
            val bytes=ByteArray(1048576) {it.toByte()};hash=sha256(bytes)
            withContext(Dispatchers.IO) {c.contentResolver.openOutputStream(source)!!.use {it.write(bytes)}}
            c.contentResolver.update(source,ContentValues().apply {put("is_pending",0)},null,null);uri=source.toString()
        }
        val paired=app.preferences.read()
        record.writeText(json("mode" to mode,"uri" to uri,"hash" to hash,"phone" to paired.deviceId,"pin" to paired.peer!!.pin,"pc" to paired.peer.deviceId,"text" to "Cold process text").toString())
        assertTrue(app.queue.pending().isEmpty())
    }
    @Test(timeout=15000) fun verifyColdShareAndCleanUp()=runBlocking<Unit> {
        val data=JsonParser.parseString(record.readText()).asJsonObject
        app.initialized.await()
        try {
            val paired=app.awaitPrefs();assertTrue(paired.loaded)
            assertEquals(data["phone"].asString,paired.deviceId);assertEquals(data["pin"].asString,paired.peer!!.pin);assertEquals(data["pc"].asString,paired.peer.deviceId)
            val mode=data["mode"].asString;val rows=app.queue.pending();assertEquals(if(mode=="multiple")2 else 1,rows.size)
            rows.forEach {row->
                assertEquals("",row.uri)
                if(mode=="text") {assertEquals("text",row.kind);assertEquals(data["text"].asString,row.text)}
                else {
                    assertEquals("file",row.kind);val bytes=File(row.path).readBytes();assertEquals(row.size,bytes.size.toLong());assertEquals(row.hash,sha256(bytes))
                    if(mode=="single") {assertEquals(1048576L,row.size);assertEquals(data["hash"].asString,row.hash)}
                }
            }
        } finally {
            c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
            data["uri"].asString.takeIf {it.isNotEmpty()}?.let {c.contentResolver.delete(Uri.parse(it),null,null)}
            record.delete()
        }
    }
    @Test(timeout=15000) fun failedInitialCaptureShowsSettingsFailureWithoutAdmitting()=runBlocking<Unit> {
        app.initialized.await();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding()
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.Default)
        try {
            val snapshots=PreferenceSnapshots(flow {awaitCancellation()},owner)
            val operation=ManualEntryViewModel(c.applicationContext as Application)
            val request=ShareRequest(snapshots.captureBinding(),emptyList(),listOf("Never admitted"),true)
            val start=android.os.SystemClock.elapsedRealtime()
            i.runOnMainSync {operation.share(request)}
            val result=withTimeout(7000) {operation.result.filterNotNull().first()}
            assertTrue(android.os.SystemClock.elapsedRealtime()-start>=4500)
            assertEquals("Couldn't load Pneu settings. Open Pneu and try again.",result.message)
            assertFalse(result.pair);assertTrue(operation.completed.isEmpty());assertTrue(app.queue.pending().isEmpty())
        } finally {owner.cancel()}
    }

}
