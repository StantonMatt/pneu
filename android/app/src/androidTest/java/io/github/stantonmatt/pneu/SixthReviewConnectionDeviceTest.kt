package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import android.content.pm.PackageManager
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.io.IOException
import java.util.concurrent.atomic.*

@RunWith(AndroidJUnit4::class)
class SixthReviewConnectionDeviceTest {
    @org.junit.Before fun resetDeviceFixture()=kotlinx.coroutines.runBlocking<Unit> {
        DeviceFixture.reset()
    }

    @Test(timeout=45000) fun failedReplacementCleanupRecoversSameLoopAndStagedShareSurvivesRevokedSource()=runBlocking<Unit> {
        val i=InstrumentationRegistry.getInstrumentation();val c=i.targetContext;val app=c.graph;val args=InstrumentationRegistry.getArguments();val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val pin=requireNotNull(args.getString("pcPin"));val port=requireNotNull(args.getString("pcPort"))
        UiDevice.getInstance(i).pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();delay(250);PeerFixture.preservePreparedIdentity(app)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        app.preferences.toggle("shareSheet",true);app.preferences.toggle("setup",true)
        fun raw(fp:String,name:String)="pneu://pair?v=1&name=$name&fp=$fp&addr=10.0.2.2%3A$port&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        val old=raw("a".repeat(64),"Old");assertTrue(app.requestPairing(old,null));assertTrue(app.installBinding(Invitation.parse(old),"c".repeat(32)))
        val phone=app.preferences.flow.first().deviceId;val identity=app.preferences.flow.first().tlsAlias
        val oldRow=Outgoing().apply {mediaId="obsolete:${randomId()}";transferId=randomId();automatic=false;kind="text";text="Old recipient data"}
        assertTrue(app.enqueueFixture(oldRow));val db=app.database.openHelper.writableDatabase
        db.execSQL("CREATE TRIGGER fail_reset BEFORE DELETE ON outgoing BEGIN SELECT RAISE(ABORT,'Synthetic replacement cleanup failure'); END")
        val finalWrites=AtomicInteger();val allowFinalWrite=AtomicBoolean(false)
        app.preferences.beforeMutation={if(it=="deliveryReset") {finalWrites.incrementAndGet();if(!allowFinalWrite.get())throw IOException("Synthetic final reset write failure")}}
        try {
            val next=raw(pin,"Recovery%20PC");assertTrue(app.requestPairing(next,"a".repeat(64)));app.connection.start(scope)
            withTimeout(10000) {app.preferences.flow.first {it.peer?.pin==pin && it.pending==null && it.deliveryReset}}
            assertNotNull(app.queue.find(oldRow.mediaId));assertFalse(app.connection.connected)
            db.execSQL("DROP TRIGGER fail_reset");app.connection.kick()
            withTimeout(12000) {while(finalWrites.get()<2)delay(20)}
            assertTrue(app.preferences.flow.first().deliveryReset);assertFalse(app.connection.connected);assertTrue(app.queue.all().isEmpty())
            assertEquals(1,PeerFixture.evidence().count {it.asJsonObject["event"].asString=="HELLO"})
            allowFinalWrite.set(true);app.connection.kick()
            withTimeout(8000) {while(!app.connection.connected)delay(20)}
            val current=app.preferences.flow.first();assertFalse(current.deliveryReset);assertEquals(phone,current.deviceId);assertEquals(identity,current.tlsAlias)
            // Cellular gating keeps the admitted share waiting while its grant is revoked.
            assertTrue("Fixture requires observed emulator cellular route",app.connection.cellular())
            app.preferences.toggle("mobile",false)
            withTimeout(5000) {while(app.connection.connected)delay(20)}
            val source=Uri.parse("content://io.github.stantonmatt.pneu.test.sources/sixth-${randomId()}")
            fun caller(operation:String?=null) {c.startActivity(Intent().setComponent(ComponentName(c.packageName+".test",FixtureCallerActivity::class.java.name)).putExtra("uri",source.toString()).putExtra("persistable",true).apply {if(operation!=null)putExtra("grantOperation",operation)}.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))}
            caller()
            val row=withTimeout(8000) {while(true) {val found=app.queue.pending().singleOrNull {it.kind=="file"};if(found!=null)return@withTimeout found;delay(20)};error("unreachable")}
            assertEquals("",row.uri);assertTrue(File(row.path).exists());assertEquals(row.hash,sha256(File(row.path).readBytes()))
            assertTrue(c.contentResolver.persistedUriPermissions.none {it.uri==source})
            caller("revoke")
            withTimeout(5000) {while(c.checkUriPermission(source,android.os.Process.myPid(),c.applicationInfo.uid,Intent.FLAG_GRANT_READ_URI_PERMISSION)==PackageManager.PERMISSION_GRANTED)delay(20)}
            assertThrows(SecurityException::class.java) {c.contentResolver.openInputStream(source)?.close()}
            app.preferences.toggle("mobile",true)
            withTimeout(10000) {while(app.queue.find(row.mediaId)?.state!="sent")delay(20)}
            val sent=app.queue.find(row.mediaId)!!;assertEquals("",sent.uri);assertFalse(File(row.path).exists())
            assertEquals(File(c.cacheDir,"thumbnails"),File(sent.path).parentFile);assertTrue(File(sent.path).length() in 1..65536)
            val events=PeerFixture.evidence();assertEquals(1,events.count {it.asJsonObject["event"].asString=="PAIR"})
            assertTrue(events.none {it.asJsonObject["event"].asString=="TEXT"})
            val file=events.single {it.asJsonObject["event"].asString=="FILE"}.asJsonObject
            assertEquals(row.transferId,file["transfer_id"].asString);assertEquals(row.hash,file["sha256"].asString)
        } finally {
            allowFinalWrite.set(true);app.preferences.beforeMutation={};db.execSQL("DROP TRIGGER IF EXISTS fail_reset");UiDevice.getInstance(i).pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.forgetBinding();scope.cancel()
        }
    }
}
