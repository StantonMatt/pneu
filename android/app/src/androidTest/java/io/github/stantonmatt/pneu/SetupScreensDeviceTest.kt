package io.github.stantonmatt.pneu

import android.Manifest
import android.app.NotificationManager
import android.graphics.Bitmap
import androidx.datastore.preferences.core.edit
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.state.ToggleableState
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.By
import androidx.test.uiautomator.UiDevice
import androidx.test.uiautomator.Until
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.Endpoint
import io.github.stantonmatt.pneu.protocol.SetupMember
import io.github.stantonmatt.pneu.protocol.SetupScope
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.runBlocking
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

/** Part C ("pair once, join your devices") on the phone, every state in light and dark,
 * driven by controlled setup state. Real actions are covered by SetupFlowDeviceTest. */
@RunWith(AndroidJUnit4::class)
class SetupScreensDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val instrumentation=InstrumentationRegistry.getInstrumentation()
    private val context=instrumentation.targetContext
    private val app=context.graph
    private val device=UiDevice.getInstance(instrumentation)
    private val now=System.currentTimeMillis()
    private var dark by mutableStateOf(false)
    private fun peer(name:String,id:Char,scope:SetupScope?=SetupScope.ENABLED,seen:Long=now-60000,via:Peer?=null,supported:Boolean=true)=
        Peer(name,id.toString().repeat(64),id.toString().repeat(32),listOf(Endpoint("192.168.1.${31+id.code%20}",47821)),lastConnected=seen,
            setupDecision=scope,addedViaId=via?.deviceId,addedViaName=via?.name,setupSupported=supported)
    private val pc=peer("MJStanton-Desktop",'d');private val mac=peer("MacBook Air",'e');private val office=peer("Office-PC",'2',SetupScope.DISABLED)
    private val wifi=LinkState("Connected","Wi-Fi",capabilities=setOf("files","clipboard_text","clipboard_image","mesh"))
    private val connecting=LinkState("Connecting…")
    private fun member(p:Peer,role:String="computer")=SetupMember(p.deviceId,p.pin,role,p.name,"member","1","a".repeat(32))
    private val pixel=SetupMember("9".repeat(32),"9".repeat(64),"phone","Pixel 9","member","1","a".repeat(32))

    @Test(timeout=60000) fun readmissionPromptHasBothActionsInLightAndDark()=runBlocking<Unit> {
        var kept=0;var allowed=0
        val p=prefs(pc)
        show {
            Home(p,mapOf(pc.deviceId to wifi))
            ReadmissionDialog("MacBook Air was removed from this phone. MJStanton-Desktop added it again",{},{kept++}) {allowed++}
        }
        compose.onNodeWithText("Keep removed").assertIsDisplayed()
        compose.onNodeWithText("Allow").assertIsDisplayed()
        bothThemes("readmission-prompt")
        compose.onNodeWithText("Keep removed").performClick();assertEquals(1,kept);assertEquals(0,allowed)
        compose.onNodeWithText("Allow").performClick();assertEquals(1,allowed)
    }
    @Test(timeout=60000) fun authenticatedRefusedIntroducedRowAndPageKeepTrust()=runBlocking<Unit> {
        val refused=mac.copy(lastConnected=0,addedViaId=pc.deviceId,addedViaName=pc.name,setupSupported=false,setupRefused=true)
        val p=prefs(pc,refused,state=linked(member(pc),member(refused)))
        var page by mutableStateOf(false)
        show {if(page)Device(p,refused) else Home(p,mapOf(pc.deviceId to wifi,refused.deviceId to LinkState()))}
        val diagnostic="MacBook Air doesn't recognize this phone · Pair with it directly"
        compose.onNodeWithText(diagnostic).assertIsDisplayed()
        bothThemes("readmission-refused-row")
        page=true;compose.waitForIdle()
        compose.onNodeWithText(diagnostic).assertIsDisplayed()
        compose.onNodeWithText("Update Pneu on MacBook Air to sync these switches.").assertDoesNotExist()
        bothThemes("readmission-refused-page")
    }
    @Test(timeout=60000) fun readmissionNotificationUsesTheLinkRequestsChannel()=runBlocking<Unit> {
        val removed=member(mac).copy(status="removed",hlc="10",reach=null)
        val state=linked(member(pc),removed).copy(barriers=mapOf(removed.key to RemovalBarrier(removed,readmission=Readmission(member(mac).copy(hlc="20"),pc.deviceId,pc.name,3))))
        val p=prefs(pc,state=state);val prompt=SetupPresentation.readmissions(p).single()
        // Give the notification collector the same durable state as the fixture.
        app.preferences.store.edit {SetupStorage.write(it,state)}
        device.pressHome()
        SetupNotifications.channel(context)
        for(night in listOf(false,true)) {
            device.executeShellCommand("cmd uimode night ${if(night)"yes" else "no"}")
            SetupNotifications.postReadmission(context,p,prompt)
            device.openNotification()
            assertTrue(device.wait(Until.hasObject(By.text("Keep removed")),5000))
            assertTrue(device.wait(Until.hasObject(By.text("Allow")),5000))
            Thread.sleep(500)
            assertTrue(device.wait(Until.hasObject(By.text("Keep removed")),5000))
            shade("readmission-notification-${if(night)"dark" else "light"}")
            context.getSystemService(NotificationManager::class.java).cancelAll()
            device.pressBack();device.pressHome()
        }
    }
    @Test(timeout=30000) fun introducedComputerDoesNotCompleteAnotherScannedTarget() {
        var current by mutableStateOf(Prefs(deviceId="a".repeat(32),pairingRevision=1))
        var home=0
        val raw="pneu://pair?v=1&name=Target&fp=${"b".repeat(64)}&addr=127.0.0.1%3A1&token=${"f".repeat(64)}&exp=9999999999999"
        show {PairScreen(current,LinkState(),{home++},{},{},lastAttemptedInvitation=raw,initialPairingRevision=1,cameraPreview={_,_->})}
        compose.waitForIdle()
        val introduced=mac.copy(generation=2)
        compose.runOnIdle {current=current.copy(peers=listOf(introduced),pairingRevision=2)}
        compose.waitForIdle();assertEquals(0,home)
        compose.onNodeWithText("Paired with MacBook Air").assertDoesNotExist()
        compose.onNodeWithText("Scan the QR code in Pneu on your computer").assertIsDisplayed()
    }
    @Before fun reset()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))
            instrumentation.uiAutomation.grantRuntimePermission(context.packageName,permission)
    }
    @After fun cleanup()=runBlocking<Unit> {
        device.executeShellCommand("cmd uimode night no")
        context.getSystemService(NotificationManager::class.java).cancelAll()
        DeviceFixture.reset()
    }

    private fun capture(name:String) {
        compose.waitForIdle()
        Thread.sleep(500) // Let dialog entry, snackbars, system bars and switch animations settle.
        val bitmap=requireNotNull(instrumentation.uiAutomation.takeScreenshot()) {"Screenshot unavailable: $name"}
        try {
            val target=File(context.filesDir,"setup-screens/$name.png").apply {parentFile!!.mkdirs()}
            target.outputStream().use {assertTrue("PNG save failed: $name",bitmap.compress(Bitmap.CompressFormat.PNG,100,it))}
        }finally {bitmap.recycle()}
    }
    private fun theme(dark:Boolean) {
        this.dark=dark
        val transparent=android.graphics.Color.TRANSPARENT
        compose.activityRule.scenario.onActivity {
            val style=if(dark)SystemBarStyle.dark(transparent) else SystemBarStyle.light(transparent,transparent)
            it.enableEdgeToEdge(style,style)
        }
    }
    /** Light first, then dark, with matching system bar icons. */
    private fun bothThemes(name:String,between:()->Unit={}) {
        for(dark in listOf(false,true)) {theme(dark);between();capture("$name-${if(dark)"dark" else "light"}")}
    }
    private fun show(content:@Composable ()->Unit) {
        compose.setContent {
            CompositionLocalProvider(LocalUiActions provides UiActions {message,_->fail("Unexpected fixture action: $message")}) {
                PneuTheme(dark=dark) {Surface(Modifier.fillMaxSize(),color=LocalPneuColors.current.bg) {content()}}
            }
        }
    }
    private suspend fun prefs(vararg peers:Peer,state:SetupState=SetupState())=
        app.preferences.read().copy(setup=true,peer=peers.first(),peers=peers.toList(),photos=false,recordings=false,membership=state)
    private suspend fun linked(vararg members:SetupMember,generation:Long=3):SetupState {
        val self=app.preferences.read().deviceId
        return SetupState("f".repeat(32),generation,listOf(SetupMember(self,"a".repeat(64),"phone","Galaxy S25+","member","1",self))+members)
    }

    @Composable private fun Home(p:Prefs,links:Map<String,LinkState>,setup:HomeSetup=HomeSetup(phones=SetupPresentation.phones(p)),message:String?=null,action:String?=null) {
        var scrolled by remember {mutableStateOf(false)}
        val snackbar=remember {SnackbarHostState()}
        LaunchedEffect(message) {if(message!=null)snackbar.showSnackbar(message,actionLabel=action,duration=SnackbarDuration.Indefinite)}
        Scaffold(containerColor=LocalPneuColors.current.bg,contentWindowInsets=WindowInsets.statusBars,topBar={HomeTopBar(scrolled){}},snackbarHost={PneuSnackbarHost(snackbar,Modifier.navigationBarsPadding())}) {padding->
            HomeScreen(p,links.values.firstOrNull() ?: LinkState(),emptyList(),Health(true,true,true,true),Modifier.padding(padding),{},{},{scrolled=it},emptyMap(),
                DeviceProjection.states(p,links),links,setup)
        }
    }
    @Composable private fun Device(p:Prefs,peer:Peer,link:LinkState=wifi) {
        DeviceScreen(p,DeviceState(peer,link,emptySet(),true),{},{},{},{}) {_,_->}
    }
    /** The scanner behind a first pairing or the removed phone's dialog. */
    @Composable private fun Scanner(p:Prefs) {
        PairScreen(p,LinkState(),{},{},{},cameraPreview={modifier,_->Box(modifier.background(Color(0xFF2B2E33)))})
    }

    @Test(timeout=60000) fun pairingConfirmationHasTheSwitchOnByDefault()=runBlocking<Unit> {
        val first=app.preferences.read().copy(setup=true)
        var on by mutableStateOf(true);var limit by mutableStateOf(false);var overHome by mutableStateOf(false)
        val home=prefs(pc)
        show {
            if(overHome)Home(home,mapOf(pc.deviceId to wifi)) else Scanner(first)
            PneuTheme(dark=if(overHome)dark else true) {
                PairConfirmDialog(context.getString(R.string.pair_confirm_title,"MacBook Air"),
                    context.getString(R.string.action_pair),on,limit,{on=it},{}) {}
            }
        }
        compose.onNodeWithText("Also link with your other devices").assertIsDisplayed()
        compose.onNode(isToggleable()).assertIsOn()
        capture("confirm-scanner-dark")
        compose.onNode(isToggleable()).performClick();assertFalse(on);compose.onNode(isToggleable()).assertIsOff()
        on=true;limit=true;compose.waitForIdle()
        compose.onNodeWithText("Your devices are at the limit of 17").assertIsDisplayed()
        compose.onNode(isToggleable()).assertIsOff().assertIsNotEnabled()
        capture("confirm-limit-scanner-dark")
        limit=false;overHome=true;compose.waitForIdle()
        compose.onNodeWithText("Pair with MacBook Air?").assertIsDisplayed()
        bothThemes("confirm-over-home")
        limit=true;compose.waitForIdle()
        bothThemes("confirm-limit-over-home")
    }
    @Test(timeout=60000) fun homeOffersLinkOnceThenSaysWhereToAllow()=runBlocking<Unit> {
        val before=prefs(pc.copy(setupDecision=SetupScope.ASK),mac.copy(setupDecision=SetupScope.ASK))
        val after=prefs(pc,mac,state=linked(generation=4))
        val done=prefs(pc,mac,state=linked(member(pc),member(mac),generation=4))
        var linkTapped by mutableStateOf(false);var finished by mutableStateOf(false)
        show {
            val links=mapOf(pc.deviceId to wifi,mac.deviceId to wifi)
            if(finished)Home(done,links,HomeSetup(SetupPresentation.banner(done,SetupUiState())),SetupPresentation.newsText(context.resources,SetupNews.Linked))
            else if(!linkTapped)Home(before,links,HomeSetup(SetupPresentation.banner(before,SetupUiState()),link={linkTapped=true}))
            else Home(after,links,HomeSetup(SetupPresentation.banner(after,SetupUiState(linkStarted=true))))
        }
        compose.onNodeWithText("Link your devices, so new ones pair just once").assertIsDisplayed()
        bothThemes("home-link-banner")
        compose.onNodeWithText("Link").performClick();assertTrue(linkTapped)
        compose.onNodeWithText("Allow linking on MJStanton-Desktop and MacBook Air").assertIsDisplayed()
        compose.onNodeWithText("Link").assertDoesNotExist()
        bothThemes("home-link-waiting")
        // Done: the banner goes and Home says so once.
        finished=true;compose.waitForIdle()
        compose.onNodeWithText("Your devices are linked").assertIsDisplayed()
        bothThemes("home-linked")
    }
    @Test(timeout=60000) fun plannedSetupRefreshKeepsTheConnectedPresentation()=runBlocking<Unit> {
        val p=prefs(pc,state=linked(member(pc)))
        val display=io.github.stantonmatt.pneu.connection.SetupRefreshDisplay()
        assertTrue(display.begin(pc,wifi,0))
        val refreshing=display.project(pc,LinkState("Connecting…"),1)
        var page by mutableStateOf(false)
        show {if(page)Device(p,pc,refreshing) else Home(p,mapOf(pc.deviceId to refreshing))}
        compose.onNodeWithText("Not connected",substring=true).assertDoesNotExist()
        bothThemes("setup-refresh-home")
        page=true;compose.waitForIdle()
        compose.onNodeWithText("Can't connect?",substring=true).assertDoesNotExist()
        compose.onNodeWithText("Not connected",substring=true).assertDoesNotExist()
        bothThemes("setup-refresh-page")
    }
    @Test(timeout=60000) fun aPhoneThatJoinedListsTheOthersAndAnotherPhoneIsGrey()=runBlocking<Unit> {
        val added=mac.copy(addedViaId=pc.deviceId,addedViaName=pc.name,lastConnected=0)
        val state=linked(member(pc),member(added),pixel).copy(provenance=mapOf(pixel.key to SetupProvenance(pc.deviceId,pc.name)))
        val p=prefs(pc,added,state=state)
        var links by mutableStateOf(mapOf(pc.deviceId to wifi,added.deviceId to connecting))
        show {Home(p,links,message=context.getString(R.string.setup_also_linked,"MacBook Air"))}
        compose.onNodeWithText("Also linked: MacBook Air").assertIsDisplayed()
        compose.onNodeWithText("Connecting…").assertIsDisplayed()
        compose.onNodeWithText("Phones don't connect to each other").assertIsDisplayed()
        bothThemes("home-also-linked")
        links=mapOf(pc.deviceId to wifi);compose.waitForIdle()
        compose.onNodeWithText("Not connected yet").assertIsDisplayed()
        bothThemes("home-not-connected-yet")
    }
    @Test(timeout=60000) fun requestDialogNamesEveryDeviceThatAsked()=runBlocking<Unit> {
        val p=prefs(pc.copy(setupDecision=SetupScope.ASK),mac.copy(setupDecision=SetupScope.ASK))
        var names by mutableStateOf(listOf(pc.name))
        show {
            Home(p,mapOf(pc.deviceId to wifi,mac.deviceId to wifi),HomeSetup())
            LinkRequestDialog(SetupPresentation.requestTitle(context.resources,names),{},{}) {}
        }
        compose.onNodeWithText("MJStanton-Desktop wants to link your devices").assertIsDisplayed()
        compose.onNodeWithText("New devices then pair just once.").assertIsDisplayed()
        compose.onNodeWithText("Don't allow").assertIsDisplayed();compose.onNodeWithText("Allow").assertIsDisplayed()
        bothThemes("request-dialog")
        names=listOf(pc.name,mac.name);compose.waitForIdle()
        compose.onNodeWithText("MJStanton-Desktop and MacBook Air want to link your devices").assertIsDisplayed()
        bothThemes("request-dialog-two")
    }
    @Test(timeout=90000) fun devicePageLastCardInEveryState()=runBlocking<Unit> {
        val plain=pc.copy(setupDecision=SetupScope.ASK)
        val cases=listOf(
            "page-linked" to (prefs(pc,mac,state=linked(member(pc),member(mac))) to mac),
            "page-added-via" to (prefs(pc,mac.copy(addedViaId=pc.deviceId,addedViaName=pc.name),state=linked(member(pc),member(mac))) to mac.copy(addedViaId=pc.deviceId,addedViaName=pc.name)),
            "page-link" to (prefs(plain,state=SetupState()) to plain),
            "page-pending" to (prefs(pc,state=linked()) to pc),
            "page-theirs" to (prefs(pc.copy(remoteSetupScope=SetupScope.DISABLED),state=linked()) to pc.copy(remoteSetupScope=SetupScope.DISABLED)),
            "page-asked" to (prefs(plain,state=SetupState(prompts=listOf(SetupPrompt(plain.deviceId,plain.pin,plain.generation,0,plain.name)))) to plain),
            "page-update" to (prefs(plain.copy(setupSupported=false)) to plain.copy(setupSupported=false)),
            "page-separate" to (prefs(pc,office,state=linked(member(pc)).copy(separateLinks=setOf(office.deviceId))) to office),
            "page-member-not-linked" to (prefs(pc,mac.copy(setupDecision=SetupScope.DISABLED),state=linked(member(pc),member(mac))) to mac.copy(setupDecision=SetupScope.DISABLED)),
        )
        val expected=mapOf("page-linked" to "Remove from all devices","page-added-via" to "Added via MJStanton-Desktop","page-link" to "Link with your other devices",
            "page-pending" to "Allow linking on MJStanton-Desktop","page-theirs" to "Link on MJStanton-Desktop too","page-asked" to "Wants to link your devices","page-update" to "Update Pneu on MJStanton-Desktop to link it",
            "page-separate" to "It already has its own linked devices","page-member-not-linked" to "Link with your other devices")
        var current by mutableStateOf(cases.first().second)
        show {Device(current.first,current.second)}
        for((name,case) in cases) {
            current=case;compose.waitForIdle()
            val bottom=if(case.first.membership.member(case.second.deviceId)!=null)"Remove from all devices" else "Unpair"
            compose.onNodeWithText(bottom).performScrollTo().assertIsDisplayed()
            compose.onNodeWithText(expected.getValue(name)).assertIsDisplayed()
            if(name=="page-theirs")compose.onNodeWithText("Link with your other devices").assertIsDisplayed()
            bothThemes(name)
        }
        compose.onNodeWithText("Remove from all devices").assertIsDisplayed()
    }
    @Test(timeout=60000) fun anotherPhoneHasOnlyItsLineAddedViaAndRemove()=runBlocking<Unit> {
        show {PhoneMemberScreen(PhoneMember(pixel.key,"Pixel 9","MJStanton-Desktop"),{}) {}}
        compose.onNodeWithText("Phones don't connect to each other.").assertIsDisplayed()
        compose.onNodeWithText("Added via MJStanton-Desktop").assertIsDisplayed()
        compose.onNodeWithText("Remove from all devices").assertIsDisplayed()
        compose.onNodeWithText("Send files").assertDoesNotExist()
        bothThemes("phone-member-page")
    }
    @Test(timeout=60000) fun removingSaysWhatIsTrue()=runBlocking<Unit> {
        val via=mac.copy(addedViaId=pc.deviceId,addedViaName=pc.name)
        val p=prefs(pc,via,state=linked(member(pc),member(via)))
        val after=prefs(pc,state=linked(member(pc)))
        var stage by mutableIntStateOf(0)
        show {
            if(stage<3)Device(p,via) else Home(after,mapOf(pc.deviceId to wifi),message=context.getString(R.string.setup_removed,"MacBook Air"))
            when(stage) {
                0->RemoveFromAllDialog("MacBook Air",0,{}) {}
                1->RemoveFromAllDialog("MacBook Air",2,{}) {}
                2->SetupNoticeDialog(context.getString(R.string.setup_remove_failed_title,"MacBook Air"),context.getString(R.string.setup_remove_failed_text)) {}
            }
        }
        compose.onNodeWithText("Remove MacBook Air from all devices?").assertIsDisplayed()
        compose.onNodeWithText("It's removed from this phone now, and from your other devices when they next connect.").assertIsDisplayed()
        bothThemes("remove-confirm")
        stage=1;compose.waitForIdle()
        compose.onNodeWithText("2 waiting items won't be sent to it.",substring=true).assertIsDisplayed()
        bothThemes("remove-confirm-waiting")
        stage=2;compose.waitForIdle()
        compose.onNodeWithText("Couldn't remove MacBook Air").assertIsDisplayed();compose.onNodeWithText("Nothing was changed. Try again.").assertIsDisplayed()
        bothThemes("remove-failed")
        stage=3;compose.waitForIdle()
        compose.onNodeWithText("MacBook Air removed. Your other devices update when they connect.").assertIsDisplayed()
        bothThemes("removed-home")
    }
    @Test(timeout=60000) fun theRemovedPhoneSaysSoOnceOverTheScanner()=runBlocking<Unit> {
        val p=app.preferences.read().copy(setup=true)
        // With other pairings left it stays on Home.
        val home=prefs(office.copy(setupDecision=SetupScope.DISABLED))
        var onHome by mutableStateOf(false)
        show {
            if(onHome) {Home(home,mapOf(office.deviceId to wifi));SetupNoticeDialog(context.getString(R.string.setup_removed_self),null) {}}
            else {Scanner(p);PneuTheme(dark=true) {SetupNoticeDialog(context.getString(R.string.setup_removed_self),null) {}}}
        }
        compose.onNodeWithText("This phone was removed from your devices").assertIsDisplayed()
        capture("removed-self-scanner-dark")
        onHome=true;compose.waitForIdle()
        bothThemes("removed-self-home")
    }
    @Test(timeout=60000) fun separateDevicesAreDescribedWithoutAWarning()=runBlocking<Unit> {
        val p=prefs(pc,mac,office,state=linked(member(pc),member(mac)).copy(separateLinks=setOf(office.deviceId)))
        var afterPairing by mutableStateOf(true)
        show {Home(p,mapOf(pc.deviceId to wifi,mac.deviceId to wifi,office.deviceId to wifi),
            message=SetupPresentation.newsText(context.resources,SetupNews.Separate(office.deviceId,office.name,afterPairing)))}
        compose.onNodeWithText("Paired with Office-PC only. It already has its own linked devices.").assertIsDisplayed()
        bothThemes("separate-after-pairing")
        afterPairing=false;compose.waitForIdle()
        compose.onNodeWithText("Office-PC wasn't linked. It already has its own linked devices.").assertIsDisplayed()
        bothThemes("separate-after-link")
    }
    @Test(timeout=60000) fun joinedSaysAddedViaWithRemove()=runBlocking<Unit> {
        val via=mac.copy(addedViaId=pc.deviceId,addedViaName=pc.name)
        val p=prefs(pc,via,state=linked(member(pc),member(via)))
        show {Home(p,mapOf(pc.deviceId to wifi,via.deviceId to wifi),message=SetupPresentation.newsText(context.resources,SetupNews.Joined(SetupNotice("joined",via.deviceId,via.name,pc.deviceId,pc.name),null)),action=context.getString(R.string.setup_action_remove))}
        compose.onNodeWithText("MacBook Air joined · Added via MJStanton-Desktop").assertIsDisplayed();compose.onNodeWithText("Remove").assertIsDisplayed()
        bothThemes("joined-in-app")
    }
    /** The system notifications, in the shade, light and dark. */
    @Test(timeout=90000) fun notificationsInTheShade()=runBlocking<Unit> {
        val via=mac.copy(addedViaId=pc.deviceId,addedViaName=pc.name)
        val p=prefs(pc,via,state=linked(member(pc),member(via)))
        val notice=SetupNotice("3/${via.deviceId}/1",via.deviceId,via.name,pc.deviceId,pc.name)
        val plain=pc.copy(setupDecision=SetupScope.ASK)
        val asked=prefs(plain,state=SetupState(prompts=listOf(SetupPrompt(plain.deviceId,plain.pin,plain.generation,0,plain.name))))
        SetupNotifications.channel(context)
        for(night in listOf(false,true)) {
            device.executeShellCommand("cmd uimode night ${if(night)"yes" else "no"}");Thread.sleep(1500)
            val suffix=if(night)"dark" else "light"
            SetupNotifications.postJoined(context,p,notice)
            device.openNotification()
            assertTrue(device.wait(Until.hasObject(By.text("MacBook Air joined")),5000))
            Thread.sleep(800);shade("notification-joined-$suffix")
            context.getSystemService(NotificationManager::class.java).cancelAll()
            repeat(3) {warning->
                context.getSystemService(NotificationManager::class.java).notify(700+warning,
                    androidx.core.app.NotificationCompat.Builder(context,"alerts").setSmallIcon(R.drawable.ic_stat_pneu)
                        .setContentTitle("Pneu warning $warning").build())
            }
            SetupNotifications.postRequest(context,asked,SetupPresentation.prompts(asked))
            assertTrue(device.wait(Until.hasObject(By.text("MJStanton-Desktop wants to link your devices")),5000))
            shade("notification-request-$suffix")
            assertTrue("Allow stays visible beside warnings",device.wait(Until.hasObject(By.text("Allow")),5000))
            assertTrue("Don't allow stays visible beside warnings",device.wait(Until.hasObject(By.text("Don't allow")),5000))
            Thread.sleep(800);shade("notification-request-$suffix")
            context.getSystemService(NotificationManager::class.java).cancelAll()
            device.pressBack();device.pressHome()
        }
    }
    private fun shade(name:String) {
        val bitmap=requireNotNull(instrumentation.uiAutomation.takeScreenshot())
        try {File(context.filesDir,"setup-screens/$name.png").apply {parentFile!!.mkdirs()}.outputStream().use {bitmap.compress(Bitmap.CompressFormat.PNG,100,it)}}
        finally {bitmap.recycle()}
    }
}
