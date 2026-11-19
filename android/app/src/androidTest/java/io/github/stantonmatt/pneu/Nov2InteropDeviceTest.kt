package io.github.stantonmatt.pneu

import android.Manifest
import android.content.ComponentName
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.core.app.ActivityScenario
import androidx.test.platform.app.InstrumentationRegistry
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.net.Socket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/** Real Android connection/Room/spool against the desktop Core integration host.
 * The host mocks only desktop OS effects, making notification receipt observable. */
@RunWith(AndroidJUnit4::class)
class Nov2InteropDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation()
    private val c=i.targetContext
    private val app=c.graph
    private val releases=mutableListOf<CountDownLatch>()
    private var originalAccess=false
    private var activity:ActivityScenario<MainActivity>?=null
    private fun stage(value:String) {i.sendStatus(0,android.os.Bundle().apply {putString("stream","nov2: $value\n")})}
    private val component get()=ComponentName(c,PhoneNotificationListener::class.java).flattenToString()
    private fun listener(allow:Boolean) {
        android.os.ParcelFileDescriptor.AutoCloseInputStream(i.uiAutomation.executeShellCommand("cmd notification ${if(allow)"allow_listener" else "disallow_listener"} $component")).use {it.readBytes()}
    }
    private fun post(text:String="Interop one",cancel:Boolean=false) {
        i.context.startActivity(Intent(i.context,MirrorNotificationPosterActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK).putExtra("text",text).putExtra("cancel",cancel))
    }
    private suspend fun rpc(action:String,fields:JsonObject=json()):JsonObject=withContext(Dispatchers.IO) {
        fields.addProperty("action",action)
        Socket().use {socket->
            socket.connect(java.net.InetSocketAddress("10.0.2.2",InstrumentationRegistry.getArguments().getString("controlPort")!!.toInt()),5000)
            socket.soTimeout=10000
            socket.getOutputStream().write((fields.toString()+"\n").toByteArray())
            JsonParser.parseString(checkNotNull(socket.getInputStream().bufferedReader().readLine()) {"Desktop interop host closed during $action"}).asJsonObject
        }
    }
    private suspend fun until(check:(JsonObject)->Boolean):JsonObject=withTimeout(15000) {
        var state=rpc("status")
        while(!check(state)) {delay(30);state=rpc("status")}
        state
    }
    @Before fun prepare()=runBlocking {
        DeviceFixture.reset(preserveIdentity=false)
        originalAccess=c.notificationAccess()
        app.initialized.await();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.connection.awaitStopped()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.POST_NOTIFICATIONS)
        i.uiAutomation.grantRuntimePermission(i.context.packageName,Manifest.permission.POST_NOTIFICATIONS)
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,false)
        app.preferences.toggle("setup",true)
        if(!originalAccess)listener(true)
        withTimeout(15000) {while(!c.notificationAccess() || !app.phoneNotifications.connected)delay(30)}
    }
    @After fun cleanup()=runBlocking {
        releases.forEach {it.countDown()}
        post(cancel=true)
        activity?.close();activity=null
        DeviceFixture.stopPneuService()
        app.connection.stop();app.connection.awaitStopped()
        withContext(Dispatchers.Main) {app.ringing.stop(RingStopSource.REVOKED)}
        if(!originalAccess)listener(false)
        c.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folder-grants"),"revoke",null,null)
        DeviceFixture.reset(preserveIdentity=false)
    }
    @Test(timeout=270000) fun resumeBatteryRingAndNotificationsCrossRealImplementations()=runBlocking<Unit> {
        app.preferences.toggle("queue",true)
        val uri=InstrumentationRegistry.getArguments().getString("pcUri")!!
        stage("pair")
        assertTrue(app.requestPairing(uri,null));app.connection.start(app.scope)
        val invitation=Invitation.parse(uri)
        val p=withTimeout(20000) {app.preferences.flow.first {it.devices.singleOrNull()?.pin==invitation.fingerprint}}
        val pc=p.devices.single();val phone=p.deviceId
        withTimeout(15000) {while(!app.connection.connected(pc.deviceId))delay(30)}
        stage("paired")
        activity=ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("home",true))
        PneuService.start(c)
        withTimeout(10000) {while(!withContext(Dispatchers.Main) {app.ringing.serviceAttached})delay(20)}
        withTimeout(15000) {while(!app.connection.connected(pc.deviceId) || !app.connection.supports(pc.deviceId,CAP_FILE_RESUME))delay(30)}
        fun control(s:JsonObject)=s.getAsJsonObject("snapshot").getAsJsonObject("phone_controls").getAsJsonObject(phone)
        stage("battery")
        until {control(it)?.get("battery")?.isJsonNull==false}
        app.preferences.toggle("showBatteryLevel",false)
        until {control(it)?.get("battery")?.isJsonNull==true}
        app.preferences.toggle("showBatteryLevel",true)
        until {control(it)?.get("battery")?.isJsonNull==false}
        stage("ring")
        rpc("ring",json("device" to phone,"ring" to "start"))
        withTimeout(10000) {app.ringing.state.first {it.ringing}}
        until {control(it)?.get("ringing")?.asBoolean==true}
        rpc("ring",json("device" to phone,"ring" to "stop"))
        withTimeout(10000) {app.ringing.state.first {!it.ringing}}
        until {control(it)?.get("ringing")?.asBoolean==false}

        stage("notifications")
        app.preferences.notificationMaster(true)
        app.preferences.notificationSelection(false,NotificationConsent.app(android.os.Process.myUserHandle().hashCode(),i.context.packageName),true)
        post()
        until {it.getAsJsonArray("notices").any {n->n.asJsonObject["text"]?.asString=="Interop one"}}
        post("Interop update")
        val updated=until {it.getAsJsonArray("notices").any {n->n.asJsonObject["text"]?.asString=="Interop update"}}
        val notices=updated.getAsJsonArray("notices")
        assertEquals(notices[0].asJsonObject["id"],notices[1].asJsonObject["id"])
        post(cancel=true);until {it["removed"].asInt>=1}
        post("Interop clear");until {it.getAsJsonArray("notices").size()>=3}
        app.preferences.notificationMaster(false);until {it["removed"].asInt>=2}

        stage("resume prefix")
        val id=randomId();val file=java.io.File(c.filesDir,"outgoing/$id.bin")
        file.parentFile!!.mkdirs();file.writeBytes(ByteArray(8*1024*1024) {(it%251).toByte()})
        val hash=sha256(file.readBytes())
        val row=Outgoing().apply {mediaId="interop:$id";transferId=id;peerId=pc.deviceId;bindingGeneration=pc.generation
            path=file.path;kind="file";automatic=false;size=file.length();fileName="interop-resume.bin";mime="application/octet-stream"
            this.hash=hash;createdAt=System.currentTimeMillis()}
        val entered=CompletableDeferred<Unit>();val release=CountDownLatch(1).also {releases+=it};val frames=AtomicInteger()
        app.connection.frameFlushedHookForTest {transfer->if(transfer==id && frames.incrementAndGet()==17) {
            entered.complete(Unit);check(release.await(20,TimeUnit.SECONDS))
        }}
        DeviceFixture.insert(app,row);app.connection.kick()
        withTimeout(15000) {entered.await()}
        val checkpoint=withTimeout(10000) {
            var offset=rpc("checkpoint")["offset"].asLong
            while(offset<4*1024*1024) {delay(30);offset=rpc("checkpoint")["offset"].asLong}
            offset
        }
        stage("resume checkpoint $checkpoint")
        app.connection.stop();release.countDown();app.connection.awaitStopped()
        val pending=app.queue.delivery(row.mediaId,pc.deviceId)
        pending.state="failed";pending.error="connection_lost";pending.nextAttempt=Long.MAX_VALUE;app.queue.update(pending)
        val suffix=CompletableDeferred<Unit>();val suffixRelease=CountDownLatch(1).also {releases+=it};val resumedFrames=AtomicInteger()
        stage("resume reconnect")
        app.connection.start(app.scope)
        withTimeout(15000) {while(!app.connection.connected(pc.deviceId))delay(20)}
        // The first new worker inherits test hooks only when installed explicitly.
        app.connection.frameFlushedHookForTest {transfer->if(transfer==id && resumedFrames.incrementAndGet()==2) {
            suffix.complete(Unit);check(suffixRelease.await(20,TimeUnit.SECONDS))
        }}
        // Explicit retry starts only after the new worker's frame hook is installed.
        assertTrue(app.retry(row.mediaId,ConfirmedQueue.from(app.preferences.read(),listOf(pending))))
        withTimeout(15000) {suffix.await()}
        stage("resume suffix")
        val resumedOffset=withTimeout(5000) {app.connection.devices.first {it[pc.deviceId]?.sent?.let {n->n>=checkpoint}==true}[pc.deviceId]!!.sent}
        assertTrue(resumedOffset>=checkpoint)
        suffixRelease.countDown()
        withTimeout(20000) {while(app.queue.delivery(row.mediaId,pc.deviceId).state!="sent")delay(30)}
        stage("resume complete")
        stage("folder capability")
        until {state->state.getAsJsonObject("snapshot").getAsJsonArray("send_targets").any {
            it.asJsonObject["device"].asString==phone && it.asJsonObject["folder_reason"].asString=="Can't receive folders yet"
        }}
        assertTrue(app.connection.supports(pc.deviceId,CAP_FOLDER))
        val tree=DocumentsContract.buildTreeDocumentUri("io.github.stantonmatt.pneu.test.folders","root")
        c.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folder-grants"),"grant",null,null)
        fun fixture(method:String)=c.contentResolver.call(Uri.parse("content://io.github.stantonmatt.pneu.test.folders"),method,"abc",null)
        fun authority()=CapturedAuthority(requireNotNull(DeliveryBinding.from(app.prefs.value,pc.deviceId)))
        stage("folder send")
        fixture("fixture")
        app.folderSends.send(tree,authority())
        withTimeout(15000) {while(app.queue.recentDeliveries().first().none {it.isFolder() && it.state=="sent"})delay(30)}
        rpc("folder_received",json("large" to false))

        stage("folder resume prefix")
        fixture("large-fixture")
        val folderEntered=CompletableDeferred<String>();val folderRelease=CountDownLatch(1).also {releases+=it};val folderFrames=AtomicInteger()
        // Folder streams can flush the manifest separately from file content.
        // Pause after enough content frames to cross the durable 4 MiB threshold.
        app.connection.frameFlushedHookForTest {transfer->if(folderFrames.incrementAndGet()==19) {
            folderEntered.complete(transfer);check(folderRelease.await(20,TimeUnit.SECONDS))
        }}
        app.folderSends.send(tree,authority())
        val folderId=withTimeout(15000) {folderEntered.await()}
        val folderCheckpoint=withTimeout(10000) {
            var offset=rpc("checkpoint",json("transfer" to folderId))["offset"].asLong
            while(offset<4*1024*1024) {delay(30);offset=rpc("checkpoint",json("transfer" to folderId))["offset"].asLong}
            offset
        }
        app.connection.stop();folderRelease.countDown();app.connection.awaitStopped()
        val folderPending=app.queue.recentDeliveries().first().single {it.transferId==folderId}
        folderPending.state="failed";folderPending.error="connection_lost";folderPending.nextAttempt=Long.MAX_VALUE;app.queue.update(folderPending)
        stage("folder resume reconnect")
        app.connection.start(app.scope)
        withTimeout(15000) {while(!app.connection.connected(pc.deviceId))delay(20)}
        val folderSuffix=CompletableDeferred<Unit>();val folderSuffixRelease=CountDownLatch(1).also {releases+=it};val folderResumedFrames=AtomicInteger()
        app.connection.frameFlushedHookForTest {transfer->if(transfer==folderId && folderResumedFrames.incrementAndGet()==2) {
            folderSuffix.complete(Unit);check(folderSuffixRelease.await(20,TimeUnit.SECONDS))
        }}
        assertTrue(app.retry(folderPending.mediaId,ConfirmedQueue.from(app.preferences.read(),listOf(folderPending))))
        withTimeout(15000) {folderSuffix.await()}
        val folderResumedOffset=withTimeout(5000) {app.connection.devices.first {it[pc.deviceId]?.sent?.let {n->n>=folderCheckpoint}==true}[pc.deviceId]!!.sent}
        folderSuffixRelease.countDown()
        withTimeout(20000) {while(app.queue.delivery(folderPending.mediaId,pc.deviceId).state!="sent")delay(30)}
        stage("folder resume complete")
        rpc("folder_received",json("large" to true,"checkpoint" to folderCheckpoint,"resumedOffset" to folderResumedOffset,"sha256" to hash))
        rpc("done",json("sha256" to hash,"resumedOffset" to resumedOffset))
    }
}
