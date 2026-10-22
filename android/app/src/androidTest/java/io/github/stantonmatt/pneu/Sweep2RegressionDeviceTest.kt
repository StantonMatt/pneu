package io.github.stantonmatt.pneu

import android.app.Application
import android.content.Intent
import androidx.datastore.preferences.core.edit
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class Sweep2RegressionDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val c=InstrumentationRegistry.getInstrumentation().targetContext
    private val app=c.graph
    private val a="a".repeat(32);private val b="b".repeat(32)
    private val fixtures=mutableListOf<LocalPeerFixture>()
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset();app.initialized.await()
        app.preferences.toggle("setup",true)
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,false)
        app.preferences.store.edit {p->PeerRegistry.write(p,listOf(Peer("Desktop","a".repeat(64),a,emptyList(),generation=1),Peer("MacBook","b".repeat(64),b,emptyList(),generation=2)))}
        withTimeout(5000) {app.prefs.first {it.devices.size==2}}
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeMutation={}
        val db=app.database.openHelper.writableDatabase
        for(trigger in listOf("fail_clip_complete","fail_clip_reserve","fail_clip_cleanup","fail_file_recovery"))db.execSQL("DROP TRIGGER IF EXISTS $trigger")
        DeviceFixture.stopPneuService();app.connection.stop();app.connection.awaitStopped()
        fixtures.forEach {it.close()};DeviceFixture.reset()
    }
    @Test(timeout=45000) fun receiptJournalRecoversLegacyAndMeshCompletionAndRetainsConflict()=runBlocking<Unit> {
        val db=app.database.openHelper.writableDatabase
        for(mesh in listOf(false,true)) {
            var effects=0
            val delivery=ClipboardDelivery(app) {_,_->effects++}
            val stamp=app.preferences.originateClipboard(false)
            val body=json("transfer_id" to randomId(),"text" to "Journal $mesh","created_at" to System.currentTimeMillis(),"origin" to "clipboard")
            if(mesh) {body.addProperty("hlc",stamp.hlc);body.addProperty("source_device",a)}
            val id=body["transfer_id"].asString
            val binding=DeliveryBinding.from(app.preferences.read(),a)
            db.execSQL("CREATE TRIGGER fail_clip_complete BEFORE UPDATE ON receipts WHEN NEW.state='applied' BEGIN SELECT RAISE(ABORT,'injected completion failure'); END")
            val failed=delivery.receive(a,body,binding,mesh)
            assertEquals(Type.NACK,failed.type);assertEquals("storage_failed",failed.body["code"].asString)
            assertEquals(1,effects);assertEquals("reserved",app.queue.receipt(a,id).state)
            // A new receiver instance also recovers the durable marker.
            val restarted=ClipboardDelivery(app) {_,_->effects++}
            assertEquals(Type.NACK,restarted.receive(a,body,binding,mesh).type)
            assertEquals(1,effects)
            // A sender may supersede the failed ID. New traffic must first
            // finish the old durable effect, rather than accumulating journals.
            val next=body.deepCopy().apply {addProperty("transfer_id",randomId());addProperty("text","Next")}
            assertEquals(Type.NACK,restarted.receive(a,next,binding,mesh).type)
            assertEquals(1,effects)
            db.execSQL("DROP TRIGGER fail_clip_complete")
            val replay=restarted.receive(a,body,binding,mesh)
            assertEquals(Type.ACK,replay.type);assertTrue(replay.body["deduplicated"].asBoolean)
            assertEquals("applied",app.queue.receipt(a,id).state);assertEquals(1,effects)
            val changed=restarted.receive(a,body.deepCopy().apply {addProperty("text","Changed")},binding,mesh)
            assertEquals(Type.NACK,changed.type);assertEquals("id_conflict",changed.body["code"].asString)
            assertEquals(1,effects)
        }
    }
    @Test(timeout=45000) fun reservationFailsBeforeEffectAndAppliedBookkeepingRetriesWithoutRepeatingIt()=runBlocking<Unit> {
        val db=app.database.openHelper.writableDatabase
        var effects=0;val delivery=ClipboardDelivery(app) {_,_->effects++}
        val body=json("transfer_id" to randomId(),"text" to "Before effect","created_at" to System.currentTimeMillis(),"origin" to "clipboard")
        db.execSQL("CREATE TRIGGER fail_clip_reserve BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT,'injected reservation failure'); END")
        assertEquals(Type.NACK,delivery.receive(a,body).type);assertEquals(0,effects)
        db.execSQL("DROP TRIGGER fail_clip_reserve")
        val before=app.preferences.read().lastReceivedHash
        app.preferences.beforeMutation={if(it=="clipboardApplied")throw java.io.IOException("injected preferences failure")}
        assertEquals(Type.NACK,delivery.receive(a,body).type);assertEquals(1,effects)
        assertEquals(before,app.preferences.store.data.first()[androidx.datastore.preferences.core.stringPreferencesKey("lastReceivedHash")].orEmpty())
        app.preferences.beforeMutation={}
        assertEquals(Type.ACK,delivery.receive(a,body).type);assertEquals(1,effects)
        assertEquals(sha256("Before effect".toByteArray()),app.preferences.read().lastReceivedHash)
        assertEquals("id_conflict",delivery.receive(a,body.deepCopy().apply {addProperty("text","Conflict")}).body["code"].asString)
    }
    @Test(timeout=45000) fun unfinishedEffectFencesOlderIdsFromEitherComputerAndRecoversWithoutRetry()=runBlocking<Unit> {
        var effects=0;var value=""
        val firstReceiver=ClipboardDelivery(app) {text,_->effects++;value=text}
        val order=System.currentTimeMillis().toBigInteger()+1000.toBigInteger()
        fun body(peer:String,stamp:java.math.BigInteger,text:String)=json("transfer_id" to randomId(),"text" to text,"created_at" to System.currentTimeMillis(),"origin" to "clipboard","hlc" to stamp,"source_device" to peer)
        val first=body(a,order,"Newest")
        val binding=DeliveryBinding.from(app.preferences.read(),a)
        app.preferences.beforeMutation={if(it=="clipboardApplied")throw java.io.IOException("injected preferences failure")}
        assertEquals(Type.NACK,firstReceiver.receive(a,first,binding,true).type)
        assertEquals(1,effects)
        app.preferences.beforeMutation={}
        val nextReceiver=ClipboardDelivery(app) {text,_->effects++;value=text}
        for(peer in listOf(a,b)) {
            val response=nextReceiver.receive(peer,body(peer,order-1.toBigInteger(),"Older"),DeliveryBinding.from(app.preferences.read(),peer),true)
            assertEquals(Type.ACK,response.type);assertTrue(response.body["deduplicated"].asBoolean)
            assertEquals("Newest",value);assertEquals(1,effects)
        }
        assertEquals("applied",app.queue.receipt(a,first["transfer_id"].asString).state)
        assertTrue(app.preferences.unfinishedClipboardCompletions(a,1).isEmpty())
        assertEquals(Type.ACK,firstReceiver.receive(a,first,binding,true).type)
        assertEquals("Newest",value);assertEquals(1,effects)
        assertEquals(Type.ACK,nextReceiver.receive(b,body(b,order+1.toBigInteger(),"Later"),DeliveryBinding.from(app.preferences.read(),b),true).type)
        assertEquals("Later",value);assertEquals(2,effects)
    }
    @Test(timeout=45000) fun abandonedEffectOwnershipFencesEchoAndResendAfterUnrelatedStorageSuccess()=runBlocking<Unit> {
        val text="Recovered ownership ${randomId()}";val hash=sha256(text.toByteArray())
        val body=json("transfer_id" to randomId(),"text" to text,"created_at" to System.currentTimeMillis(),"origin" to "clipboard")
        val binding=DeliveryBinding.from(app.preferences.read(),a)
        app.preferences.beforeMutation={if(it=="clipboardApplied")throw java.io.IOException("injected preferences failure")}
        assertEquals(Type.NACK,ClipboardDelivery(app) {_,_->}.receive(a,body,binding).type)
        app.preferences.beforeMutation={}
        app.preferences.setString("selfName","Phone")
        val p=app.preferences.read();val captured=p.devices.mapNotNull {DeliveryBinding.from(p,it.deviceId)}
        val restored=android.content.ClipData.newPlainText("restored",text)
        assertEquals(a,p.recentClipboardSources[hash])
        assertTrue(ClipboardSendPolicy.targets(restored,p,captured,automatic=true).isEmpty())
        assertEquals(listOf(b),ClipboardSendPolicy.targets(restored,p,captured).map {it.pcId})
        app.clipboard.reconcile()
        assertEquals("applied",app.queue.receipt(a,body["transfer_id"].asString).state)
    }
    @Test(timeout=45000) fun cleanupStorageFailureStillDisconnectsAndRetriesDurableAbandonment()=runBlocking<Unit> {
        val fixture=LocalPeerFixture(app,setOf("files","clipboard_text","clipboard_image","mesh")).also {fixtures+=it}
        fixture.connect(a)
        val binding=DeliveryBinding.from(app.preferences.read(),a)!!
        val row=Outgoing().apply {mediaId=randomId();transferId=randomId();kind="text";automatic=false;text="Never replay";createdAt=System.currentTimeMillis();recipients=listOf(binding)}
        assertTrue(app.enqueueFixture(row))
        val db=app.database.openHelper.writableDatabase
        db.execSQL("CREATE TRIGGER fail_clip_cleanup BEFORE UPDATE ON deliveries WHEN NEW.state='superseded' BEGIN SELECT RAISE(ABORT,'injected cleanup failure'); END")
        fixture.close();fixtures.remove(fixture)
        withTimeout(5000) {app.connection.devices.first {it[a]?.status!="Connected"}}
        assertFalse(app.connection.connected(a));assertFalse(app.connection.supports(a,"clipboard_text"))
        assertEquals("waiting",app.queue.delivery(row.mediaId,a).state)
        db.execSQL("DROP TRIGGER fail_clip_cleanup");app.connection.kick()
        withTimeout(10000) {while(app.queue.delivery(row.mediaId,a).state!="superseded")delay(20)}
    }
    @Test(timeout=45000) fun failingFileLaneRecoveryStillRecoversTheTextLane()=runBlocking<Unit> {
        val binding=DeliveryBinding.from(app.preferences.read(),a)!!
        val rows=listOf("file","text").map {kind->Outgoing().apply {mediaId=randomId();transferId=randomId();peerId=a;bindingGeneration=binding.generation;this.kind=kind;automatic=false;state="sending";createdAt=System.currentTimeMillis()}}
        rows.forEach {app.queue.insert(it)}
        val db=app.database.openHelper.writableDatabase
        db.execSQL("CREATE TRIGGER fail_file_recovery BEFORE UPDATE ON deliveries WHEN NEW.state='waiting' AND NEW.mediaId='${rows[0].mediaId}' BEGIN SELECT RAISE(ABORT,'injected lane recovery failure'); END")
        val worker=io.github.stantonmatt.pneu.connection.ConnectionManager(app)
        assertFalse(worker.recoverSession(binding))
        assertEquals("waiting",app.queue.delivery(rows[1].mediaId,a).state)
        assertEquals("sending",app.queue.delivery(rows[0].mediaId,a).state)
        db.execSQL("DROP TRIGGER fail_file_recovery")
        assertTrue(worker.recoverSession(binding))
        assertEquals("waiting",app.queue.delivery(rows[0].mediaId,a).state)
    }
    @Test(timeout=45000) fun survivingOsMarkerRecoversPreferencesFailureWithANewReceiver()=runBlocking<Unit> {
        ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("home",true)).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("Recent").fetchSemanticsNodes().isNotEmpty()}
            val manager=c.getSystemService(android.content.ClipboardManager::class.java)
            try {
                val body=json("transfer_id" to randomId(),"text" to "OS journal ${randomId()}","created_at" to System.currentTimeMillis(),"origin" to "clipboard")
                app.preferences.beforeMutation={if(it=="clipboardApplied")throw java.io.IOException("injected preferences failure")}
                assertEquals(Type.NACK,ClipboardDelivery(app).receive(a,body).type)
                val marker=withContext(Dispatchers.Main) {manager.primaryClipDescription?.extras}
                assertNotNull("OS marker missing",marker)
                assertEquals(body["transfer_id"].asString,marker!!.getString("io.github.stantonmatt.pneu.clipboard.transfer"))
                assertEquals(a,marker.getString(ClipboardDelivery.OWN_SOURCE))
                assertEquals(1L,marker.getLong("io.github.stantonmatt.pneu.clipboard.generation"))
                assertTrue(marker.getString("io.github.stantonmatt.pneu.clipboard.order").orEmpty().isNotEmpty())
                assertEquals("reserved",app.queue.receipt(a,body["transfer_id"].asString).state)
                app.preferences.beforeMutation={}
                ClipboardDelivery(app).reconcile()
                assertEquals(sha256(body["text"].asString.toByteArray()),app.preferences.read().lastReceivedHash)
                // Android may publish a second callback when classification
                // finishes. Compare the OS effect's order marker instead: an
                // actual second legacy write would allocate a different order.
                val after=withContext(Dispatchers.Main) {manager.primaryClipDescription?.extras}
                assertEquals(marker.getString("io.github.stantonmatt.pneu.clipboard.order"),after?.getString("io.github.stantonmatt.pneu.clipboard.order"))
                assertEquals("applied",app.queue.receipt(a,body["transfer_id"].asString).state)
            }finally {app.preferences.beforeMutation={}}
        }
    }
    @Test(timeout=45000) fun startupReconcilesSurvivingMarkerWithoutAnyInMemoryEffectOrIdRetry()=runBlocking<Unit> {
        ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("home",true)).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("Recent").fetchSemanticsNodes().isNotEmpty()}
            val manager=c.getSystemService(android.content.ClipboardManager::class.java)
            val text="Surviving startup clip ${randomId()}";val hash=sha256(text.toByteArray());val id=randomId()
            val order=System.currentTimeMillis().toBigInteger()+1000.toBigInteger()
            // Reconstruct the exact crash boundary: Room reserved, OS changed,
            // no DataStore register and no process-local effect cache.
            app.queue.insertReceipt(Receipt().apply {peerId=a;transferId=id;this.hash=hash;createdAt=System.currentTimeMillis();origin="clipboard";hlc=order.toString();sourceDevice=a;state="reserved"})
            withContext(Dispatchers.Main) {manager.setPrimaryClip(ClipboardDelivery.receivedClip("Pneu",text,hash,a,id,order.toString(),1))}
            assertNull(app.preferences.clipboardEffect(a,1,id))
            ClipboardDelivery(app).reconcile() // AppGraph's startup recovery entry point.
            assertEquals("applied",app.queue.receipt(a,id).state)
            assertEquals(hash,app.preferences.read().lastReceivedHash)
            assertFalse(app.preferences.clipboardWins(ClipboardStamp(order-1.toBigInteger(),b)))
            assertTrue(app.newTextOrder().first.hlc>order)
            assertEquals(order.toString(),withContext(Dispatchers.Main) {manager.primaryClipDescription!!.extras!!.getString("io.github.stantonmatt.pneu.clipboard.order")})
        }
    }
    @Test(timeout=45000) fun recentShowsPartialOutcomeAndAggregatedProgressThroughViewModel()=runBlocking<Unit> {
        val rows=listOf(a,b).map {peer->Outgoing().apply {mediaId="aggregate";transferId=randomId();peerId=peer;bindingGeneration=if(peer==a)1 else 2;kind="file";automatic=false;fileName="Both.bin";size=100;state=if(peer==a)"sent" else "waiting"}}
        rows.forEach {app.queue.insert(it)}
        val vm=withContext(Dispatchers.Main) {RecentViewModel(c.applicationContext as Application)}
        val collector=launch {vm.items.collect()}
        try {
            val item=withTimeout(5000) {vm.items.first {it.any {item->item.row.mediaId=="aggregate"}}}.single()
            assertEquals(100L,item.sent);assertEquals(200L,item.total);assertEquals(2,item.recipients.size)
            ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("home",true)).use {
                compose.waitUntil(10000) {compose.onAllNodesWithText("Sent to 1 of 2").fetchSemanticsNodes().isNotEmpty()}
                app.queue.update(rows[1].apply {state="failed";error="disk_full"})
                compose.waitUntil(5000) {compose.onAllNodesWithText("Failed on MacBook").fetchSemanticsNodes().isNotEmpty()}
                compose.onNodeWithContentDescription("Retry").assertExists()
            }
        }finally {collector.cancelAndJoin();withContext(Dispatchers.Main) {androidx.lifecycle.ViewModelStore().apply {put("recent",vm);clear()}}}
    }
}
