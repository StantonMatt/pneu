package io.github.stantonmatt.pneu

import android.Manifest
import android.app.NotificationManager
import android.content.Intent
import android.graphics.Bitmap
import android.net.Uri
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.datastore.preferences.core.edit
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.By
import androidx.test.uiautomator.UiDevice
import androidx.test.uiautomator.Until
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.File

/** Part C's key flows in the real MainActivity, with real settings: the pairing switch,
 * the Allow request (durable until answered), and Remove from all devices. */
@RunWith(AndroidJUnit4::class)
class SetupFlowDeviceTest {
    @get:Rule val compose=createEmptyComposeRule()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val device=UiDevice.getInstance(i)
    private val pc="b".repeat(32);private val mac="c".repeat(32)
    private var scenario:ActivityScenario<MainActivity>?=null
    private fun raw(name:String,pin:Char)="pneu://pair?v=1&name=${Uri.encode(name)}&fp=${pin.toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    private suspend fun bind(id:String,name:String,pin:Char) {
        val uri=raw(name,pin);assertTrue(app.requestPairing(uri,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(uri),id))
        withTimeout(5000) {app.prefs.first {it.devices.any {peer->peer.deviceId==id}}}
    }
    /** Writes the setup state the way the core persists it, in one edit. */
    private suspend fun setup(change:(List<Peer>,SetupState)->Pair<List<Peer>,SetupState>) {
        app.preferences.store.edit {p->
            val (peers,state)=change(PeerRegistry.read(p),SetupStorage.read(p))
            PeerRegistry.write(p,peers);SetupStorage.write(p,state)
        }
    }
    private suspend fun self():SetupMember {
        val p=app.preferences.read();val identity=app.setupIdentity(p)
        return SetupMember(p.deviceId,identity.pin,"phone",identity.name,"member","1",p.deviceId)
    }
    private fun member(peer:Peer)=SetupMember(peer.deviceId,peer.pin,"computer",peer.name,"member","1",peer.deviceId)
    private fun launch(intent:Intent=Intent(c,MainActivity::class.java).putExtra("home",true)) {
        scenario=ActivityScenario.launch(intent.setClass(c,MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
    }
    private fun waitFor(text:String,substring:Boolean=false,timeout:Long=10000) {
        compose.waitUntil(timeout) {compose.onAllNodesWithText(text,substring=substring).fetchSemanticsNodes().isNotEmpty()}
    }
    /** Home → a device's page: its row is the last exact match. */
    private fun openDevice(name:String) {
        waitFor(name)
        compose.onAllNodesWithText(name).onLast().performClick()
        compose.waitUntil(10000) {compose.onAllNodesWithContentDescription("Back").fetchSemanticsNodes().isNotEmpty()}
    }
    private fun capture(name:String) {
        compose.waitForIdle();Thread.sleep(500)
        val bitmap=requireNotNull(i.uiAutomation.takeScreenshot())
        try {File(c.filesDir,"setup-screens/$name.png").apply {parentFile!!.mkdirs()}.outputStream().use {bitmap.compress(Bitmap.CompressFormat.PNG,100,it)}}
        finally {bitmap.recycle()}
    }

    @Before fun reset()=runBlocking<Unit> {
        DeviceFixture.reset();device.pressHome();app.initialized.await()
        for(permission in listOf(Manifest.permission.CAMERA,Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))
            i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true)
        for(key in listOf("screenshots","photos","recordings"))app.preferences.toggle(key,false)
        c.setupUi.update {SetupUiState()};c.setupUi.dismissedPrompts.value=emptySet();c.setupUi.notifiedPrompts.value=emptySet()
        c.setupUi.presentedNotices.value=emptySet();c.getSharedPreferences("setup_ui",0).edit().remove("presentedNotices").commit()
    }
    @After fun cleanup()=runBlocking<Unit> {
        scenario?.close();device.pressHome()
        device.executeShellCommand("cmd uimode night no")
        c.getSystemService(NotificationManager::class.java).cancelAll()
        app.cancelPairing()
        DeviceFixture.stopPneuService();app.connection.stop();app.connection.awaitStopped()
        c.setupUi.update {SetupUiState()};c.setupUi.dismissedPrompts.value=emptySet()
        DeviceFixture.reset()
    }

    private suspend fun arrangeReadmission():ReadmissionPrompt {
        bind(pc,"MJStanton-Desktop",'b')
        val me=self()
        setup {peers,state->
            val generation=state.generation+1
            val removed=SetupMember(mac,"c".repeat(64),"computer","MacBook Air","removed","10",me.deviceId)
            val retained=removed.copy(status="member",hlc="20",writer=pc,reach=MemberReach(listOf("127.0.0.1:1"),"21",mac),readmits=MemberReadmits(removed.hlc,removed.writer))
            peers to state.copy(setupId="f".repeat(32),generation=generation,roster=listOf(me,member(peers.single()),removed),
                barriers=mapOf(removed.key to RemovalBarrier(removed,readmission=Readmission(retained,pc,"MJStanton-Desktop",generation))))
        }
        return SetupPresentation.readmissions(app.preferences.read()).single()
    }
    @Test(timeout=90000) fun unclaimedNewerMemberNeverAsksAndOnlyAuthoredClaimAsks()=runBlocking<Unit> {
        val prompt=arrangeReadmission()
        setup {peers,state->peers.map {it.copy(setupDecision=SetupScope.ENABLED)} to state.copy(
            barriers=state.barriers.mapValues {(_,barrier)->barrier.copy(readmission=null,authored=true,revoked_binding=true)})}
        val p=app.preferences.read();val sender=p.devices.single();val identity=app.setupIdentity(p)
        val candidate=prompt.removal.copy(status="member",hlc="20",writer=pc)
        suspend fun receive(m:SetupMember) {
            val snap=MembersSnapshot(randomId(),p.membership.setupId!!,listOf(p.membership.member(p.deviceId)!!,member(sender),m))
            app.receiveSetupMembers(DeliveryBinding(p.deviceId,sender.pin,pc,sender.generation),p.membership.generation,snap.setupId,snap.json(),identity) {true}
        }
        launch();waitFor("MJStanton-Desktop")
        receive(candidate)
        assertTrue(SetupPresentation.readmissions(app.preferences.read()).isEmpty())
        compose.waitForIdle();compose.onNodeWithText("Keep removed").assertDoesNotExist()
        receive(candidate.copy(readmits=MemberReadmits(prompt.removal.hlc,prompt.removal.writer)))
        waitFor("MacBook Air was removed from this phone. MJStanton-Desktop added it again")
        compose.onNodeWithText("Keep removed").assertIsDisplayed()
        compose.onNodeWithText("Allow").assertIsDisplayed()
    }
    @Test(timeout=90000) fun inheritedClaimInstallsWithoutDialogAndOldRemovalKeepsTheSameBinding()=runBlocking<Unit> {
        val prompt=arrangeReadmission()
        setup {peers,state->peers.map {it.copy(setupDecision=SetupScope.ENABLED)} to state.copy(
            barriers=state.barriers.mapValues {(_,barrier)->barrier.copy(readmission=null,authored=false,revoked_binding=true)})}
        val p=app.preferences.read();val sender=p.devices.single();val identity=app.setupIdentity(p)
        val candidate=prompt.removal.copy(status="member",hlc="20",writer=pc,readmits=MemberReadmits(prompt.removal.hlc,prompt.removal.writer))
        suspend fun receive(m:SetupMember) {
            val snap=MembersSnapshot(randomId(),p.membership.setupId!!,listOf(p.membership.member(p.deviceId)!!,member(sender),m))
            app.receiveSetupMembers(DeliveryBinding(p.deviceId,sender.pin,pc,sender.generation),p.membership.generation,snap.setupId,snap.json(),identity) {true}
        }
        receive(candidate)
        val admitted=app.preferences.read();val peer=admitted.devices.single {it.deviceId==mac}
        assertEquals(pc,peer.addedViaId);assertFalse(peer.readyObserved)
        assertFalse(admitted.membership.denies(mac,peer.pin));assertTrue(SetupPresentation.readmissions(admitted).isEmpty())
        receive(prompt.removal)
        assertEquals(peer,app.preferences.read().devices.single {it.deviceId==mac})
        launch();waitFor("MacBook Air")
        compose.onNodeWithText("Keep removed").assertDoesNotExist()
        openDevice("MacBook Air")
        waitFor("Hasn't connected")
        compose.onNodeWithText("Open Pneu on MacBook Air, or pair with it directly").assertIsDisplayed()
        compose.onNodeWithText("MacBook Air doesn't recognize this phone",substring=true).assertDoesNotExist()
        capture("flow-inherited-claim-neutral-page")
    }
    @Test(timeout=90000) fun dismissedMigrationRequestDoesNotHideReadmission()=runBlocking<Unit> {
        arrangeReadmission();askedBy(pc)
        launch();waitFor("MJStanton-Desktop wants to link your devices")
        compose.onNodeWithText("Keep removed").assertDoesNotExist()
        device.pressBack()
        waitFor("MacBook Air was removed from this phone. MJStanton-Desktop added it again")
        compose.onNodeWithText("Keep removed").assertIsDisplayed()
        compose.onNodeWithText("Allow").assertIsDisplayed()
        assertTrue("Dismissal leaves migration pending",SetupPresentation.prompts(app.preferences.read()).isNotEmpty())
        assertTrue(MainActivity.foreground.value)
        compose.onNodeWithText("Keep removed").performClick()
        withTimeout(10000) {app.prefs.first {it.membership.barriers.values.single().readmission?.decision==ReadmissionDecision.KEPT}}
    }
    @Test(timeout=90000) fun readmissionAllowInstallsOnceAndAuthenticatedRefusalHasADirectPairHint()=runBlocking<Unit> {
        val prompt=arrangeReadmission()
        launch();waitFor("MacBook Air was removed from this phone. MJStanton-Desktop added it again")
        compose.onNodeWithText("Keep removed").assertIsDisplayed();capture("flow-readmission-prompt")
        compose.onNodeWithText("Allow").performClick()
        withTimeout(10000) {app.prefs.first {it.devices.any {peer->peer.deviceId==mac}}}
        val admitted=app.preferences.read();val peer=admitted.devices.single {it.deviceId==mac}
        assertFalse(admitted.membership.denies(mac,peer.pin));assertEquals("20",admitted.membership.member(mac)!!.hlc)
        assertEquals(pc,peer.addedViaId);assertFalse(peer.established);assertTrue(admitted.membership.notices.isEmpty())
        app.answerReadmission(prompt,true)
        assertEquals(peer,app.preferences.read().devices.single {it.deviceId==mac})
        app.preferences.introducedRefusal(DeliveryBinding(admitted.deviceId,peer.pin,mac,peer.generation),true)
        waitFor("MacBook Air doesn't recognize this phone · Pair with it directly")
        capture("flow-readmission-refused-row")
        openDevice("MacBook Air")
        waitFor("MacBook Air doesn't recognize this phone · Pair with it directly")
        capture("flow-readmission-refused-page")
        assertEquals(peer.pin,app.preferences.read().devices.single {it.deviceId==mac}.pin)
    }
    @Test(timeout=90000) fun keepRemovedSurvivesReopenAndStaleNotificationCannotAllowANewerBarrier()=runBlocking<Unit> {
        val prompt=arrangeReadmission()
        SetupNotifications.channel(c)
        SetupNotifications.postReadmission(c,app.preferences.read(),prompt)
        val notification=c.getSystemService(NotificationManager::class.java).activeNotifications.single {it.tag==prompt.actionKey}.notification
        val capturedAllow=notification.actions.last().actionIntent
        launch();waitFor("Keep removed")
        compose.onNodeWithText("Keep removed").performClick()
        withTimeout(10000) {app.prefs.first {it.membership.barriers[prompt.key]?.readmission?.decision==ReadmissionDecision.KEPT}}
        scenario?.close();launch();waitFor("MJStanton-Desktop")
        compose.onNodeWithText("Keep removed").assertDoesNotExist()
        assertTrue(app.preferences.read().membership.denies(mac,"c".repeat(64)))
        setup {peers,state->peers to state.copy(barriers=state.barriers+(prompt.key to RemovalBarrier(prompt.removal.copy(hlc="30"))))}
        val before=app.preferences.read()
        val completed=CompletableDeferred<Unit>()
        capturedAllow.send(c,0,null,{_,_,_,_,_->completed.complete(Unit)},android.os.Handler(android.os.Looper.getMainLooper()))
        withTimeout(10000) {completed.await()}
        app.answerReadmission(prompt,true)
        assertEquals(before.membership,app.preferences.read().membership)
        assertTrue(app.preferences.read().devices.none {it.deviceId==mac})
    }
    @Test(timeout=90000) fun pairingSwitchIsOnByDefaultAndOffMakesAPlainPairing()=runBlocking<Unit> {
        // A first pairing, from a deep link: the confirmation is over the scanner.
        launch(Intent(Intent.ACTION_VIEW,Uri.parse(raw("MacBook Air",'e'))))
        waitFor("Pair with MacBook Air?")
        compose.onNodeWithText("Also link with your other devices").assertIsDisplayed()
        compose.onNode(isToggleable()).assertIsOn()
        capture("flow-confirm-first-pairing")
        compose.onNode(isToggleable()).performClick().assertIsOff()
        compose.onNodeWithText("Pair").performClick()
        val plain=withTimeout(10000) {app.connection.attempt.first {it!=null}}!!
        assertFalse("Off makes a plain pairing",plain.setupEnabled)
        assertEquals("",c.setupUi.state.value.joinedPin)
        assertTrue(app.cancelPairing(plain));scenario?.close()
        // Default on, again with a new code.
        launch(Intent(Intent.ACTION_VIEW,Uri.parse(raw("MacBook Air",'f'))))
        waitFor("Pair with MacBook Air?")
        compose.onNode(isToggleable()).assertIsOn()
        compose.onNodeWithText("Pair").performClick()
        val linked=withTimeout(10000) {app.connection.attempt.first {it!=null && it.invitation.fingerprint=="f".repeat(64)}}!!
        assertTrue(linked.setupEnabled)
        assertEquals("f".repeat(64),c.setupUi.state.value.joinedPin)
        assertTrue(app.cancelPairing(linked))
    }
    @Test(timeout=90000) fun atTheLimitTheSwitchIsOffAndPairingIsPlain()=runBlocking<Unit> {
        val me=self()
        val others=(1..16).map {SetupMember("%032x".format(it),"%064x".format(it),"computer","PC $it","member","1",me.deviceId)}
        setup {peers,state->peers to state.copy(setupId="f".repeat(32),generation=state.generation+1,roster=listOf(me)+others)}
        launch(Intent(Intent.ACTION_VIEW,Uri.parse(raw("MacBook Air",'e'))))
        waitFor("Your devices are at the limit of 17")
        compose.onNode(isToggleable()).assertIsOff().assertIsNotEnabled()
        capture("flow-confirm-limit")
        compose.onNodeWithText("Pair").performClick()
        val attempt=withTimeout(10000) {app.connection.attempt.first {it!=null}}!!
        assertFalse(attempt.setupEnabled)
        assertTrue(app.cancelPairing(attempt))
    }
    private suspend fun askedBy(vararg ids:String) {
        setup {peers,state->
            val asking=peers.map {it.copy(setupDecision=SetupScope.ASK,setupSupported=true)}
            asking to state.copy(prompts=asking.filter {it.deviceId in ids}.map {SetupPrompt(it.deviceId,it.pin,it.generation,state.generation,it.name)})
        }
    }
    @Test(timeout=120000) fun allowRequestWaitsUntilAnsweredAndAllowLinks()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');bind(mac,"MacBook Air",'c')
        askedBy(pc)
        launch()
        waitFor("MJStanton-Desktop wants to link your devices")
        compose.onNodeWithText("New devices then pair just once.").assertIsDisplayed()
        capture("flow-request-dialog")
        // Back leaves it waiting: gone until Pneu restarts, and the device page keeps Allow.
        device.pressBack()
        compose.waitUntil(5000) {compose.onAllNodesWithText("MJStanton-Desktop wants to link your devices").fetchSemanticsNodes().isEmpty()}
        assertTrue(SetupPresentation.prompts(app.preferences.read()).isNotEmpty())
        openDevice("MJStanton-Desktop")
        compose.onNodeWithText("Wants to link your devices").performScrollTo().assertIsDisplayed()
        capture("flow-request-device-page")
        scenario?.close()
        // A new process forgets the dismissal: it asks again, once.
        c.setupUi.dismissedPrompts.value=emptySet()
        launch()
        waitFor("MJStanton-Desktop wants to link your devices")
        c.setupUi.update {it.copy(joinedPin="b".repeat(64),joinedAt=System.currentTimeMillis())}
        compose.onNodeWithText("Allow").performClick()
        assertEquals("",c.setupUi.state.value.joinedPin)
        val p=withTimeout(10000) {app.prefs.first {it.membership.prompts.all {prompt->prompt.allowRequested}}}
        assertEquals(1,p.membership.prompts.size)
        compose.waitUntil(5000) {compose.onAllNodesWithText("MJStanton-Desktop wants to link your devices").fetchSemanticsNodes().isEmpty()}
        // Allowed: nothing left to tap on its page until it reconnects and links.
        assertEquals(LinkRow.NONE,SetupPresentation.page(p,p.devices.first {it.deviceId==pc}).row)
    }
    @Test(timeout=90000) fun linkClearsTheRecentPairingOutcome()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b')
        setup {peers,state->peers.map {it.copy(setupDecision=SetupScope.ASK,setupSupported=true)} to state}
        c.setupUi.update {it.copy(joinedPin="b".repeat(64),joinedAt=System.currentTimeMillis())}
        launch();waitFor("Link your devices, so new ones pair just once")
        compose.onNodeWithText("Link").performClick()
        compose.waitUntil(5000) {c.setupUi.state.value.joinedPin.isEmpty()}
        assertEquals(0L,c.setupUi.state.value.joinedAt)
    }
    @Test(timeout=90000) fun dontAllowAnswersOnlyTheDeviceThatAsked()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');bind(mac,"MacBook Air",'c')
        askedBy(pc)
        launch()
        waitFor("MJStanton-Desktop wants to link your devices")
        compose.onNodeWithText("Don't allow").performClick()
        val p=withTimeout(10000) {app.prefs.first {it.membership.prompts.isEmpty()}}
        assertEquals(SetupScope.DISABLED,p.devices.first {it.deviceId==pc}.setupScope)
        assertEquals(SetupScope.ASK,p.devices.first {it.deviceId==mac}.setupScope)
        // The Mac's pairing is still undecided, so Home offers Link once.
        waitFor("Link your devices, so new ones pair just once")
        capture("flow-after-dont-allow")
    }
    @Test(timeout=90000) fun lastDeclineDropsTheBannerWithoutSuccess()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');bind(mac,"MacBook Air",'c')
        setup {peers,state->peers.map {it.copy(setupDecision=SetupScope.ASK,setupSupported=true)} to state}
        val before=app.preferences.read()
        val targets=SetupPresentation.linkTargets(before,pc)
        linkedWith(pc)
        setup {peers,state->peers.map {it.copy(remoteSetupScope=if(it.deviceId==pc)SetupScope.ENABLED else SetupScope.ASK)} to state}
        c.setupUi.update {it.copy(linkStarted=true,linkTargets=targets)}
        launch();waitFor("Allow linking on MacBook Air")
        setup {peers,state->peers.map {if(it.deviceId==mac)it.copy(remoteSetupScope=SetupScope.DISABLED) else it} to state}
        compose.waitUntil(5000) {compose.onAllNodesWithText("Allow linking on MacBook Air").fetchSemanticsNodes().isEmpty()}
        compose.waitUntil(5000) {!c.setupUi.state.value.linkStarted}
        for(night in listOf(false,true)) {
            device.executeShellCommand("cmd uimode night ${if(night)"yes" else "no"}")
            compose.waitForIdle();delay(700)
            compose.onNodeWithText("Your devices are linked").assertDoesNotExist()
            capture("flow-last-decline-${if(night)"dark" else "light"}")
        }
        scenario!!.recreate();waitFor("MacBook Air")
        compose.onNodeWithText("Your devices are linked").assertDoesNotExist()
        device.executeShellCommand("cmd uimode night no")
    }
    @Test(timeout=90000) fun notificationAllowAnswersTheRequest()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b')
        askedBy(pc)
        val p=app.preferences.read()
        SetupNotifications.channel(c)
        // Post into an already open shade, as in the notification screenshot test.
        // This avoids Android 17's heads-up reparenting animation retaining stale hit bounds.
        device.openNotification()
        SetupNotifications.postRequest(c,p,SetupPresentation.prompts(p))
        assertTrue(device.wait(Until.hasObject(By.text("MJStanton-Desktop wants to link your devices")),5000))
        device.waitForIdle()
        try {
            val allow=device.wait(Until.findObject(By.text("Allow")),5000) ?: device.wait(Until.findObject(By.text("ALLOW")),2000)
            assertNotNull("Allow action",allow);allow!!.click()
            withTimeout(10000) {app.prefs.first {it.membership.prompts.isNotEmpty() && it.membership.prompts.all {prompt->prompt.allowRequested}}}
        }catch(e:Throwable) {
            capture("flow-notification-allow-timeout")
            device.dumpWindowHierarchy(File(c.filesDir,"setup-screens/notification-allow-timeout.xml"))
            throw e
        }
        device.pressHome()
    }
    @Test(timeout=90000) fun staleNotificationActionCannotCancelANewerRequest()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');bind(mac,"MacBook Air",'c');askedBy(pc)
        SetupNotifications.channel(c)
        val first=app.preferences.read()
        SetupNotifications.postRequest(c,first,SetupPresentation.prompts(first))
        val nm=c.getSystemService(NotificationManager::class.java)
        val stale=nm.activeNotifications.single {it.tag==SetupNotifications.REQUEST}.notification.actions[0].actionIntent
        askedBy(mac)
        val next=app.preferences.read()
        SetupNotifications.postRequest(c,next,SetupPresentation.prompts(next))
        val keys=SetupPresentation.prompts(next).map(SetupPresentation::promptKey).toSet()
        c.setupUi.notifiedPrompts.value=keys
        val read=CompletableDeferred<Unit>()
        app.preferences.beforeReadStart={read.complete(Unit)}
        try {
            stale.send()
            withTimeout(5000) {read.await()}
            delay(800)
            assertTrue(nm.activeNotifications.any {it.tag==SetupNotifications.REQUEST})
            assertEquals(keys,c.setupUi.notifiedPrompts.value)
            assertEquals(listOf(mac),SetupPresentation.prompts(app.preferences.read()).map {it.peerId})
        }finally {app.preferences.beforeReadStart={}}
    }
    @Test(timeout=90000) fun pausedActivityDoesNotCancelTheBackgroundRequest()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');launch();waitFor("MJStanton-Desktop")
        scenario!!.moveToState(androidx.lifecycle.Lifecycle.State.STARTED)
        askedBy(pc)
        val nm=c.getSystemService(NotificationManager::class.java)
        withTimeout(10000) {while(nm.activeNotifications.none {it.tag==SetupNotifications.REQUEST})delay(100)}
        delay(800) // Give the paused composition a chance to incorrectly cancel it.
        assertTrue(nm.activeNotifications.any {it.tag==SetupNotifications.REQUEST})
        scenario!!.moveToState(androidx.lifecycle.Lifecycle.State.RESUMED)
        waitFor("MJStanton-Desktop wants to link your devices")
        withTimeout(5000) {while(nm.activeNotifications.any {it.tag==SetupNotifications.REQUEST})delay(100)}
    }
    @Test(timeout=90000) fun joinedSnackbarIsAcknowledgedOnDisplayAndNeverRepeatsAfterRecreationOrBackground()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');bind(mac,"MacBook Air",'c');linkedWith(pc,mac)
        launch();waitFor("MacBook Air")
        val notice=SetupNotice("shown-once",mac,"MacBook Air",pc,"MJStanton-Desktop")
        setup {peers,state->peers to state.copy(notices=listOf(notice))}
        waitFor("MacBook Air joined · Added via MJStanton-Desktop")
        withTimeout(5000) {app.prefs.first {it.membership.notices.none {n->n.id==notice.id}}}
        assertTrue(notice.id in c.setupUi.presentedNotices.value)
        scenario!!.moveToState(androidx.lifecycle.Lifecycle.State.STARTED)
        delay(500)
        assertTrue(c.getSystemService(NotificationManager::class.java).activeNotifications.none {it.tag==SetupNotifications.joinedTag(notice.id)})
        scenario!!.moveToState(androidx.lifecycle.Lifecycle.State.RESUMED);scenario!!.recreate()
        waitFor("MacBook Air")
        compose.onNodeWithText("MacBook Air joined · Added via MJStanton-Desktop").assertDoesNotExist()
    }
    @Test(timeout=90000) fun removingLastComputerKeepsRosterOnlyPhonesOnHome()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');linkedWith(pc)
        val phone=SetupMember("d".repeat(32),"d".repeat(64),"phone","Other phone","member","1",pc)
        setup {peers,state->peers to state.copy(roster=state.roster+phone)}
        launch();openDevice("MJStanton-Desktop")
        compose.onNodeWithText("Remove from all devices").performScrollTo().performClick()
        waitFor("Remove MJStanton-Desktop from all devices?")
        compose.onNodeWithText("Remove").performClick()
        waitFor("Other phone")
        compose.onNodeWithText("Phones don't connect to each other").assertIsDisplayed()
        scenario!!.recreate();waitFor("Other phone")
        compose.onNodeWithText("Phones don't connect to each other").assertIsDisplayed()
        compose.onNodeWithText("Add device").performScrollTo().performClick()
        waitFor("Scan the QR code in Pneu on your computer")
        compose.onNodeWithContentDescription("Back").performClick()
        waitFor("Other phone")
    }
    private suspend fun linkedWith(vararg ids:String) {
        val me=self()
        setup {peers,state->
            val scoped=peers.map {it.copy(setupDecision=SetupScope.ENABLED,setupSupported=true)}
            scoped to state.copy(setupId="f".repeat(32),generation=state.generation+1,roster=listOf(me)+scoped.filter {it.deviceId in ids}.map(::member))
        }
    }
    @Test(timeout=90000) fun directPairingClearsAddedViaFromTheDevicePage()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');bind(mac,"MacBook Air",'c')
        linkedWith(pc,mac)
        setup {peers,state->
            val peer=peers.single {it.deviceId==mac}
            peers.map {if(it==peer)it.copy(addedViaId=pc,addedViaName="MJStanton-Desktop") else it} to
                state.copy(provenance=mapOf("${peer.deviceId}/${peer.pin}" to SetupProvenance(pc,"MJStanton-Desktop")))
        }
        launch();openDevice("MacBook Air")
        compose.onNodeWithText("Added via MJStanton-Desktop").performScrollTo().assertIsDisplayed()
        bind(mac,"MacBook Air",'c')
        compose.waitUntil(10000) {compose.onAllNodesWithText("Added via MJStanton-Desktop").fetchSemanticsNodes().isEmpty()}
        compose.onNodeWithText("Remove from all devices").performScrollTo().assertIsDisplayed()
        capture("flow-direct-pair-no-added-via")
        scenario!!.recreate()
        compose.onNodeWithText("Remove from all devices").performScrollTo().assertIsDisplayed()
        compose.onNodeWithText("Added via MJStanton-Desktop").assertDoesNotExist()
    }
    @Test(timeout=120000) fun removeFromAllDevicesAsksFirstThenRemovesHere()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');bind(mac,"MacBook Air",'c')
        linkedWith(pc,mac)
        launch()
        waitFor("MacBook Air")
        openDevice("MacBook Air")
        compose.onNodeWithText("Unpair").assertDoesNotExist()
        compose.onNodeWithText("Remove from all devices").performScrollTo().performClick()
        waitFor("Remove MacBook Air from all devices?")
        compose.onNodeWithText("It's removed from this phone now, and from your other devices when they next connect.").assertIsDisplayed()
        capture("flow-remove-confirm")
        compose.onNodeWithText("Cancel").performClick()
        assertTrue(app.preferences.read().devices.any {it.deviceId==mac})
        // Can't be saved: nothing changes and it says so.
        compose.onNodeWithText("Remove from all devices").performScrollTo().performClick()
        waitFor("Remove MacBook Air from all devices?")
        setup {peers,state->peers to state.copy(generation=state.generation+1)}
        scenario!!.recreate()
        waitFor("Remove MacBook Air from all devices?")
        compose.onNodeWithText("Remove").performClick()
        waitFor("Couldn't remove MacBook Air")
        scenario!!.recreate()
        waitFor("Couldn't remove MacBook Air")
        compose.onNodeWithText("Nothing was changed. Try again.").assertIsDisplayed()
        capture("flow-remove-failed")
        compose.onNodeWithText("OK").performClick()
        assertTrue(app.preferences.read().devices.any {it.deviceId==mac})
        // Now it works: the page closes, the row goes and Home says what's true.
        compose.onNodeWithText("Remove from all devices").performScrollTo().performClick()
        waitFor("Remove MacBook Air from all devices?")
        compose.onNodeWithText("Remove").performClick()
        waitFor("MacBook Air removed. Your other devices update when they connect.")
        capture("flow-removed-home")
        val p=app.preferences.read()
        assertTrue(p.devices.none {it.deviceId==mac});assertTrue(p.devices.any {it.deviceId==pc})
        assertFalse(p.membership.roster.first {it.deviceId==mac}.active)
        assertTrue(p.membership.barriers.keys.any {it.startsWith(mac)})
    }
    @Test(timeout=90000) fun joinedNotificationRemoveOpensTheConfirmation()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b');bind(mac,"MacBook Air",'c')
        linkedWith(pc,mac)
        setup {peers,state->peers.map {if(it.deviceId==mac)it.copy(addedViaId=pc,addedViaName="MJStanton-Desktop") else it} to state}
        val p=app.preferences.read()
        SetupNotifications.channel(c)
        SetupNotifications.postJoined(c,p,SetupNotice("n",mac,"MacBook Air",pc,"MJStanton-Desktop"))
        device.openNotification()
        assertTrue(device.wait(Until.hasObject(By.text("MacBook Air joined")),5000))
        val remove=device.wait(Until.findObject(By.text("Remove")),5000) ?: device.wait(Until.findObject(By.text("REMOVE")),2000)
        assertNotNull("Remove action",remove);remove!!.click()
        // A cold start from the notification: the page opens with its confirmation.
        waitFor("Remove MacBook Air from all devices?",timeout=30000)
        compose.onNodeWithText("Added via MJStanton-Desktop").assertExists()
        capture("flow-joined-remove")
        compose.onNodeWithText("Cancel").performClick()
        assertTrue(app.preferences.read().devices.any {it.deviceId==mac})
    }
    @Test(timeout=90000) fun theRemovedPhoneSaysSoOnce()=runBlocking<Unit> {
        bind(pc,"MJStanton-Desktop",'b')
        launch()
        waitFor("MJStanton-Desktop")
        // What the core records when one of your devices removes this phone.
        setup {peers,state->peers to state.copy(withdrawn=true,withdrawalRevision=state.withdrawalRevision+1)}
        waitFor("This phone was removed from your devices")
        capture("flow-removed-self")
        compose.onNodeWithText("OK").performClick()
        compose.waitUntil(5000) {compose.onAllNodesWithText("This phone was removed from your devices").fetchSemanticsNodes().isEmpty()}
        scenario?.close();launch()
        waitFor("MJStanton-Desktop")
        compose.onNodeWithText("This phone was removed from your devices").assertDoesNotExist()
    }
}
