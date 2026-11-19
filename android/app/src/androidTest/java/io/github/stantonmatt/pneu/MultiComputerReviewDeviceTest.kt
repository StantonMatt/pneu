package io.github.stantonmatt.pneu

import android.Manifest
import android.app.*
import android.content.*
import android.net.Uri
import android.os.PersistableBundle
import android.os.Bundle
import android.os.Looper
import android.provider.MediaStore
import androidx.datastore.preferences.core.edit
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

@RunWith(AndroidJUnit4::class)
class MultiComputerReviewDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private val a="a".repeat(32);private val b="b".repeat(32)
    private val local=mutableListOf<LocalPeerFixture>();private val sources=mutableListOf<Uri>()
    private val reviewProvider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
    private fun control(method:String)=c.contentResolver.call(reviewProvider,method,null,null)!!
    private fun raw(name:String,pin:Char)="pneu://pair?v=1&name=$name&fp=${pin.toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private suspend fun bind(id:String,name:String,pin:Char) {
        val uri=raw(name,pin);assertTrue(app.requestPairing(uri,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(uri),id))
        withTimeout(5000) {app.prefs.first {it.devices.any {peer->peer.deviceId==id}}}
    }
    private suspend fun online(id:String) {LocalPeerFixture(app).also {local+=it}.connect(id)}
    private fun home()=Intent(c,MainActivity::class.java).putExtra("home",true).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset();device.pressHome();app.initialized.await()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true)
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,false)
        app.preferences.toggle("queue",true)
        bind(a,"A",'a')
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.preferences.beforeReadStart={};control("release")
        app.staging.beforePayloadWrite={};device.pressHome();DeviceFixture.stopPneuService()
        DeviceFixture.stopShareAdmissionService()
        app.connection.stop();app.connection.awaitStopped();local.forEach {it.close()}
        sources.forEach {c.contentResolver.delete(it,null,null)}
        DeviceFixture.reset()
    }
    private suspend fun unavailableShare(request:ShareRequest,picker:Boolean=true) {
        val operation=ManualEntryViewModel(c.applicationContext as Application).apply {targetPickerAvailable=picker}
        val opened=AtomicBoolean()
        val observer=app.scope.launch(start=CoroutineStart.UNDISPATCHED) {operation.targetSelection.collect {if(it!=null)opened.set(true)}}
        try {
            i.runOnMainSync {operation.share(request)}
            val result=withTimeout(5000) {operation.result.filterNotNull().first()}
            assertEquals(c.getString(R.string.toast_no_recipient),result.message)
            assertFalse("No empty picker may be published",opened.get())
            assertNull(operation.targetSelection.value);assertTrue(operation.completed.isEmpty())
            assertTrue(app.queue.all().isEmpty())
        }finally {observer.cancelAndJoin()}
    }
    private suspend fun unavailableCapturedShare(files:Boolean=false,picker:Boolean=true) {
        val captured=app.captureAdmission().await()
        unavailableShare(ShareRequest(CompletableDeferred(captured.binding),
            if(files)listOf(reviewProvider.buildUpon().appendPath("slow-type").build()) else emptyList(),
            if(files)emptyList() else listOf("No recipient"),CompletableDeferred(captured)),picker)
    }
    @Test(timeout=30000) fun setupRefreshKeepsDisplayConnectedWhileTheSocketIsNotReady()=runBlocking<Unit> {
        val blocked=CountDownLatch(1);val release=CountDownLatch(1);val hold=AtomicBoolean(false)
        val fixture=LocalPeerFixture(app,beforeHello={if(hold.get()) {blocked.countDown();check(release.await(10,TimeUnit.SECONDS))}}).also {local+=it}
        fixture.connect(a)
        try {
            val peer=app.preferences.read().devices.single()
            // Exercise the exact socket-refresh entry point used by Link, Allow and Don't allow.
            hold.set(true);app.connection.refreshSetupPeer(peer.pin)
            assertTrue("Refreshed HELLO is held",blocked.await(5,TimeUnit.SECONDS))
            assertFalse("No live READY authority during refresh",app.connection.connected(a))
            assertEquals("Connected",app.connection.devices.value.getValue(a).status)
            assertNull(app.connection.freezeSession(requireNotNull(DeliveryBinding.from(app.preferences.read(),a))))
            hold.set(false);release.countDown()
            withTimeout(5000) {while(!app.connection.connected(a))delay(20)}
            assertTrue(fixture.hellos.get()>=2)
        }finally {hold.set(false);release.countDown()}
    }
    @Test(timeout=30000) fun remoteSetupRefreshRedialsWithoutBackoffAndKeepsOnlyDisplayAuthority()=runBlocking<Unit> {
        val blocked=CountDownLatch(1);val release=CountDownLatch(1);val hold=AtomicBoolean(false)
        val fixture=LocalPeerFixture(app,capabilities=setOf("files","mesh","setup"),setupHello=SetupHello(SetupScope.ASK,null),
            beforeHello={if(hold.get()) {blocked.countDown();check(release.await(10,TimeUnit.SECONDS))}}).also {local+=it}
        fixture.connect(a,holdFirstFrame=false)
        try {
            hold.set(true)
            fixture.sendControl(Frame.Control(Type.ERROR,json("code" to "closing")))
            assertTrue("Explicit peer refresh redials before the minimum 1.6 s backoff",blocked.await(1400,TimeUnit.MILLISECONDS))
            assertFalse(app.connection.connected(a))
            assertEquals("Connected",app.connection.devices.value.getValue(a).status)
            assertNull(app.connection.freezeSession(requireNotNull(DeliveryBinding.from(app.preferences.read(),a))))
            hold.set(false);release.countDown()
            withTimeout(5000) {while(!app.connection.connected(a))delay(20)}
        }finally {hold.set(false);release.countDown()}
    }
    @Test(timeout=30000) fun ordinarySetupPeerEofNeverStartsDisplayGrace()=runBlocking<Unit> {
        val blocked=CountDownLatch(1);val release=CountDownLatch(1);val hold=AtomicBoolean(false)
        val fixture=LocalPeerFixture(app,capabilities=setOf("files","mesh","setup"),setupHello=SetupHello(SetupScope.ASK,null),
            beforeHello={if(hold.get()) {blocked.countDown();check(release.await(10,TimeUnit.SECONDS))}}).also {local+=it}
        fixture.connect(a,holdFirstFrame=false)
        try {
            hold.set(true);fixture.disconnect()
            withTimeout(5000) {app.connection.devices.first {it[a]?.status=="PC unreachable"}}
            assertFalse(app.connection.connected(a))
            // Network/discovery wakes may legitimately shorten ordinary backoff.
            // Hold HELLO so neither the disconnect nor the dial can borrow grace.
            assertTrue(blocked.await(5000,TimeUnit.MILLISECONDS))
            assertEquals("Connecting…",app.connection.devices.value.getValue(a).status)
        }finally {hold.set(false);release.countDown()}
    }
    @Test(timeout=30000) fun offlineTextSharesFailImmediatelyWithAndWithoutPicker()=runBlocking<Unit> {
        bind(b,"B",'b');app.preferences.toggle("chooseDevicesEachTime",true)
        for(picker in listOf(true,false))unavailableCapturedShare(picker=picker)
    }
    @Test(timeout=30000) fun connectedTextIneligibleComputersNeverOpenAnEmptyPicker()=runBlocking<Unit> {
        LocalPeerFixture(app,setOf("files","clipboard_image")).also {local+=it}.connect(a)
        app.preferences.toggle("chooseDevicesEachTime",true)
        assertTrue(app.connection.connected(a))
        for(picker in listOf(true,false))unavailableCapturedShare(picker=picker)
    }
    @Test(timeout=30000) fun clipboardDisabledComputersNeverOpenAnEmptyTextPicker()=runBlocking<Unit> {
        online(a);app.setPairSwitch(a,"sendClipboard",false);app.preferences.toggle("chooseDevicesEachTime",true)
        assertTrue(app.connection.connected(a))
        for(picker in listOf(true,false))unavailableCapturedShare(picker=picker)
    }
    @Test(timeout=30000) fun emptyFileOffersCannotUseDefaultBindingOrReachProvider()=runBlocking<Unit> {
        app.preferences.toggle("chooseDevicesEachTime",true);control("reset")
        val binding=requireNotNull(DeliveryBinding.from(app.preferences.read()))
        val empty=FrozenAdmission(binding,emptyList(),emptyList(),emptyList())
        for(picker in listOf(true,false))unavailableShare(ShareRequest(CompletableDeferred(binding),
            listOf(reviewProvider.buildUpon().appendPath("slow-type").build()),emptyList(),CompletableDeferred(empty)),picker)
        assertEquals(0,control("status").getInt("types"));assertEquals(0,control("status").getInt("opens"))
        // Binding-only entries also obey computed eligibility when queueing is off.
        app.preferences.toggle("queue",false)
        unavailableShare(ShareRequest(CompletableDeferred(binding),listOf(reviewProvider.buildUpon().appendPath("slow-type").build()),emptyList()),picker=false)
    }
    @Test(timeout=30000) fun unsupportedFileComputerIsRejectedByShareAndDevicePage()=runBlocking<Unit> {
        LocalPeerFixture(app,setOf("clipboard_text")).also {local+=it}.connect(a)
        app.preferences.toggle("chooseDevicesEachTime",true);control("reset")
        for(picker in listOf(true,false))unavailableCapturedShare(files=true,picker=picker)
        val authority=CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),a)))
        ActivityScenario.launch<MainActivity>(home()).use {scenario->
            lateinit var operation:ManualEntryViewModel
            val expected=c.getString(R.string.toast_no_recipient)
            val event=i.uiAutomation.executeAndWaitForEvent({scenario.onActivity {activity->
                operation=sendFilesTo(activity,authority,listOf(reviewProvider.buildUpon().appendPath("slow-type").build()))
            }},{it.text.any {text->text.toString().contains(expected)}},10000)
            event.recycle()
            assertEquals(expected,withTimeout(5000) {operation.result.filterNotNull().first()}.message)
            assertTrue(operation.completed.isEmpty());assertTrue(app.queue.all().isEmpty())
        }
        assertEquals(0,control("status").getInt("types"));assertEquals(0,control("status").getInt("opens"))
    }
    @Test(timeout=30000) fun emptyClipboardTargetsRejectBeforeProviderEvenWithConnectedDefaultBinding()=runBlocking<Unit> {
        online(a);control("reset")
        val binding=requireNotNull(DeliveryBinding.from(app.preferences.read()))
        for(clip in listOf(ClipData.newPlainText("text","No recipient"),ClipData.newRawUri("image",reviewProvider.buildUpon().appendPath("slow-type").build()))) {
            val operation=ManualEntryViewModel(c.applicationContext as Application)
            operation.captureClipboardAdmission(CompletableDeferred(FrozenAdmission(binding,emptyList(),emptyList(),emptyList())))
            i.runOnMainSync {operation.clipboard(clip)}
            assertEquals(c.getString(R.string.toast_not_sent),withTimeout(5000) {operation.result.filterNotNull().first()}.message)
            assertTrue(operation.completed.isEmpty());assertTrue(app.queue.all().isEmpty())
        }
        assertEquals(0,control("status").getInt("types"));assertEquals(0,control("status").getInt("opens"))
    }
    @Test(timeout=30000) fun clipboardPolicyCanRemoveEveryFrozenRecipientWithoutReadingProvider()=runBlocking<Unit> {
        online(a);control("reset")
        val captured=app.captureAdmission().await()
        assertTrue(captured.text.isNotEmpty());assertTrue(captured.images.isNotEmpty())
        // A received clip cannot be explicitly echoed to its sole source. This
        // removes all otherwise connected, valid targets for both representations.
        for(image in listOf(false,true)) {
            val clip=if(image)ClipData.newRawUri("image",reviewProvider.buildUpon().appendPath("slow-type").build())
                else ClipData.newPlainText("text","Received clip")
            clip.description.extras=PersistableBundle().apply {
                putBoolean(ClipboardDelivery.OWN_CLIP,true);putString(ClipboardDelivery.OWN_SOURCE,a)
            }
            val operation=ManualEntryViewModel(c.applicationContext as Application)
            operation.captureClipboardAdmission(CompletableDeferred(captured))
            i.runOnMainSync {operation.clipboard(clip)}
            assertEquals(c.getString(R.string.toast_no_recipient),withTimeout(5000) {operation.result.filterNotNull().first()}.message)
            assertTrue(operation.completed.isEmpty())
        }
        app.setPairSwitch(a,"sendClipboard",false)
        val disabled=ManualEntryViewModel(c.applicationContext as Application)
        disabled.captureClipboardAdmission(CompletableDeferred(captured))
        i.runOnMainSync {disabled.clipboard(ClipData.newRawUri("image",reviewProvider.buildUpon().appendPath("slow-type").build()))}
        assertEquals(c.getString(R.string.toast_no_recipient),withTimeout(5000) {disabled.result.filterNotNull().first()}.message)
        assertTrue(disabled.completed.isEmpty());assertTrue(app.queue.all().isEmpty())
        assertEquals(0,control("status").getInt("types"));assertEquals(0,control("status").getInt("opens"))
    }
    @Test(timeout=30000) fun directAdmissionsNeverReplaceComputedEmptyTargetsWithDefaultBinding()=runBlocking<Unit> {
        online(a);control("reset")
        val binding=requireNotNull(DeliveryBinding.from(app.preferences.read()))
        for(kind in listOf("text","file","clipboard_image")) {
            val failure=try {
                if(kind=="text")ManualEntries.text(app,"No recipient","share",binding,emptyList())
                else ManualEntries.file(app,reviewProvider.buildUpon().appendPath("slow-type").build(),kind,binding,targets=emptyList())
                null
            }catch(e:ShareFailure) {e}
            assertEquals(kind,"no_recipient",failure?.reason)
        }
        assertTrue(app.queue.all().isEmpty())
        assertEquals(0,control("status").getInt("types"));assertEquals(0,control("status").getInt("opens"))
    }
    @Test(timeout=30000) fun defaultBindingCannotAdmitWhenComputedTextOrFileRecipientsAreEmpty()=runBlocking<Unit> {
        app.preferences.toggle("queue",false);control("reset")
        val binding=requireNotNull(DeliveryBinding.from(app.preferences.read()))
        for(kind in listOf("text","file","clipboard_image")) {
            val failure=try {
                if(kind=="text")ManualEntries.text(app,"No recipient","share",binding)
                else ManualEntries.file(app,reviewProvider.buildUpon().appendPath("slow-type").build(),kind)
                null
            }catch(e:ShareFailure) {e}
            assertEquals(kind,"no_recipient",failure?.reason)
        }
        assertTrue(app.queue.all().isEmpty())
        assertEquals(0,control("status").getInt("types"));assertEquals(0,control("status").getInt("opens"))
    }
    @Test(timeout=45000) fun clipboardImageKeepsItsOrderAcrossMimeProviderStalls()=runBlocking<Unit> {
        online(a);control("reset")
        val admission=app.captureAdmission()
        val clip=ClipData.newRawUri("image",reviewProvider.buildUpon().appendPath("slow-type").build())
        val operation=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.captureClipboardAdmission(admission);it.clipboard(clip)
        }}
        try {
            assertTrue(withContext(Dispatchers.IO) {control("entered").getBoolean("entered")})
            val newer=app.newTextOrder().first
            control("release")
            withTimeout(5000) {operation.result.filterNotNull().first()}
            val row=app.queue.find(operation.completed.single()!!)!!
            val stamp=ClipboardStamp(row.hlc.toBigInteger(),row.sourceDevice)
            assertTrue(stamp<newer);assertFalse(app.preferences.clipboardWins(stamp))
            assertEquals("clipboard_image",row.kind)
        }finally {control("release")}
    }
    @Test(timeout=45000) fun textAndImageEntriesReserveBeforeSettingsAndKeepIncomingClipboardNewer()=runBlocking<Unit> {
        online(a)
        for(image in listOf(false,true)) {
            val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>();val once=AtomicBoolean()
            app.preferences.beforeReadStart={if(currentCoroutineContext()[CoroutineName]?.name=="manual-entry" && once.compareAndSet(false,true)) {assertNotEquals(Looper.getMainLooper(),Looper.myLooper());entered.complete(Unit);release.await()}}
            val admission=app.captureAdmission()
            val clip=if(image)ClipData.newRawUri("image",reviewProvider.buildUpon().appendPath("image").build()) else ClipData.newPlainText("text","Older text")
            val operation=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
                it.captureClipboardAdmission(admission);it.clipboard(clip)
            }}
            try {
                withTimeout(5000) {entered.await()}
                val incoming=app.preferences.originateClipboard(apply=false)
                val received=ClipboardDelivery(app) {_,_->}
                assertEquals(Type.ACK,received.receive(a,json("transfer_id" to randomId(),"text" to "Newer incoming $image","created_at" to System.currentTimeMillis(),
                    "origin" to "clipboard","hlc" to incoming.hlc,"source_device" to a),DeliveryBinding.from(app.preferences.read()),mesh=true).type)
                release.complete(Unit)
                withTimeout(5000) {operation.result.filterNotNull().first()}
                val row=app.queue.find(operation.completed.single()!!)!!
                val stamp=ClipboardStamp(row.hlc.toBigInteger(),row.sourceDevice)
                assertTrue(stamp.hlc<incoming.hlc);assertFalse(app.preferences.clipboardWins(stamp))
            }finally {release.complete(Unit);app.preferences.beforeReadStart={}}
        }
    }
    @Test(timeout=45000) fun shareBatchReservesAllOrdersBeforeItsFirstSettingsRead()=runBlocking<Unit> {
        online(a)
        val p=app.preferences.read();val targets=app.recipients(p,"text");val binding=DeliveryBinding.from(p)
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>();val once=AtomicBoolean()
        app.preferences.beforeReadStart={if(currentCoroutineContext()[CoroutineName]?.name=="manual-entry" && once.compareAndSet(false,true)) {assertNotEquals(Looper.getMainLooper(),Looper.myLooper());entered.complete(Unit);release.await()}}
        val operation=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.share(ShareRequest(CompletableDeferred(binding),emptyList(),listOf("Older one","Older two"),CompletableDeferred(FrozenAdmission(binding,targets,targets,targets))))
        }}
        try {
            withTimeout(5000) {entered.await()};val newer=app.newTextOrder().first;release.complete(Unit)
            withTimeout(5000) {operation.result.filterNotNull().first()}
            assertEquals(2,operation.completed.size)
            for(id in operation.completed)assertTrue(app.queue.find(id!!)!!.hlc.toBigInteger()<newer.hlc)
        }finally {release.complete(Unit);app.preferences.beforeReadStart={}}
    }
    @Test(timeout=45000) fun pickerOptInIsSavedAndOwnersWithoutAPickerShareToEligibleComputers()=runBlocking<Unit> {
        bind(b,"B",'b');online(a)
        val devices=withContext(Dispatchers.Main) {DevicesViewModel(c.applicationContext as Application)}
        assertFalse(devices.chooseDevicesEachTime.first())
        devices.setChooseDevicesEachTime(true);withTimeout(5000) {devices.chooseDevicesEachTime.first {it}}
        app.preferences.toggle("queue",false)
        withTimeout(5000) {app.prefs.first {it.chooseDevicesEachTime && !it.queue}}
        val admission=app.captureAdmission();val frozen=admission.await();assertEquals(listOf(a),frozen.files.map {it.pcId})
        val operation=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {
            it.share(ShareRequest(CompletableDeferred(frozen.binding),listOf(reviewProvider.buildUpon().appendPath("image").build()),emptyList(),admission))
        }}
        withTimeout(5000) {operation.result.filterNotNull().first()}
        assertNull(operation.targetSelection.value)
        assertEquals(listOf(a),app.queue.deliveries(operation.completed.single()!!).map {it.peerId})
    }
    @Test(timeout=45000) fun directTextClipboardImageAndSharedImageReserveBeforeSettings()=runBlocking<Unit> {
        online(a)
        for(kind in listOf("text","clipboard_image","file")) {
            val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>();val once=AtomicBoolean()
            app.preferences.beforeReadStart={if(currentCoroutineContext()[CoroutineName]?.name=="direct-entry" && once.compareAndSet(false,true)) {entered.complete(Unit);release.await()}}
            val sending=async(Dispatchers.IO+CoroutineName("direct-entry")) {
                if(kind=="text")ManualEntries.text(app,"Direct older text","share")
                else ManualEntries.file(app,reviewProvider.buildUpon().appendPath("image").build(),kind)
            }
            try {
                withTimeout(5000) {entered.await()};val newer=app.newTextOrder().first;release.complete(Unit)
                val row=withTimeout(5000) {sending.await()}
                assertTrue("$kind originated after a newer operation",row.hlc.toBigInteger()<newer.hlc)
                assertFalse(app.preferences.clipboardWins(ClipboardStamp(row.hlc.toBigInteger(),row.sourceDevice)))
            }finally {release.complete(Unit);app.preferences.beforeReadStart={};sending.cancelAndJoin()}
        }
    }
    @Test(timeout=45000) fun olderReceivedClipboardRestoresKeepTheirSourceAfterPreferencesReload()=runBlocking<Unit> {
        bind(b,"B",'b')
        val delivery=ClipboardDelivery(app) {_,_->}
        for((peer,text) in listOf(a to "X",b to "Y"))assertEquals(Type.ACK,delivery.receive(peer,
            json("transfer_id" to randomId(),"text" to text,"created_at" to System.currentTimeMillis(),"origin" to "clipboard")).type)
        val reloaded=Preferences(c,app.preferences.store).read()
        val restored=ClipData.newPlainText("history","X")
        val targets=reloaded.devices.map {DeliveryBinding.from(reloaded,it.deviceId)!!}
        assertEquals(a,reloaded.recentClipboardSources[sha256("X".toByteArray())])
        assertEquals(listOf(b),ClipboardSendPolicy.targets(restored,reloaded,targets).map {it.pcId})
        assertTrue(ClipboardSendPolicy.targets(restored,reloaded,targets,automatic=true).isEmpty())
    }
    @Test(timeout=45000) fun missingPickerOrFinishedActivityCancelsAndReleasesGrantsAndNotification()=runBlocking<Unit> {
        app.preferences.toggle("chooseDevicesEachTime",true)
        val application=c.applicationContext as Application
        for(finishActivity in listOf(false,true)) {
            val created=CompletableDeferred<ShareActivity>()
            val callbacks=object:Application.ActivityLifecycleCallbacks {
                override fun onActivityCreated(activity:Activity,state:Bundle?) {}
                override fun onActivityPostCreated(activity:Activity,state:Bundle?) {
                    if(activity is ShareActivity) {
                        activity.operation!!.apply {targetPickerAvailable=true;targetSelectionTimeoutMillis=if(finishActivity)5000 else 1500}
                        created.complete(activity)
                    }
                }
                override fun onActivityStarted(a:Activity) {}
                override fun onActivityResumed(a:Activity) {}
                override fun onActivityPaused(a:Activity) {}
                override fun onActivityStopped(a:Activity) {}
                override fun onActivitySaveInstanceState(a:Activity,s:Bundle) {}
                override fun onActivityDestroyed(a:Activity) {}
            }
            val source=ChooserShareFixture.source("picker-${randomId()}")
            fun granted()=c.checkUriPermission(source,android.os.Process.myPid(),android.os.Process.myUid(),Intent.FLAG_GRANT_READ_URI_PERMISSION)==android.content.pm.PackageManager.PERMISSION_GRANTED
            assertFalse(granted())
            application.registerActivityLifecycleCallbacks(callbacks)
            try {
                c.startActivity(Intent().setComponent(ComponentName(c.packageName+".test",FixtureCallerActivity::class.java.name))
                    .putExtra("uri",source.toString()).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                val activity=withTimeout(5000) {created.await()};val operation=activity.operation!!
                withTimeout(5000) {operation.targetSelection.filterNotNull().first()}
                compose.waitUntil(5000) {compose.onAllNodesWithText("Send to").fetchSemanticsNodes().isNotEmpty()}
                assertTrue(granted())
                if(finishActivity)withContext(Dispatchers.Main) {activity.finish()}
                withTimeout(5000) {operation.result.filterNotNull().first()}
                assertNull(operation.targetSelection.value);assertTrue(operation.completed.isEmpty())
                // Once the activity finishes there may be no hierarchy left to query at all.
                compose.waitUntil(5000) {runCatching {compose.onAllNodesWithText("Send to").fetchSemanticsNodes().isEmpty()}.getOrDefault(true)}
                withTimeout(5000) {while(!activity.isDestroyed || granted() || c.getSystemService(NotificationManager::class.java).activeNotifications.any {it.id==4})delay(30)}
                assertTrue(app.queue.pending().isEmpty())
            }finally {application.unregisterActivityLifecycleCallbacks(callbacks)}
        }
    }
    @Test(timeout=45000) fun pickerKeepsOfflineChoiceWhenOnlyDefaultRecipientWasRevokedBeforeSettingsRead()=runBlocking<Unit> {
        bind(b,"B",'b');online(a);app.preferences.toggle("chooseDevicesEachTime",true);app.preferences.toggle("queue",false)
        withTimeout(5000) {app.prefs.first {it.chooseDevicesEachTime && !it.queue}}
        val captured=app.captureAdmission().await()
        assertEquals(listOf(a),captured.files.map {it.pcId});assertEquals(setOf(a,b),captured.fileChoices.map {it.pcId}.toSet())
        assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(app.preferences.read(),a)))
        val operation=ManualEntryViewModel(c.applicationContext as Application).apply {targetPickerAvailable=true}
        i.runOnMainSync {operation.share(ShareRequest(CompletableDeferred(captured.binding),listOf(Uri.parse("content://io.github.stantonmatt.pneu.test.review/image")),emptyList(),CompletableDeferred(captured)))}
        val offered=withTimeout(5000) {operation.targetSelection.filterNotNull().first()}
        assertEquals(listOf(b),offered.map {it.deviceId})
        i.runOnMainSync {operation.selectTargets(setOf(b))}
        withTimeout(10000) {operation.result.filterNotNull().first()}
        val rows=app.queue.deliveries(operation.completed.filterNotNull().single())
        assertEquals(listOf(b),rows.map {it.peerId});assertEquals("waiting",rows.single().state)
    }
    @Test(timeout=45000) fun pickerSheetSendsOnlyToTheChosenComputerAndWaitsForIt()=runBlocking<Unit> {
        bind(b,"B",'b');online(a);app.preferences.toggle("chooseDevicesEachTime",true);app.preferences.toggle("queue",false)
        val created=CompletableDeferred<ShareActivity>()
        val callbacks=object:Application.ActivityLifecycleCallbacks {
            override fun onActivityCreated(activity:Activity,state:Bundle?) {}
            override fun onActivityPostCreated(activity:Activity,state:Bundle?) {if(activity is ShareActivity)created.complete(activity)}
            override fun onActivityStarted(a:Activity) {}
            override fun onActivityResumed(a:Activity) {}
            override fun onActivityPaused(a:Activity) {}
            override fun onActivityStopped(a:Activity) {}
            override fun onActivitySaveInstanceState(a:Activity,s:Bundle) {}
            override fun onActivityDestroyed(a:Activity) {}
        }
        val application=c.applicationContext as Application
        val source=ChooserShareFixture.source("chosen-${randomId()}")
        application.registerActivityLifecycleCallbacks(callbacks)
        try {
            c.startActivity(Intent().setComponent(ComponentName(c.packageName+".test",FixtureCallerActivity::class.java.name))
                .putExtra("uri",source.toString()).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            val operation=withTimeout(5000) {created.await()}.operation!!
            compose.waitUntil(5000) {compose.onAllNodesWithText("Send to").fetchSemanticsNodes().isNotEmpty()}
            // A is connected and starts chosen; B is offline and can be chosen to wait.
            compose.onNodeWithText("A").performClick();compose.onNodeWithText("Send").assertIsNotEnabled()
            compose.onNodeWithText("B").performClick();compose.onNodeWithText("Sends when connected").assertIsDisplayed()
            compose.onNodeWithText("Send").performClick()
            withTimeout(10000) {operation.result.filterNotNull().first()}
            assertEquals(listOf(b),app.queue.deliveries(operation.completed.single()!!).map {it.peerId})
        }finally {application.unregisterActivityLifecycleCallbacks(callbacks)}
    }
    @Test(timeout=45000) fun devicePageSwitchAppliesAtOnceAndWaitsForItsComputer()=runBlocking<Unit> {
        bind(b,"Laptop",'b');online(a)
        ActivityScenario.launch<MainActivity>(home()).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("Laptop").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Laptop").performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Receive clipboard").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Receive clipboard").performClick()
            withTimeout(5000) {app.prefs.first {it.devices.first {peer->peer.deviceId==b}.clipboardReceive==false}}
            assertTrue(app.preferences.read().devices.first {peer->peer.deviceId==a}.clipboardReceive)
            // Offline: the change applies here at once and waits for Laptop to sync.
            compose.waitUntil(5000) {compose.onAllNodesWithText("Applies when Laptop connects").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithContentDescription("Back").performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Devices").fetchSemanticsNodes().isNotEmpty()}
            // A connected computer shows no pending line for its own change.
            compose.onNodeWithText("A").performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Send clipboard").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Send clipboard").performClick()
            withTimeout(5000) {app.prefs.first {it.devices.first {peer->peer.deviceId==a}.clipboardSend==false}}
            compose.onAllNodesWithText("Applies when A connects").assertCountEquals(0)
        }
    }
    @Test(timeout=45000) fun renameNeedsANameAndSavesWhatOthersSee()=runBlocking<Unit> {
        ActivityScenario.launch<MainActivity>(home()).use {
            compose.waitUntil(10000) {compose.onAllNodesWithContentDescription("Settings").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithContentDescription("Settings").performClick()
            compose.onNodeWithText("Name").performScrollTo().performClick()
            compose.onNodeWithText("Rename this phone").assertIsDisplayed()
            compose.onNode(hasSetTextAction()).performTextClearance()
            compose.onNodeWithText("Save").assertIsNotEnabled()
            compose.onNode(hasSetTextAction()).performTextInput("  Field Phone ")
            compose.onNodeWithText("Save").performClick()
            withTimeout(5000) {app.prefs.first {it.selfName=="Field Phone"}}
            compose.waitUntil(5000) {compose.onAllNodesWithText("Rename this phone").fetchSemanticsNodes().isEmpty()}
            compose.onNodeWithText("Field Phone").assertExists()
        }
    }
    @Test(timeout=45000) fun addingAnotherComputerSaysPairAndScannerCompletesForItsBinding()=runBlocking<Unit> {
        val baseline=app.preferences.read().pairingRevision
        ActivityScenario.launch<MainActivity>(Intent(Intent.ACTION_VIEW,Uri.parse(raw("B",'b')),c,MainActivity::class.java)).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("Pair with B?").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Pair").performClick()
            withTimeout(5000) {DeviceFixture.awaitPrefs(app) {app.connection.attempt.value?.invitation?.name=="B"}}
            val pending=requireNotNull(app.connection.attempt.value?.invitation)
            assertTrue(app.installBinding(pending,b))
            compose.waitUntil(10000) {compose.onAllNodesWithText("Recent").fetchSemanticsNodes().isNotEmpty()}
            val p=app.preferences.read();assertEquals(setOf(a,b),p.devices.map {peer->peer.deviceId}.toSet())
            assertTrue(p.devices.all {peer->peer.clipboardSend})
            assertEquals(b,p.installedAfter(baseline)?.pcId)
        }
    }
    @Test(timeout=60000) fun healthyCertificateCollisionEndsWithoutRequestAndKeepsDeliveries()=runBlocking<Unit> {
        val fixture=LocalPeerFixture(app).also {local+=it}
        val uri=fixture.invitation(a,"New identity")
        val before=app.preferences.read();val binding=DeliveryBinding.from(before,a)!!
        val row=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();kind="file";automatic=false;fileName="Keep.bin";recipients=listOf(binding)}
        assertTrue(app.enqueueFixture(row))
        ActivityScenario.launch<MainActivity>(Intent(Intent.ACTION_VIEW,Uri.parse(uri),c,MainActivity::class.java)).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("Pair with New identity?").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Pair").performClick()
            compose.waitUntil(10000) {compose.onAllNodesWithText("Already paired as A. Unpair it first.").fetchSemanticsNodes().isNotEmpty()}
            assertEquals(0,fixture.pairRequests.get())
            assertTrue(binding.matches(app.preferences.read()))
            assertEquals("waiting",app.queue.delivery(row.mediaId,a).state)
            withTimeout(5000) {DeviceFixture.awaitPrefs(app) {app.connection.attempt.value?.invitation==null}}
            assertEquals(0,fixture.pairRequests.get());assertTrue(binding.matches(app.preferences.read()))
            assertEquals("waiting",app.queue.delivery(row.mediaId,a).state)
        }
    }
    @Test(timeout=60000) fun lostCertificateRepairAtLimitPreservesOtherComputers()=runBlocking<Unit> {
        app.preferences.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p)+(1 until 16).map {n->
            Peer("Computer $n",n.toString(16).padStart(64,'0'),n.toString(16).padStart(32,'0'),emptyList(),generation=n+1L)
        })}
        app.preferences.setLong("pairingRevision",16)
        app.preferences.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).map {if(it.deviceId==a)it.copy(pairingLost=true) else it})}
        val before=app.preferences.read();val old=before.devices.first {it.deviceId==a}
        val fixture=LocalPeerFixture(app).also {local+=it};val uri=fixture.invitation(a,"Repaired")
        ActivityScenario.launch<MainActivity>(Intent(Intent.ACTION_VIEW,Uri.parse(uri),c,MainActivity::class.java)).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("Pair with Repaired?").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Pair").performClick()
            withTimeout(10000) {DeviceFixture.awaitPrefs(app) {app.connection.attempt.value?.invitation==null && it.devices.any {peer->peer.deviceId==a && peer.pin==Invitation.parse(uri).fingerprint}}}
            val after=app.preferences.read();assertEquals(16,after.devices.size);assertNotEquals(old.generation,after.devices.first {it.deviceId==a}.generation)
            assertEquals(before.devices.filter {it.deviceId!=a},after.devices.filter {it.deviceId!=a})
            assertEquals(1,fixture.pairRequests.get())
            assertEquals(1,fixture.hellos.get()) // One authenticated handshake, one token use.
        }
    }
    @Test(timeout=60000) fun newerSettingsSyncBeforeInFlightNackReleasesTheRejectedAttempt()=runBlocking<Unit> {
        val firstEnd=CompletableDeferred<Pair<String,(Frame.Control)->Unit>>()
        val attempts=java.util.concurrent.atomic.AtomicInteger()
        val fixture=LocalPeerFixture(app,setOf("files","clipboard_text","clipboard_image","mesh")) {frame,send->
            when(frame.type) {
                Type.FILE_END->{
                    val id=frame.body["transfer_id"].asString
                    if(attempts.incrementAndGet()==1)firstEnd.complete(id to send)
                    else send(Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to false)))
                    true
                }
                else->false
            }
        }.also {local+=it}
        app.preferences.toggle("originals",true)
        fixture.connect(a,holdFirstFrame=false)
        withTimeout(10000) {app.prefs.first {it.devices.first {peer->peer.deviceId==a}.let {peer->!PairSettings.syncing(peer)}}}
        val p=app.preferences.read();val binding=DeliveryBinding.from(p,a)!!
        val bytes="in-flight settings regression".toByteArray()
        val row=Outgoing().apply {
            mediaId="manual:${randomId()}";transferId=randomId();kind="clipboard_image";automatic=false;fileName="clipboard.png";mime="image/png"
            path=File(c.filesDir,"outgoing/$transferId.bin").apply {parentFile!!.mkdirs();writeBytes(bytes)}.path
            size=bytes.size.toLong();hash=sha256(bytes);createdAt=System.currentTimeMillis();recipients=listOf(binding)
        }
        assertTrue(app.enqueueFixture(row))
        val (id,send)=withTimeout(10000) {firstEnd.await()}
        app.setPairSwitch(a,"sendClipboard",false);app.setPairSwitch(a,"sendClipboard",true)
        withTimeout(10000) {app.prefs.first {it.devices.first {peer->peer.deviceId==a}.let {peer->peer.clipboardSend && !PairSettings.syncing(peer) && peer.confirmedSettings!=p.devices.first {old->old.deviceId==a}.confirmedSettings}}}
        send(Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "unsupported_kind","retryable" to false)))
        withTimeout(10000) {while(app.queue.delivery(row.mediaId,a)?.state!="sent")delay(20)}
        assertEquals(2,attempts.get());assertEquals(row.transferId,app.queue.delivery(row.mediaId,a).transferId)
    }

    @Test(timeout=45000) fun secondaryConnectionEnablesHomeAndNotificationClipboardActions()=runBlocking<Unit> {
        bind(b,"B",'b');online(b)
        ActivityScenario.launch<MainActivity>(home()).use {scenario->
            // Availability also requires sendable content; never inherit an
            // empty, sensitive or deleted-URI clipboard from another test.
            scenario.onActivity {c.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("local","Secondary connection clipboard"))}
            compose.waitUntil(10000) {compose.onAllNodesWithText("1 of 2 devices").fetchSemanticsNodes().isNotEmpty()}
            compose.waitUntil(5000) {!compose.onNodeWithText("Send clipboard").fetchSemanticsNode().config.contains(androidx.compose.ui.semantics.SemanticsProperties.Disabled)}
            compose.onNodeWithText("Send clipboard").assertIsEnabled()
            withTimeout(5000) {while(c.getSystemService(NotificationManager::class.java).activeNotifications.none {it.id==1 && it.notification.actions?.any {action->action.title.toString()=="Send clipboard"}==true})delay(30)}
            app.setPairSwitch(b,"sendClipboard",false)
            compose.waitUntil(5000) {compose.onNodeWithText("Send clipboard").fetchSemanticsNode().config.contains(androidx.compose.ui.semantics.SemanticsProperties.Disabled)}
            compose.onNodeWithText("Send clipboard").assertIsNotEnabled()
            withTimeout(5000) {while(c.getSystemService(NotificationManager::class.java).activeNotifications.any {it.id==1 && it.notification.actions?.any {action->action.title.toString()=="Send clipboard"}==true})delay(30)}
        }
    }
    @Test(timeout=45000) fun receivedClipDoesNotAutoForwardAndExplicitSendOriginatesOnlyToOtherComputers()=runBlocking<Unit> {
        bind(b,"B",'b');online(a);online(b)
        ActivityScenario.launch<MainActivity>(home()).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("Recent").fetchSemanticsNodes().isNotEmpty()}
            val text="Received link ${randomId()}";val incoming=randomId()
            val response=app.clipboard.receive(a,json("transfer_id" to incoming,"text" to text,"created_at" to System.currentTimeMillis(),"origin" to "clipboard"))
            assertEquals(Type.ACK,response.type);delay(400);assertTrue(app.queue.pending().isEmpty())
            val p=app.preferences.read();assertEquals(a,p.lastReceivedSource)
            val targets=app.recipients(p,"text");assertEquals(2,targets.size)
            val clip=withContext(Dispatchers.Main) {c.getSystemService(ClipboardManager::class.java).primaryClip!!}
            val operation=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {vm->
                vm.captureClipboardAdmission(CompletableDeferred(FrozenAdmission(DeliveryBinding.from(p),targets,emptyList(),emptyList())))
                vm.clipboard(clip)
            }}
            withTimeout(5000) {operation.result.filterNotNull().first()}
            val rows=app.queue.pending();assertEquals(1,rows.size);assertEquals(b,rows.single().peerId)
            assertNotEquals(incoming,rows.single().transferId);assertEquals(p.deviceId,rows.single().sourceDevice);assertTrue(rows.single().hlc.isNotEmpty())
            val before=app.queue.nextOrder()
            val sensitive=ClipData.newPlainText("private","secret").apply {description.extras=PersistableBundle().apply {putBoolean(ClipDescription.EXTRA_IS_SENSITIVE,true)}}
            val blocked=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {vm->
                vm.captureClipboardAdmission(CompletableDeferred(FrozenAdmission(DeliveryBinding.from(p),targets,emptyList(),emptyList())))
                vm.clipboard(sensitive)
            }}
            assertEquals(c.getString(R.string.toast_sensitive),withTimeout(5000) {blocked.result.filterNotNull().first()}.message)
            assertEquals(before,app.queue.nextOrder())
        }
    }
    @Test(timeout=45000) fun recentRetryAndCancelApplyToAllCapturedRecipients()=runBlocking<Unit> {
        bind(b,"B",'b');val p=app.preferences.read()
        val row=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();kind="file";automatic=false;fileName="Both-computers.bin";state="failed";error="busy";nextAttempt=Long.MAX_VALUE
            path=File(c.filesDir,"outgoing/$transferId.bin").apply {parentFile!!.mkdirs();writeText("shared payload")}.path
            recipients=p.devices.map {DeliveryBinding.from(p,it.deviceId)!!}}
        assertTrue(app.enqueueFixture(row))
        withTimeout(5000) {app.work.first {it.size==2}}
        ActivityScenario.launch<MainActivity>(home()).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText(row.fileName).fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithContentDescription("Retry").performClick()
            withTimeout(5000) {while(app.queue.deliveries(row.mediaId).any {it.state!="waiting" || it.revision!=1L})delay(30)}
            compose.waitUntil(5000) {compose.onAllNodesWithContentDescription("Don't send").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithContentDescription("Don't send").performClick()
            withTimeout(5000) {while(app.queue.deliveries(row.mediaId).any {it.state!="discarded"})delay(30)}
            assertEquals(2,app.queue.deliveries(row.mediaId).size);assertFalse(File(row.path).exists())
        }
    }
    @Test(timeout=45000) fun removingFirstRecipientDuringMultiFileShareKeepsLaterFilesForSurvivors()=runBlocking<Unit> {
        bind(b,"B",'b');val p=app.preferences.read();val targets=p.devices.map {DeliveryBinding.from(p,it.deviceId)!!}
        repeat(3) {n->sources+=c.contentResolver.insert(MediaStore.Downloads.getContentUri("external_primary"),ContentValues().apply {put("_display_name","Pneu_review_${randomId()}.bin");put("mime_type","application/octet-stream")})!!.also {uri->c.contentResolver.openOutputStream(uri)!!.use {it.write("file $n".toByteArray())}}}
        val entered=CountDownLatch(1);val release=CountDownLatch(1);val once=AtomicBoolean()
        app.staging.beforePayloadWrite={if(once.compareAndSet(false,true)) {entered.countDown();check(release.await(10,TimeUnit.SECONDS))}}
        try {
            val operation=withContext(Dispatchers.Main) {ManualEntryViewModel(c.applicationContext as Application).also {vm->vm.share(ShareRequest(CompletableDeferred(targets[0]),sources.toList(),emptyList(),CompletableDeferred(FrozenAdmission(targets[0],targets,targets,targets))))}}
            assertTrue(withContext(Dispatchers.IO) {entered.await(5,TimeUnit.SECONDS)})
            assertTrue(app.forgetBinding(confirmed=ConfirmedBinding.from(p,a)))
            bind("c".repeat(32),"C",'c')
            release.countDown()
            withTimeout(10000) {operation.result.filterNotNull().first()}
            assertEquals(3,operation.completed.filterNotNull().size)
            val rows=app.queue.pending();assertEquals(3,rows.size);assertTrue(rows.all {it.peerId==b})
        }finally {release.countDown();app.staging.beforePayloadWrite={}}
    }
    @Test(timeout=45000) fun repairingSecondaryComputerCompletesScannerAndPreservesFirstBinding()=runBlocking<Unit> {
        bind(b,"B",'b');val before=app.preferences.read();val generation=before.devices.first {it.deviceId==a}.generation
        ActivityScenario.launch<MainActivity>(Intent(Intent.ACTION_VIEW,Uri.parse(raw("B",'b')),c,MainActivity::class.java)).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("Pair with B?").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Pair").performClick()
            withTimeout(5000) {DeviceFixture.awaitPrefs(app) {app.connection.attempt.value?.invitation?.name=="B"}}
            assertTrue(app.installBinding(requireNotNull(app.connection.attempt.value?.invitation),b))
            compose.waitUntil(10000) {compose.onAllNodesWithText("Recent").fetchSemanticsNodes().isNotEmpty()}
            val p=app.preferences.read();assertEquals(generation,p.devices.first {it.deviceId==a}.generation)
            assertEquals(before.devices.first {it.deviceId==b}.generation,p.devices.first {it.deviceId==b}.generation)
            assertNull(p.installedAfter(before.pairingRevision))
        }
    }
    @Test(timeout=45000) fun devicePagesExposeNamedPairControlsAndHomeAddsDevices()=runBlocking<Unit> {
        bind(b,"B",'b');online(a)
        ActivityScenario.launch<MainActivity>(home()).use {
            compose.waitUntil(10000) {compose.onAllNodesWithText("B").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("Connected · ",substring=true).assertIsDisplayed()
            compose.onNodeWithText("B").performClick()
            compose.onNodeWithText("Receive clipboard").performScrollTo().performClick()
            withTimeout(5000) {app.prefs.first {it.devices.first {peer->peer.deviceId==b}.clipboardReceive==false}}
            assertTrue(app.preferences.read().devices.first {peer->peer.deviceId==a}.clipboardReceive)
            compose.onNodeWithText("Unpair").performScrollTo().performClick()
            compose.onNodeWithText("Unpair from B?").assertIsDisplayed()
            compose.onAllNodesWithText("Unpair").onLast().performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Recent").fetchSemanticsNodes().isNotEmpty()}
            assertEquals(listOf(a),app.preferences.read().devices.map {peer->peer.deviceId})
            assertTrue(app.connection.connected(a))
            compose.onNodeWithText("Add device").performScrollTo().performClick()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Scan the QR code in Pneu on your computer").fetchSemanticsNodes().isNotEmpty()}
        }
    }

    @Test(timeout=45000) fun secondaryUnpairCleanupRetryNeverTargetsTheRemainingComputer()=runBlocking<Unit> {
        bind(b,"B",'b');val before=app.preferences.read()
        val row=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();kind="file";automatic=false;recipients=before.devices.map {DeliveryBinding.from(before,it.deviceId)!!}}
        assertTrue(app.enqueueFixture(row))
        val db=app.database.openHelper.writableDatabase
        db.execSQL("CREATE TRIGGER fail_secondary_cleanup BEFORE DELETE ON deliveries WHEN OLD.peerId='$b' BEGIN SELECT RAISE(ABORT,'fixture cleanup failure'); END")
        try {
            assertTrue(runCatching {app.connection.unpair(ConfirmedBinding.from(before,b))}.isFailure)
            val removed=app.preferences.read();assertEquals(listOf(a),removed.devices.map {it.deviceId})
            val retry=ConfirmedBinding.from(removed,b);assertEquals(b,retry.pcId);assertNull(retry.pin)
            db.execSQL("DROP TRIGGER fail_secondary_cleanup")
            assertTrue(withContext(Dispatchers.Main) {app.connection.unpair(retry)})
            assertEquals(before.devices.first {it.deviceId==a},app.preferences.read().devices.single())
            assertNull(app.queue.delivery(row.mediaId,b));assertNotNull(app.queue.delivery(row.mediaId,a))
        }finally {db.execSQL("DROP TRIGGER IF EXISTS fail_secondary_cleanup")}
    }

}
