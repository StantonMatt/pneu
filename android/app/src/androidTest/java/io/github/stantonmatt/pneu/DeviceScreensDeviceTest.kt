package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Activity
import android.app.Instrumentation
import android.content.*
import android.net.Uri
import io.github.stantonmatt.pneu.connection.IncomingTransfer
import io.github.stantonmatt.pneu.protocol.*
import android.graphics.Bitmap
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.Endpoint
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File
import java.util.Calendar

/** The device screens with controlled fixtures, in light and dark, plus their interactions
 * against real settings. Real transfers and pairing remain covered by the peer suites. */
@RunWith(AndroidJUnit4::class)
class DeviceScreensDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val instrumentation=InstrumentationRegistry.getInstrumentation()
    private val context=instrumentation.targetContext
    private val app=context.graph
    private val now=System.currentTimeMillis()
    private val today=Calendar.getInstance().apply {set(Calendar.HOUR_OF_DAY,10);set(Calendar.MINUTE,12)}.timeInMillis.coerceAtMost(now)
    private fun peer(name:String,id:Char,seen:Long=today,lost:Boolean=false)=Peer(name,id.toString().repeat(64),id.toString().repeat(32),
        listOf(Endpoint("192.168.1.${31+id.code%20}",47821),Endpoint("100.67.144.${28+id.code%20}",47821)),lastConnected=seen,pairingLost=lost)
    private val pc=peer("MJStanton-Desktop",'d');private val mac=peer("MacBook Air",'e',today-25*60000)
    private val long=peer("Matthew’s MacBook Pro (Studio, 16-inch)",'f');private val htpc=peer("Living-Room-HTPC",'1')
    private val office=peer("Office-PC",'2',now-86400000);private val mini=peer("Studio Mac mini",'3',now-4*86400000)
    private val wifi=LinkState("Connected","Wi-Fi",capabilities=setOf("files","clipboard_text","clipboard_image","mesh"));private val tailscale=wifi.copy(route="Tailscale")
    private var dark by mutableStateOf(false)
    private fun clipboard(text:String)=ClipData.newPlainText("fixture",text).apply {
        description.extras=android.os.PersistableBundle().apply {putBoolean("com.android.systemui.SUPPRESS_CLIPBOARD_OVERLAY",true)}
    }

    @Before fun reset()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,
            Manifest.permission.POST_NOTIFICATIONS,Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO))
            instrumentation.uiAutomation.grantRuntimePermission(context.packageName,permission)
    }
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.stopPneuService();app.connection.stop();app.connection.awaitStopped();DeviceFixture.reset()}

    private fun capture(name:String) {
        compose.waitForIdle()
        Thread.sleep(400) // Let dialog entry, system bars and switch animations settle.
        val bitmap=requireNotNull(instrumentation.uiAutomation.takeScreenshot()) {"Screenshot unavailable: $name"}
        try {
            val target=File(context.filesDir,"device-screens/$name.png").apply {parentFile!!.mkdirs()}
            target.outputStream().use {assertTrue("PNG save failed: $name",bitmap.compress(Bitmap.CompressFormat.PNG,100,it))}
        }finally {bitmap.recycle()}
    }
    /** Light first, then dark, with matching system bar icons. */
    private fun bothThemes(name:String,between:()->Unit={}) {
        for(theme in listOf(false,true)) {
            dark=theme
            val transparent=android.graphics.Color.TRANSPARENT
            compose.activityRule.scenario.onActivity {
                val style=if(theme)SystemBarStyle.dark(transparent) else SystemBarStyle.light(transparent,transparent)
                it.enableEdgeToEdge(style,style)
            }
            between()
            capture("$name-${if(theme)"dark" else "light"}")
        }
    }
    private fun show(content:@Composable ()->Unit) {
        compose.setContent {
            CompositionLocalProvider(LocalUiActions provides UiActions {message,_->fail("Unexpected fixture action: $message")}) {
                PneuTheme(dark=dark) {Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {content()}}
            }
        }
    }
    private suspend fun prefs(vararg peers:Peer,paused:Boolean=false,screenshots:Boolean=true)=
        app.preferences.read().copy(setup=true,peer=peers.first(),peers=peers.toList(),paused=paused,screenshots=screenshots,photos=false,recordings=false)
    private fun row(id:String,peer:Peer,state:String,kind:String="screenshot",minutes:Int=0,size:Long=2400000,error:String="",block:Outgoing.()->Unit={})=Outgoing().apply {
        mediaId=id;transferId="$id-${peer.deviceId.first()}";peerId=peer.deviceId;this.state=state;this.kind=kind;this.error=error
        this.size=size;createdAt=now-minutes*60000L;automatic=kind in listOf("screenshot","photo","recording");block()
    }
    private val history=listOf(
        row("shot",pc,"sent",minutes=8),row("shot",mac,"sent",minutes=8),
        row("link",pc,"sent","text",minutes=16) {text="https://github.com/stantonmatt/pneu";mime="text/plain";origin="share"},
        row("note",pc,"sent","text",minutes=40) {text="Meet at the north entrance at 6";mime="text/plain";origin="clipboard"},
        row("note",mac,"sent","text",minutes=40) {text="Meet at the north entrance at 6";mime="text/plain";origin="clipboard"},
        row("report",mac,"sent","file",minutes=55) {fileName="Quarterly report final v3.pdf";mime="application/pdf"})

    @Composable private fun Home(p:Prefs,links:Map<String,LinkState>,rows:List<Outgoing>,link:LinkState=wifi) {
        val items=RecentProjection.items(rows,p,links)
        var scrolled by remember {mutableStateOf(false)}
        Scaffold(containerColor=LocalPneuColors.current.bg,contentWindowInsets=WindowInsets.statusBars,topBar={HomeTopBar(scrolled){}}) {padding->
            HomeScreen(p,link,items.map {it.row},Health(true,true,true,true),Modifier.padding(padding),{},{},{scrolled=it},
                items.associateBy {it.row.mediaId},DeviceProjection.states(p,links),links)
        }
    }

    @Test(timeout=30000) fun homeWithTwoConnectedComputers()=runBlocking<Unit> {
        val p=prefs(pc,mac)
        compose.runOnUiThread {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clipboard("Ready to send"))}
        show {Home(p,mapOf(pc.deviceId to wifi,mac.deviceId to tailscale),history)}
        compose.onNodeWithText("Send clipboard").assertIsEnabled()
        compose.onNodeWithText("MJStanton-Desktop and MacBook Air").assertIsDisplayed()
        compose.onNodeWithText("Connected · Tailscale").assertIsDisplayed()
        compose.onNodeWithText("Clipboard · To 2 devices",substring=true).assertExists()
        bothThemes("home-two-connected")
    }
    @Test(timeout=45000) fun receivedFileRecentOpensAndReverseCapsuleUsesTypedState()=runBlocking<Unit> {
        // Real receive/storage + live RecentViewModel, with controlled connected rows.
        for(peer in listOf(pc,mac)) {
            val raw="pneu://pair?v=1&name=${Uri.encode(peer.name)}&fp=${peer.pin}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
            assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
            assertTrue(app.installBinding(Invitation.parse(raw),peer.deviceId))
        }
        val p=app.preferences.read().copy(setup=true)
        val data=android.util.Base64.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==",0)
        val metadata=ReceivedMetadata(randomId(),"Review image.png","image/png",data.size.toLong(),sha256(data),"file",now)
        val receiver=app.receivedFiles.connection(requireNotNull(DeliveryBinding.from(p,mac.deviceId)),false) {true}
        receiver.begin(com.google.gson.JsonParser.parseString(metadata.canonical).asJsonObject)
        receiver.chunk(Frame.Chunk(metadata.id,0,data))
        assertEquals(Type.ACK,receiver.end(metadata.id).type)
        val file=app.queue.receivedFile(mac.deviceId,metadata.id)
        val model=RecentViewModel(context.applicationContext as android.app.Application)
        var receiving by mutableStateOf(true)
        val incoming=IncomingTransfer(mac.deviceId,randomId(),"Review image.png",20,100)
        try {
            show {
                val items by model.items.collectAsState()
                val link=wifi.copy(incoming=if(receiving)incoming else null)
                var scrolled by remember {mutableStateOf(false)}
                Scaffold(containerColor=LocalPneuColors.current.bg,contentWindowInsets=WindowInsets.statusBars,topBar={HomeTopBar(scrolled){}}) {padding->
                    HomeScreen(p,link,items.map {it.row},Health(true,true,true,true),Modifier.padding(padding),{},{},{scrolled=it},items.associateBy {it.row.mediaId},
                        DeviceProjection.states(p,mapOf(pc.deviceId to wifi,mac.deviceId to link)),mapOf(pc.deviceId to wifi,mac.deviceId to link))
                }
            }
            compose.onNode(SemanticsMatcher.expectValue(ReceivingTransfer,true)).assertExists()
            compose.mainClock.autoAdvance=false
            bothThemes("home-receiving") {compose.mainClock.advanceTimeBy(600)}
            compose.mainClock.autoAdvance=true
            receiving=false
            compose.waitUntil(5000) {compose.onAllNodesWithText("From MacBook Air",substring=true).fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("From MacBook Air",substring=true).performScrollTo().assertIsDisplayed()
            bothThemes("recent-from")
            val monitor=instrumentation.addMonitor(IntentFilter(Intent.ACTION_CHOOSER),Instrumentation.ActivityResult(Activity.RESULT_OK,null),true)
            try {
                compose.onNodeWithText("Review image.png").performClick()
                compose.waitUntil(5000) {monitor.hits>0}
            }finally {instrumentation.removeMonitor(monitor)}
        } finally {context.contentResolver.delete(Uri.parse(file.uri),null,null)}
    }
    @Test(timeout=30000) fun updateRequiredAppearsOnHomeAndDevicePageWithoutUnpairing()=runBlocking<Unit> {
        val p=prefs(pc,mac);val update=LinkState("Update required")
        var page by mutableStateOf(false)
        show {if(page)Device(p,DeviceState(mac,update,emptySet(),false)) else Home(p,mapOf(pc.deviceId to wifi,mac.deviceId to update),emptyList())}
        compose.onNodeWithText("Update required").assertIsDisplayed()
        bothThemes("home-update-required")
        page=true
        compose.onNodeWithText("Update required").assertIsDisplayed()
        compose.onNodeWithText("Update Pneu on MacBook Air",useUnmergedTree=true).assertIsDisplayed()
        assertFalse(p.devices.any {it.pairingLost})
        bothThemes("device-update-required")
    }
    @Test(timeout=30000) fun clipboardButtonTracksContentAndEligibleComputer()=runBlocking<Unit> {
        val p=prefs(pc,mac).copy(tailscale=false)
        var online by mutableStateOf(true)
        show {Home(p,if(online)mapOf(pc.deviceId to wifi,mac.deviceId to tailscale) else emptyMap(),emptyList(),if(online)wifi else LinkState())}
        compose.runOnUiThread {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clipboard("Send this"))}
        compose.waitForIdle();compose.onNodeWithText("Send clipboard").assertIsEnabled()
        compose.runOnUiThread {context.getSystemService(ClipboardManager::class.java).clearPrimaryClip()}
        compose.waitForIdle();compose.onNodeWithText("Send clipboard").assertIsNotEnabled()
        compose.runOnUiThread {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clipboard("Send this"))}
        compose.waitForIdle();compose.onNodeWithText("Send clipboard").assertIsEnabled()
        online=false
        compose.waitForIdle();compose.onNodeWithText("Send clipboard").assertIsNotEnabled()
    }
    @Test(timeout=30000) fun uriMimeReadSurvivesProgressAndSettingsWhileEligibilityUpdatesIndependently()=runBlocking<Unit> {
        val entered=java.util.concurrent.CountDownLatch(1);val release=java.util.concurrent.CountDownLatch(1)
        val reads=java.util.concurrent.atomic.AtomicInteger()
        var p by mutableStateOf(prefs(pc))
        var link by mutableStateOf(wifi)
        var clip by mutableStateOf(ClipData.newRawUri("first",Uri.parse("content://fixture/image")))
        show {
            val available=clipboardAvailable(clip,p,mapOf(pc.deviceId to link)) {
                reads.incrementAndGet();entered.countDown();check(release.await(10,java.util.concurrent.TimeUnit.SECONDS));"image/png"
            }
            Text(if(available)"Ready" else "Unavailable")
        }
        try {
            assertTrue(entered.await(5,java.util.concurrent.TimeUnit.SECONDS))
            repeat(5) {n->
                compose.runOnUiThread {link=link.copy(sent=n.toLong(),total=100,transferMediaId="upload");p=p.copy(paused=!p.paused)}
                compose.waitForIdle()
            }
            assertEquals(1,reads.get());compose.onNodeWithText("Unavailable").assertExists()
            release.countDown()
            compose.waitUntil(5000) {compose.onAllNodesWithText("Ready").fetchSemanticsNodes().isNotEmpty()}
            compose.runOnUiThread {link=link.copy(sent=50)}
            compose.waitForIdle();compose.onNodeWithText("Ready").assertExists();assertEquals(1,reads.get())
            compose.runOnUiThread {link=LinkState()}
            compose.waitForIdle();compose.onNodeWithText("Unavailable").assertExists();assertEquals(1,reads.get())
            compose.runOnUiThread {link=wifi;clip=ClipData.newRawUri("first",Uri.parse("content://fixture/image"))}
            compose.waitForIdle();compose.onNodeWithText("Ready").assertExists();assertEquals(1,reads.get())
            compose.runOnUiThread {clip=ClipData.newRawUri("second",Uri.parse("content://fixture/image"))}
            compose.waitUntil(5000) {reads.get()==2}
            compose.waitForIdle();compose.onNodeWithText("Ready").assertExists()
        }finally {release.countDown()}
    }
    @Test(timeout=30000) fun homeSendingToTwoAndDetailsSheet()=runBlocking<Unit> {
        val p=prefs(pc,mac,office)
        val links=mapOf(pc.deviceId to wifi,mac.deviceId to wifi.copy(sent=900000,total=2400000,transferMediaId="rec"),office.deviceId to wifi)
        val rows=listOf(row("rec",pc,"sent","recording",size=46000000),row("rec",mac,"sending","recording",size=46000000),row("rec",office,"failed","recording",size=46000000,error="disk_full"))+history
        show {Home(p,links,rows,wifi.copy(sent=900000,total=2400000,transferMediaId="rec"))}
        compose.onNodeWithText("Failed on Office-PC · Disk is full").assertIsDisplayed()
        compose.onNodeWithText("Screen recording").performClick()
        compose.onNodeWithText("Sent · ",substring=true).assertIsDisplayed()
        compose.onNodeWithText("Failed · Disk is full").assertIsDisplayed()
        compose.onNodeWithContentDescription("Stop sending").assertIsDisplayed()
        bothThemes("home-recent-details")
    }
    @Test(timeout=30000) fun homeOneOfflineWaitingForIt()=runBlocking<Unit> {
        val p=prefs(pc,mac)
        // A shared file: the emulator's mobile data would hold a recording for Wi-Fi.
        val video:Outgoing.()->Unit={fileName="design-review-final.mp4";mime="video/mp4"}
        val rows=listOf(row("rec",pc,"sent","file",block=video),row("rec",mac,"waiting","file",block=video),row("lone",pc,"sent",minutes=3))+history.drop(2)
        show {Home(p,mapOf(pc.deviceId to wifi),rows)}
        compose.onNodeWithText("1 of 2 devices").assertIsDisplayed()
        compose.onNodeWithText("Waiting for MacBook Air").assertIsDisplayed()
        compose.onNodeWithText("Not connected · last seen",substring=true).assertIsDisplayed()
        bothThemes("home-one-offline")
    }
    @Test(timeout=30000) fun homeManyDevicesAndLongNames()=runBlocking<Unit> {
        val p=prefs(pc,mac,long,htpc,office,mini)
        show {Home(p,listOf(pc,mac,long,htpc).associate {it.deviceId to wifi},history.take(3))}
        compose.onNodeWithText("4 of 6 devices").assertIsDisplayed()
        compose.onNodeWithText("Not connected · last seen yesterday").assertIsDisplayed()
        bothThemes("home-many-devices")
    }
    @Test(timeout=30000) fun homeOnePairingLostAndConnecting()=runBlocking<Unit> {
        var lostScene by mutableStateOf(true)
        val lost=prefs(pc,mac.copy(pairingLost=true));val two=prefs(pc,mac)
        show {if(lostScene)Home(lost,mapOf(pc.deviceId to wifi),history) else Home(two,mapOf(pc.deviceId to LinkState("Connecting…"),mac.deviceId to LinkState("Connecting…")),emptyList(),LinkState("Connecting…"))}
        compose.onNodeWithText("Pairing lost").assertIsDisplayed()
        bothThemes("home-one-pairing-lost")
        compose.mainClock.autoAdvance=false;lostScene=false;compose.mainClock.advanceTimeBy(1000)
        compose.onNodeWithText("2 devices").assertIsDisplayed()
        bothThemes("home-connecting") {compose.mainClock.advanceTimeBy(400)}
    }
    @Test(timeout=30000) fun homeOneComputerNothingSentAndPaused()=runBlocking<Unit> {
        var paused by mutableStateOf(false)
        val one=prefs(pc);val two=prefs(pc,mac,paused=true)
        show {if(!paused)Home(one,mapOf(pc.deviceId to wifi),emptyList()) else Home(two,mapOf(pc.deviceId to wifi,mac.deviceId to tailscale),history,LinkState("Connected","Wi-Fi"))}
        compose.onNodeWithText("MJStanton-Desktop · Wi-Fi").assertIsDisplayed();compose.onNodeWithText("Add device").assertIsDisplayed()
        bothThemes("home-one-computer")
        paused=true;compose.waitForIdle()
        bothThemes("home-paused")
    }

    @Composable private fun Device(p:Prefs,state:DeviceState,unpairDialog:Boolean=false) {
        DeviceScreen(p,state,{},{},{},{}) {_,_->}
        if(unpairDialog)PneuDialog(destructive=true,onDismissRequest={},title={Text(context.getString(R.string.unpair_title,state.peer.name))},
            text={Text(context.resources.getQuantityString(R.plurals.unpair_text,2,2))},
            confirmButton={PneuTextButton({}){Text(context.getString(R.string.action_unpair))}},
            dismissButton={PneuTextButton({}){Text(context.getString(R.string.action_cancel))}})
    }
    @Test(timeout=30000) fun devicePageConnected()=runBlocking<Unit> {
        val p=prefs(pc,mac);show {Device(p,DeviceState(mac,wifi,emptySet(),true))}
        compose.onNodeWithText("Wi-Fi · ",substring=true).assertIsDisplayed()
        compose.onNodeWithText("Send files").assertIsDisplayed();compose.onNodeWithText("Screenshots").assertIsDisplayed()
        bothThemes("device-connected")
    }
    @Test(timeout=30000) fun devicePageOfflineWithPendingSwitchAndNoKinds()=runBlocking<Unit> {
        val p=prefs(pc,mac.copy(clipboardReceive=false),screenshots=false)
        show {Device(p,DeviceState(p.devices[1],LinkState(),setOf("receiveClipboard"),true))}
        compose.onNodeWithText("Applies when MacBook Air connects").assertIsDisplayed()
        compose.onNodeWithText("Off for all devices in Settings").assertIsDisplayed()
        compose.onNodeWithText("Last connected",substring=true).assertIsDisplayed()
        bothThemes("device-offline-pending")
    }
    @Test(timeout=30000) fun devicePagePairingLostAndNeedsUpdate()=runBlocking<Unit> {
        var lost by mutableStateOf(true)
        val p=prefs(pc,mac.copy(pairingLost=true));val working=prefs(pc,mac)
        show {if(lost)Device(p,DeviceState(p.devices[1],LinkState(),emptySet(),true)) else Device(working,DeviceState(mac,tailscale,emptySet(),false))}
        compose.onNodeWithText("Pair again").assertIsDisplayed();compose.onNodeWithText("Sharing").assertDoesNotExist()
        bothThemes("device-pairing-lost")
        lost=false;compose.waitForIdle()
        compose.onNodeWithText("Update Pneu on MacBook Air to sync these switches.").assertIsDisplayed()
        bothThemes("device-needs-update")
    }
    @Test(timeout=30000) fun devicePageLongNameAndUnpair()=runBlocking<Unit> {
        val p=prefs(pc,long);show {Device(p,DeviceState(long,wifi,emptySet(),true),unpairDialog=true)}
        compose.onNodeWithText("Unpair from Matthew’s MacBook Pro (Studio, 16-inch)?").assertIsDisplayed()
        bothThemes("device-long-name-unpair")
    }

    @Test(timeout=30000) fun pickerStartsWithConnectedAndSendsTheChoice()=runBlocking<Unit> {
        var sent:Set<String>?=null;var cancelled=false
        show {TargetPicker(listOf(office,pc,long,mac),mapOf(pc.deviceId to wifi,mac.deviceId to tailscale,long.deviceId to wifi),{sent=it}) {cancelled=true}}
        compose.onNodeWithText("Send to").assertIsDisplayed()
        compose.onNodeWithText("Send").assertIsEnabled()
        bothThemes("picker-connected-selected")
        for(name in listOf("MJStanton-Desktop","MacBook Air","Matthew’s MacBook Pro (Studio, 16-inch)"))compose.onNodeWithText(name).performClick()
        compose.onNodeWithText("Send").assertIsNotEnabled()
        bothThemes("picker-nothing-chosen")
        compose.onNodeWithText("Office-PC").performClick();compose.onNodeWithText("MJStanton-Desktop").performClick()
        compose.onNodeWithText("Sends when connected").assertIsDisplayed()
        bothThemes("picker-offline-chosen")
        compose.onNodeWithText("Send").performClick()
        assertEquals(setOf(office.deviceId,pc.deviceId),sent);assertFalse(cancelled)
    }

    @Test(timeout=30000) fun renameDialogAboveTheKeyboard()=runBlocking<Unit> {
        val p=prefs(pc);var saved:String?=null
        show {SettingsScreen(p.copy(selfName="Galaxy S25+"),Health(true,true,true,true),0,{},{});RenameDialog("Galaxy S25+",{}) {saved=it}}
        compose.onNodeWithText("Rename this phone").assertIsDisplayed()
        bothThemes("settings-rename")
        compose.onNodeWithText("Save").performClick();assertEquals("Galaxy S25+",saved)
    }
    @Test(timeout=30000) fun settingsTopAndBottom()=runBlocking<Unit> {
        val p=prefs(pc);show {SettingsScreen(p.copy(selfName="Galaxy S25+"),Health(true,true,true,true),0,{},{})}
        compose.onNodeWithText("Show in share sheet").assertIsDisplayed()
        compose.onNodeWithText("Receive clipboard from computer").assertDoesNotExist()
        bothThemes("settings-top")
        compose.onNodeWithText("Version",substring=true).performScrollTo()
        compose.onNodeWithText("Privacy policy").assertIsDisplayed()
        bothThemes("settings-bottom")
        val opened=CompletableDeferred<Intent>()
        val monitor=object:Instrumentation.ActivityMonitor() {
            override fun onStartActivity(intent:Intent):Instrumentation.ActivityResult? {
                if(intent.action!=Intent.ACTION_VIEW)return null
                opened.complete(Intent(intent));return Instrumentation.ActivityResult(Activity.RESULT_OK,null) // Don't open the browser.
            }
        }
        instrumentation.addMonitor(monitor)
        try {
            compose.onNodeWithText("Privacy policy").performClick()
            assertEquals("https://github.com/StantonMatt/pneu/blob/main/PRIVACY.md",withTimeout(5000) {opened.await()}.dataString)
        } finally {instrumentation.removeMonitor(monitor)}
    }
    @Test(timeout=30000) fun addDeviceAlreadyPaired()=runBlocking<Unit> {
        val p=prefs(pc)
        show {PairScreen(p,wifi,{},{},{},validationFeedback=UiFeedback("Already paired with MJStanton-Desktop"),cameraPreview={modifier,_->androidx.compose.foundation.layout.Box(modifier.then(Modifier.fillMaxSize()))})}
        compose.waitUntil(5000) {compose.onAllNodesWithText("Already paired with MJStanton-Desktop").fetchSemanticsNodes().isNotEmpty()}
        compose.onNodeWithText("Scan the QR code in Pneu on your computer").assertIsDisplayed()
        capture("add-device-already-paired")
    }
    @Test(timeout=30000) fun stoppedRecipientsShowTerminalOutcomesWithoutActionsWhileOtherRecipientsStayActionable()=runBlocking<Unit> {
        val p=prefs(pc,mac,long,htpc,office,mini)
        val rows=listOf(row("terminal",pc,"sent"),row("terminal",mac,"discarded"),row("terminal",long,"superseded"),row("terminal",htpc,"skipped"),row("terminal",office,"waiting"),row("terminal",mini,"failed",error="disk_full"))
        val item=RecentProjection.items(rows,p,emptyMap()).single()
        show {RecipientsSheet("Review image",item,emptyMap(),{}, {fail("Terminal stop")}, {fail("Unexpected retry")})}
        compose.onNodeWithText("Stopped").assertIsDisplayed()
        compose.onNodeWithText("Replaced by a newer item").assertIsDisplayed()
        compose.onNodeWithText("Skipped").assertIsDisplayed()
        compose.onAllNodesWithContentDescription("Don't send").assertCountEquals(1)
        compose.onAllNodesWithContentDescription("Retry").assertCountEquals(1)
        assertTrue(item.pending);assertTrue(item.actionable)
        val finished=RecentProjection.items(rows.take(4),p,emptyMap()).single()
        assertFalse(finished.pending);assertFalse(finished.actionable);assertTrue(finished.waiting.isEmpty())
    }
    @Test(timeout=30000) fun pendingPairSwitchSurvivesConnectedAndClearsOnlyItsMatchingAck()=runBlocking<Unit> {
        val raw="pneu://pair?v=1&name=Pending%20PC&fp=${pc.pin}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(raw),pc.deviceId))
        app.preferences.setPairSwitch(pc.deviceId,"receiveClipboard",false)
        var snapshot by mutableStateOf(app.preferences.read())
        var online by mutableStateOf(false)
        show {Device(snapshot,DeviceProjection.states(snapshot,if(online)mapOf(pc.deviceId to wifi) else emptyMap()).single())}
        compose.onNodeWithText("Applies when Pending PC connects").assertIsDisplayed()
        online=true;compose.waitForIdle()
        compose.onNodeWithText("Applies when Pending PC connects").assertIsDisplayed()
        val binding=DeliveryBinding.from(app.preferences.read(),pc.deviceId)!!
        val old=app.preferences.prepareSettings(binding)
        app.preferences.setPairSwitch(pc.deviceId,"receiveClipboard",true)
        app.preferences.acknowledgeSettings(binding,old)
        snapshot=app.preferences.read();compose.waitForIdle()
        compose.onNodeWithText("Applies when Pending PC connects").assertIsDisplayed()
        val latest=app.preferences.prepareSettings(binding);app.preferences.acknowledgeSettings(binding,latest)
        snapshot=app.preferences.read();compose.waitForIdle()
        compose.onNodeWithText("Applies when Pending PC connects").assertDoesNotExist()
    }

}
