package io.github.stantonmatt.pneu

import android.app.Application
import android.content.*
import android.net.Uri
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.datastore.preferences.core.edit
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
class MergeRegressionDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val c=InstrumentationRegistry.getInstrumentation().targetContext
    private val app=c.graph
    private val a="a".repeat(32);private val b="b".repeat(32)
    private val fixtures=mutableListOf<LocalPeerFixture>()
    private val owned=mutableListOf<Uri>()
    private val clipboard get()=c.getSystemService(ClipboardManager::class.java)
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset();app.initialized.await()
        app.preferences.toggle("setup",true)
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,false)
        app.preferences.store.edit {p->PeerRegistry.write(p,listOf(
            Peer("Desktop","a".repeat(64),a,emptyList(),generation=1),
            Peer("MacBook","b".repeat(64),b,emptyList(),generation=2)))}
        withTimeout(5000) {app.prefs.first {it.devices.size==2}}
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeMutation={};app.staging.beforeAutomaticPayloadWrite={}
        DeviceFixture.stopPneuService();app.connection.stop();app.connection.awaitStopped()
        fixtures.forEach {it.close()};owned.forEach {c.contentResolver.delete(it,null,null)}
        DeviceFixture.reset()
    }
    private fun home()=ActivityScenario.launch<MainActivity>(Intent(c,MainActivity::class.java).putExtra("home",true))
    private suspend fun online() {
        for(peer in listOf(a,b))LocalPeerFixture(app,setOf("files","clipboard_text","clipboard_image","mesh")).also {fixtures+=it;it.connect(peer)}
    }
    private suspend fun receivedText(text:String,peer:String=a):Pair<Frame.Control,com.google.gson.JsonObject> {
        val order=app.newTextOrder(apply=false)
        val body=json("transfer_id" to randomId(),"text" to text,"created_at" to order.second,"origin" to "clipboard",
            "hlc" to order.first.hlc,"source_device" to peer)
        return app.clipboard.receive(peer,body,DeliveryBinding.from(app.preferences.read(),peer),mesh=true) to body
    }
    private suspend fun sendClipboard(clip:ClipData,admission:FrozenAdmission?=null):ManualEntryViewModel {
        val captured=admission?.let {CompletableDeferred(it)}
            ?: ManualEntries.captureClipboard(app,ManualEntries.clipboardIntent(c,app.preferences.read()))
        return withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.captureClipboardAdmission(captured);it.clipboard(clip)
        }}.also {operation->withTimeout(10000) {operation.result.filterNotNull().first()}}
    }
    @Test(timeout=60000) fun shareAndPickerExcludeTheReceivedSourceAndClipboardActionsUseTheSamePolicy()=runBlocking<Unit> {
        online()
        home().use {scenario->
            scenario.onActivity {clipboard.setPrimaryClip(ClipData.newPlainText("local","different"))}
            val text="Shared received text ${randomId()}"
            assertEquals(Type.ACK,receivedText(text).first.type)
            val frozen=app.captureAdmission().await()
            for(origin in listOf("share","clipboard")) {
                val row=ManualEntries.text(app,text,origin,frozen.binding,frozen.text)
                assertEquals(listOf(b),app.queue.deliveries(row.mediaId).map {it.peerId})
            }
            app.preferences.toggle("chooseDevicesEachTime",true)
            val operation=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
                it.targetPickerAvailable=true
                it.share(ShareRequest(CompletableDeferred(frozen.binding),emptyList(),listOf(text),CompletableDeferred(frozen)))
            }}
            try {
                val offered=withTimeout(5000) {operation.targetSelection.filterNotNull().first()}
                assertEquals(listOf(b),offered.map {it.deviceId})
                operation.selectTargets(setOf(b))
                withTimeout(10000) {operation.result.filterNotNull().first()}
                assertEquals(listOf(b),app.queue.deliveries(operation.completed.single()!!).map {it.peerId})
            }finally {operation.cancelTargetSelection()}
            // The tile and notification action both carry this frozen intent.
            val intent=ManualEntries.clipboardIntent(c,app.preferences.read())
            val action=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
                it.captureClipboardAdmission(ManualEntries.captureClipboard(app,intent));it.clipboard(clipboard.primaryClip)
            }}
            withTimeout(10000) {action.result.filterNotNull().first()}
            assertEquals(listOf(b),app.queue.deliveries(action.completed.single()!!).map {it.peerId})
            scenario.onActivity {clipboard.clearPrimaryClip()}
        }
    }
    private suspend fun file(bytes:ByteArray,mime:String,peer:String=a):Pair<ReceivedFile,ReceivedMetadata> {
        val order=app.newTextOrder(apply=false)
        val metadata=ReceivedMetadata(randomId(),"merge-${randomId()}.${if(mime.startsWith("image/"))"png" else "txt"}",mime,bytes.size.toLong(),sha256(bytes),"file",order.second,
            hlc=if(mime.startsWith("image/"))order.first.hlc.toString() else null,source=if(mime.startsWith("image/"))peer else null)
        val receiver=app.receivedFiles.connection(requireNotNull(DeliveryBinding.from(app.preferences.read(),peer)),true) {true}
        try {
            receiver.begin(com.google.gson.JsonParser.parseString(metadata.canonical).asJsonObject)
            receiver.chunk(Frame.Chunk(metadata.id,0,bytes));assertEquals(Type.ACK,receiver.end(metadata.id).type)
        }finally {receiver.disconnect()}
        val row=app.queue.receivedFile(peer,metadata.id);owned+=Uri.parse(row.uri)
        return row to metadata
    }
    @Test(timeout=60000) fun sharedTextFilesAndImagesFilterTheirStagedBytesIncludingExplicitTargets()=runBlocking<Unit> {
        online()
        val image=android.util.Base64.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==",0)
        for((bytes,mime) in listOf("received text file".toByteArray() to "text/plain","received text without MIME".toByteArray() to "application/octet-stream",image to "image/png")) {
            val (saved,metadata)=file(bytes,mime)
            val stamp=ClipboardStamp(metadata.hlc?.toBigInteger() ?: app.newTextOrder(apply=false).first.hlc,a)
            app.preferences.appliedClipboard(stamp,metadata.hash,a,saved.generation,saved.transferId)
            val p=app.preferences.read();val binding=DeliveryBinding.from(p)
            for(kind in if(mime.startsWith("image/"))listOf("file","clipboard_image") else listOf("file")) {
                val targets=app.recipients(p,kind,selected=if(kind=="file")setOf(a,b) else null)
                val row=ManualEntries.file(app,Uri.parse(saved.uri),kind,binding,targets=targets)
                assertEquals(listOf(b),app.queue.deliveries(row.mediaId).map {it.peerId})
            }
            val onlySource=app.recipients(app.preferences.read(),"file",selected=setOf(a))
            val before=app.queue.all().map {it.mediaId}.toSet()
            try {ManualEntries.file(app,Uri.parse(saved.uri),"file",binding,targets=onlySource);fail("Source-only selection must not publish")}
            catch(e:ShareFailure) {assertEquals("no_recipient",e.reason)}
            assertEquals(before,app.queue.all().map {it.mediaId}.toSet())
        }
    }

    @Test(timeout=60000) fun ordinaryReceivedFileNotificationShareAndPickerExcludeSourceWithoutClipboardHistory()=runBlocking<Unit> {
        online()
        val (saved,metadata)=file("Ordinary download ${randomId()}".toByteArray(),"application/octet-stream")
        assertFalse(metadata.hash in app.preferences.read().recentClipboardSources)
        assertNotEquals(metadata.hash,app.preferences.read().lastReceivedHash)
        val shared=ReceivedFileActions.resolve(c,saved,true)!!
        val uri=shared.getParcelableExtra(Intent.EXTRA_STREAM,Uri::class.java)!!
        assertEquals(saved.uri,uri.toString())
        val frozen=app.captureAdmission().await()
        val row=ManualEntries.file(app,uri,"file",frozen.binding,targets=frozen.files)
        assertEquals(listOf(b),app.queue.deliveries(row.mediaId).map {it.peerId})
        val alias=Uri.parse("content://media/external/file/${ContentUris.parseId(uri)}")
        val aliased=ManualEntries.file(app,alias,"file",frozen.binding,targets=frozen.files)
        assertEquals(listOf(b),app.queue.deliveries(aliased.mediaId).map {it.peerId})
        val onlySource=listOf(CapturedAuthority(frozen.files.first {it.pcId==a}))
        val rejected=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.share(ShareRequest(CompletableDeferred(frozen.binding),listOf(uri),emptyList(),explicitTargets=onlySource))
        }}
        assertEquals(c.getString(R.string.toast_no_recipient),withTimeout(10000) {rejected.result.filterNotNull().first()}.message)
        assertTrue(rejected.completed.isEmpty())
        app.preferences.toggle("chooseDevicesEachTime",true)
        val picker=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.targetPickerAvailable=true
            it.share(ShareRequest(CompletableDeferred(frozen.binding),listOf(uri),emptyList(),CompletableDeferred(frozen)))
        }}
        try {
            assertEquals(listOf(b),withTimeout(5000) {picker.targetSelection.filterNotNull().first()}.map {it.deviceId})
            picker.selectTargets(setOf(b));withTimeout(10000) {picker.result.filterNotNull().first()}
            assertEquals(listOf(b),app.queue.deliveries(picker.completed.single()!!).map {it.peerId})
        }finally {picker.cancelTargetSelection()}
    }
    @Test(timeout=60000) fun sourceOnlySharedTextAndClipboardTileAndNotificationActionsTerminateWithFeedback()=runBlocking<Unit> {
        online()
        home().use {scenario->
            scenario.onActivity {clipboard.setPrimaryClip(ClipData.newPlainText("local","before source-only"))}
            val text="source-only ${randomId()}";assertEquals(Type.ACK,receivedText(text).first.type)
            val frozen=app.captureAdmission().await().let {it.copy(text=it.text.filter {target->target.pcId==a},images=it.images.filter {target->target.pcId==a})}
            val shared=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
                it.share(ShareRequest(CompletableDeferred(frozen.binding),emptyList(),listOf(text),CompletableDeferred(frozen)))
            }}
            assertEquals(c.getString(R.string.toast_no_recipient),withTimeout(5000) {shared.result.filterNotNull().first()}.message)
            // Both internal entry points deserialize the same frozen intent.
            val intent=ManualEntries.clipboardIntent(c,app.preferences.read()).putExtra("pneu.admission.targets",com.google.gson.Gson().toJson(frozen))
            repeat(2) {
                val action=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
                    it.captureClipboardAdmission(ManualEntries.captureClipboard(app,intent));it.clipboard(clipboard.primaryClip)
                }}
                assertEquals(c.getString(R.string.toast_no_recipient),withTimeout(5000) {action.result.filterNotNull().first()}.message)
                assertTrue(action.completed.isEmpty())
            }
            assertTrue(app.queue.all().isEmpty())
            scenario.onActivity {clipboard.clearPrimaryClip()}
        }
    }

    @Test(timeout=60000) fun stagedSourceOnlyShareAndRawClipboardImageTerminateAndSurvivingRecipientAdmits()=runBlocking<Unit> {
        online()
        val bytes=android.util.Base64.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==",0)
        val stamp=app.newTextOrder(apply=false).first
        app.preferences.appliedClipboard(ClipboardStamp(stamp.hlc,a),sha256(bytes),a,1,randomId())
        val uri=observedImage(bytes)
        val frozen=app.captureAdmission().await().let {it.copy(text=it.text.filter {target->target.pcId==a},images=it.images.filter {target->target.pcId==a},files=it.files.filter {target->target.pcId==a})}
        val share=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.share(ShareRequest(CompletableDeferred(frozen.binding),listOf(uri),emptyList(),CompletableDeferred(frozen)))
        }}
        assertEquals(c.getString(R.string.toast_no_recipient),withTimeout(10000) {share.result.filterNotNull().first()}.message)
        assertEquals(listOf<String?>(null),share.completed)
        val action=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.captureClipboardAdmission(CompletableDeferred(frozen));it.clipboard(ClipData.newRawUri("marker stripped",uri))
        }}
        assertEquals(c.getString(R.string.toast_no_recipient),withTimeout(10000) {action.result.filterNotNull().first()}.message)
        assertTrue(action.completed.isEmpty());assertTrue(app.queue.all().isEmpty())
        assertTrue(java.io.File(c.filesDir,"outgoing").listFiles().orEmpty().none {it.extension in setOf("bin","part")})
        val row=ManualEntries.file(app,uri,"file",frozen.binding,targets=app.captureAdmission().await().files)
        assertEquals(listOf(b),app.queue.deliveries(row.mediaId).map {it.peerId})
    }
    private fun observedImage(bytes:ByteArray,photo:Boolean=false):Uri {
        val uri=c.contentResolver.insert(android.provider.MediaStore.Images.Media.getContentUri("external_primary"),ContentValues().apply {
            put("_display_name","merge-echo-${randomId()}.png");put("mime_type","image/png")
            put("relative_path",if(photo)"DCIM/Camera/" else "Pictures/Screenshots/");put("is_pending",1)
        })!!
        owned+=uri;c.contentResolver.openOutputStream(uri)!!.use {it.write(bytes)}
        c.contentResolver.update(uri,ContentValues().apply {put("is_pending",0)},null,null)
        return uri
    }
    private fun mediaKey(uri:Uri)="external_primary:${android.provider.MediaStore.getVersion(c,"external_primary")}:images:${ContentUris.parseId(uri)}"
    @Test(timeout=60000) fun suppressedAutomaticEchoIsRetiredOnceAndLaterDiscoveryIsDelivered()=runBlocking<Unit> {
        // ACK actual file transfers after observing them over the real TLS fixture.
        for(peer in listOf(a,b))LocalPeerFixture(app,setOf("files","clipboard_text","clipboard_image","mesh"),handle={frame,send->
            if(frame.type==Type.FILE_END) {send(Frame.Control(Type.ACK,json("transfer_id" to frame.body["transfer_id"].asString,"deduplicated" to false)));true} else false
        }).also {fixtures+=it;it.connect(peer,holdFirstFrame=false)}
        app.preferences.toggle("queue",true);app.preferences.toggle("originals",true)
        for(key in listOf("screenshots","photos"))app.preferences.toggle(key,true)
        val observer=io.github.stantonmatt.pneu.media.ScreenshotObserver(app,connectivity={io.github.stantonmatt.pneu.connection.Connectivity.UNMETERED})
        observer.scanOnce();observer.scanOnce()
        val bytes=android.util.Base64.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==",0)
        val received=app.newTextOrder(apply=false).first
        app.preferences.appliedClipboard(ClipboardStamp(received.hlc,a),sha256(bytes),a,1,randomId())
        for(photo in listOf(false,true)) {
            val echo=observedImage(bytes,photo);val id=mediaKey(echo)
            app.staging.beforeAutomaticPayloadWrite={if(it.mediaId==id)throw java.io.IOException("Retry regression fixture")}
            observer.scanOnce();assertNotNull(app.queue.pendingAdmission(id))
            val later=observedImage(bytes+byteArrayOf(if(photo)2 else 1),photo);val laterId=mediaKey(later)
            var echoWrites=0
            app.staging.beforeAutomaticPayloadWrite={if(it.mediaId==id)echoWrites++}
            val refresh=app.mediaRefresh.value
            observer.scanOnce()
            assertEquals(1,app.queue.seen(id));assertNull(app.queue.pendingAdmission(id));assertNull(app.queue.find(id))
            assertEquals(refresh,app.mediaRefresh.value)
            withTimeout(15000) {while(app.queue.deliveries(laterId).let {it.size!=2 || it.any {delivery->delivery.state!="sent"}})delay(20)}
            assertEquals(setOf(a,b),app.queue.deliveries(laterId).map {it.peerId}.toSet())
            val attempts=echoWrites;observer.scanOnce();assertEquals(attempts,echoWrites)
            assertTrue(app.queue.pendingAdmissions().isEmpty())
            val referenced=app.queue.all().map {it.path}.toSet()
            assertTrue(java.io.File(c.filesDir,"outgoing").listFiles().orEmpty().none {it.extension=="bin" && it.path !in referenced})
        }
    }
    @Test(timeout=60000) fun identicalCurrentTextAdvancesReceiptAndRegisterWithoutRewritingAndLocalCopyInvalidatesEquality()=runBlocking<Unit> {
        home().use {scenario->
            val text="Identical received ${randomId()}"
            scenario.onActivity {clipboard.setPrimaryClip(ClipData.newPlainText("local","before"))}
            val first=receivedText(text);assertFalse(first.first.body["deduplicated"].asBoolean)
            val marker=withContext(Dispatchers.Main) {clipboard.primaryClipDescription!!.extras!!.toString()}
            val second=receivedText(text,b)
            assertEquals(Type.ACK,second.first.type);assertTrue(second.first.body["deduplicated"].asBoolean)
            assertEquals(marker,withContext(Dispatchers.Main) {clipboard.primaryClipDescription!!.extras!!.toString()})
            assertEquals("applied",app.queue.receipt(b,second.second["transfer_id"].asString).state)
            assertFalse(app.preferences.clipboardWins(ClipboardStamp(second.second["hlc"].asString.toBigInteger(),b)))
            assertEquals(b,app.preferences.read().recentClipboardSources[sha256(text.toByteArray())])
            scenario.onActivity {clipboard.setPrimaryClip(ClipData.newPlainText("local","intervening local copy"))}
            val third=receivedText(text);assertFalse(third.first.body["deduplicated"].asBoolean)
            assertEquals(text,withContext(Dispatchers.Main) {clipboard.primaryClip!!.getItemAt(0).text.toString()})
            scenario.onActivity {clipboard.setPrimaryClip(ClipData.newPlainText("other app",text))}
            val fourth=receivedText(text,b);assertTrue(fourth.first.body["deduplicated"].asBoolean)
            assertEquals("other app",withContext(Dispatchers.Main) {clipboard.primaryClipDescription!!.label.toString()})
            app.preferences.beforeMutation={if(it=="clipboardApplied")throw java.io.IOException("Injected deduplication completion failure")}
            val failed=receivedText(text)
            assertEquals(Type.NACK,failed.first.type);assertEquals("storage_failed",failed.first.body["code"].asString)
            app.preferences.beforeMutation={}
            val recovered=app.clipboard.receive(a,failed.second,DeliveryBinding.from(app.preferences.read(),a),mesh=true)
            assertEquals(Type.ACK,recovered.type);assertTrue(recovered.body["deduplicated"].asBoolean)
            assertEquals("applied",app.queue.receipt(a,failed.second["transfer_id"].asString).state)
            assertEquals("other app",withContext(Dispatchers.Main) {clipboard.primaryClipDescription!!.label.toString()})
            scenario.onActivity {clipboard.clearPrimaryClip()}
        }
    }
    @Test(timeout=60000) fun identicalImagesAtDifferentUrisRetainTheCurrentOfferAndAdvanceTheRegister()=runBlocking<Unit> {
        online()
        home().use {scenario->
            val bytes=android.util.Base64.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==",0)
            val effects=ReceivedFileEffects(app)
            scenario.onActivity {clipboard.setPrimaryClip(ClipData.newPlainText("local","before image"))}
            val (first,meta1)=file(bytes,"image/png")
            effects.applyClipboard(first,meta1.copy(kind="clipboard_image"),true,CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),a))))
            val (second,meta2)=file(bytes,"image/png",b)
            assertNotEquals(first.uri,second.uri)
            effects.applyClipboard(second,meta2.copy(kind="clipboard_image"),true,CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),b))))
            assertEquals(first.uri,withContext(Dispatchers.Main) {clipboard.primaryClip!!.getItemAt(0).uri.toString()})
            assertFalse(app.preferences.clipboardWins(ClipboardStamp(meta2.hlc!!.toBigInteger(),b)))
            assertEquals(b,app.preferences.read().recentClipboardSources[meta2.hash])
            val retained=withContext(Dispatchers.Main) {clipboard.primaryClip!!}
            assertTrue(ClipboardSendPolicy.available(c,retained,app.preferences.read(),app.connection.devices.value))
            // Home, tile and notification actions share this admission path.
            val sent=sendClipboard(retained)
            assertEquals(listOf(a),app.queue.deliveries(sent.completed.single()!!).map {it.peerId})
            val frozen=app.captureAdmission().await()
            val sourceOnly=sendClipboard(retained,frozen.copy(text=frozen.text.filter {it.pcId==b},images=frozen.images.filter {it.pcId==b}))
            assertEquals(c.getString(R.string.toast_no_recipient),sourceOnly.result.value!!.message)
            assertTrue(sourceOnly.completed.isEmpty())
            // A raw URI loses the marker, but its staged bytes still identify
            // B as latest clipboard source, without consulting A's file journal.
            val raw=sendClipboard(ClipData.newRawUri("restored image",Uri.parse(first.uri)))
            assertEquals(listOf(a),app.queue.deliveries(raw.completed.single()!!).map {it.peerId})
            val legacy=ClipData(retained).apply {description.extras=android.os.PersistableBundle(description.extras).apply {remove(ClipboardDelivery.OWN_HASH)}}
            val oldMarker=sendClipboard(legacy)
            assertEquals(listOf(a),app.queue.deliveries(oldMarker.completed.single()!!).map {it.peerId})
            // Sharing that received file intentionally uses its own journal.
            val shared=ManualEntries.file(app,Uri.parse(first.uri),"file",frozen.binding,targets=frozen.files)
            assertEquals(listOf(b),app.queue.deliveries(shared.mediaId).map {it.peerId})
            scenario.onActivity {clipboard.setPrimaryClip(ClipData.newPlainText("local","intervening"))}
            val newer=app.newTextOrder(apply=false).first
            effects.applyClipboard(second,meta2.copy(kind="clipboard_image",hlc=newer.hlc.toString()),true,CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),b))))
            assertEquals(second.uri,withContext(Dispatchers.Main) {clipboard.primaryClip!!.getItemAt(0).uri.toString()})
            scenario.onActivity {clipboard.clearPrimaryClip()}
        }
    }
    @Test(timeout=60000) fun identicalTextUsesLatestSourceAndPickerKeepsThatResolutionThroughAdmission()=runBlocking<Unit> {
        online()
        home().use {scenario->
            val text="same received text ${randomId()}"
            scenario.onActivity {clipboard.setPrimaryClip(ClipData.newPlainText("local","before duplicate"))}
            assertFalse(receivedText(text,a).first.body["deduplicated"].asBoolean)
            assertTrue(receivedText(text,b).first.body["deduplicated"].asBoolean)
            val retained=withContext(Dispatchers.Main) {clipboard.primaryClip!!}
            assertEquals(a,retained.description.extras!!.getString(ClipboardDelivery.OWN_SOURCE))
            val sent=sendClipboard(retained)
            assertEquals(listOf(a),app.queue.deliveries(sent.completed.single()!!).map {it.peerId})
            app.preferences.toggle("chooseDevicesEachTime",true)
            val frozen=app.captureAdmission().await()
            val picker=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
                it.targetPickerAvailable=true
                it.share(ShareRequest(CompletableDeferred(frozen.binding),emptyList(),listOf(text),CompletableDeferred(frozen)))
            }}
            try {
                assertEquals(listOf(a),withTimeout(5000) {picker.targetSelection.filterNotNull().first()}.map {it.deviceId})
                // This later receive must not exclude the one target offered
                // by the already-resolved admission while the picker was open.
                assertEquals(Type.ACK,receivedText(text,a).first.type)
                assertEquals(a,app.preferences.read().recentClipboardSources[sha256(text.toByteArray())])
                picker.selectTargets(setOf(a));withTimeout(10000) {picker.result.filterNotNull().first()}
                assertEquals(listOf(a),app.queue.deliveries(picker.completed.single()!!).map {it.peerId})
            }finally {picker.cancelTargetSelection()}
            scenario.onActivity {clipboard.clearPrimaryClip()}
        }
    }

    @Test(timeout=60000) fun sharedReceivedFileKeepsItsJournalSourceWhenIdenticalContentComesFromAnotherComputer()=runBlocking<Unit> {
        online()
        val bytes="identical received file ${randomId()}".toByteArray()
        val (saved,metadata)=file(bytes,"text/plain",a)
        file(bytes,"text/plain",b)
        val order=app.newTextOrder(apply=false)
        app.preferences.appliedClipboard(ClipboardStamp(order.first.hlc,b),metadata.hash,b,2,randomId())
        assertEquals(b,app.preferences.read().recentClipboardSources[metadata.hash])
        app.preferences.toggle("chooseDevicesEachTime",true)
        val uri=ReceivedFileActions.resolve(c,saved,true)!!.getParcelableExtra(Intent.EXTRA_STREAM,Uri::class.java)!!
        val frozen=app.captureAdmission().await()
        val picker=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.targetPickerAvailable=true
            it.share(ShareRequest(CompletableDeferred(frozen.binding),listOf(uri),emptyList(),CompletableDeferred(frozen)))
        }}
        try {
            assertEquals(listOf(b),withTimeout(5000) {picker.targetSelection.filterNotNull().first()}.map {it.deviceId})
            picker.selectTargets(setOf(b));withTimeout(10000) {picker.result.filterNotNull().first()}
            assertEquals(listOf(b),app.queue.deliveries(picker.completed.single()!!).map {it.peerId})
        }finally {picker.cancelTargetSelection()}
    }
    @Test(timeout=60000) fun queueCapacityRejectsUnrelatedAndPartiallySupersededTextAndRollsBackEveryRecipient()=runBlocking<Unit> {
        val p=app.preferences.read();val targets=p.devices.map {requireNotNull(DeliveryBinding.from(p,it.deviceId))}
        app.database.runInTransaction {
            repeat(2047) {app.queue.insert(Outgoing().apply {mediaId="file$it";transferId=randomId();kind="file";peerId=a;automatic=false})}
            app.queue.admit(Outgoing().apply {mediaId="old";transferId=randomId();kind="text";peerId=a;createdAt=1;automatic=false})
        }
        fun text(id:String,selected:List<DeliveryBinding>)=Outgoing().apply {mediaId=id;transferId=randomId();kind="text";text="new";createdAt=2;automatic=false;recipients=selected}
        suspend fun reject(row:Outgoing) {
            try {app.enqueueFixture(row);fail("Unrelated/remaining text cannot free capacity")}catch(_:LocalCapacityException) {}
            assertEquals(2048,app.queue.pendingCount());assertNull(app.queue.find(row.mediaId))
            assertEquals("waiting",app.queue.delivery("old",a).state)
        }
        reject(text("unrelated",listOf(targets[1])))
        app.queue.admit(Outgoing().apply {mediaId="old";transferId=app.queue.find("old").transferId;kind="text";peerId=b;createdAt=1;automatic=false})
        reject(text("partial",listOf(targets[0])))
        assertTrue(app.enqueueFixture(text("both",targets)))
        assertEquals(2048,app.queue.pendingCount());assertTrue(app.queue.deliveries("old").all {it.state=="superseded"})
        assertTrue(app.preferences.read().localCapacity)
    }
}
