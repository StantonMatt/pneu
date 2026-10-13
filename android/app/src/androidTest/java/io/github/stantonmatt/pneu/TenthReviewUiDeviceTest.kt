package io.github.stantonmatt.pneu

import android.Manifest
import android.content.Intent
import androidx.activity.ComponentActivity
import androidx.compose.runtime.*
import androidx.compose.foundation.layout.*
import androidx.compose.material3.Text
import androidx.compose.material3.IconButton
import androidx.compose.ui.Modifier
import androidx.compose.foundation.background
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.input.InputMode
import androidx.compose.ui.input.InputModeManager
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalInputModeManager
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.dp
import androidx.compose.ui.test.*
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import io.github.stantonmatt.pneu.ui.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.junit4.StateRestorationTester
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.Invitation
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class TenthReviewUiDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private fun content(body:@Composable ()->Unit) {
        compose.setContent {
            CompositionLocalProvider(LocalUiActions provides UiActions {_,_->fail("Unexpected UI failure")}) {body()}
        }
    }
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        DeviceFixture.stopPneuService();app.connection.stop();app.initialized.await();delay(200);app.forgetBinding()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.CAMERA)
    }
    @After fun cleanup()=runBlocking<Unit> {DeviceFixture.stopPneuService();app.connection.stop();app.forgetBinding()}
    @Test fun retainedDialogSurvivesConfirmErrorRetryAndCancel() {
        var title by mutableStateOf("Pair with First PC?")
        var candidate by mutableStateOf(true);var error by mutableStateOf(false);var attempts=0
        content {PneuTheme {
            if(candidate)PneuDialog(onDismissRequest={candidate=false},title={Text(title)},
                confirmButton={PneuTextButton({attempts++;error=true}){Text("Pair")}},
                dismissButton={PneuTextButton({candidate=false}){Text("Cancel")}})
            if(error)PneuDialog(onDismissRequest={error=false},title={Text("Couldn't start pairing")},
                confirmButton={PneuTextButton({error=false;title="Pair with Current PC?"}){Text("Try again")}})
        }}
        compose.onNodeWithText("Pair").performClick()
        compose.onNodeWithText("Couldn't start pairing").assertIsDisplayed()
        compose.onNodeWithText("Try again").performClick()
        compose.onNodeWithText("Pair with Current PC?").assertIsDisplayed()
        compose.onNodeWithText("Pair").performClick()
        assertEquals(2,attempts)
        compose.onNodeWithText("Try again").performClick()
        compose.onNodeWithText("Cancel").performClick()
        compose.onNodeWithText("Pair with Current PC?").assertDoesNotExist()
    }
    @Test fun permissionPromptActionThatRetainsDialogLeavesItsButtonsUsable() {
        var visible by mutableStateOf(true);var requests=0
        content {PneuTheme {if(visible)PneuDialog(onDismissRequest={visible=false},title={Text("Pair with PC?")},
            confirmButton={PneuTextButton({requests++}){Text("Pair")}},
            dismissButton={PneuTextButton({visible=false}){Text("Cancel")}})}}
        compose.onNodeWithText("Pair").performClick()
        compose.onNodeWithText("Pair with PC?").assertIsDisplayed()
        compose.onNodeWithText("Pair").performClick()
        assertEquals(2,requests)
        compose.onNodeWithText("Cancel").performClick()
        compose.onNodeWithText("Pair with PC?").assertDoesNotExist()
    }
    @Test fun shortDialogKeepsBothActionsVisibleAndScrollsCopyAtLargeFonts() {
        var fontScale by mutableFloatStateOf(1f);var retries=0;var cancels=0
        content {PneuTheme {
            val density=LocalDensity.current
            CompositionLocalProvider(LocalDensity provides Density(density.density,fontScale)) {
                Box(Modifier.size(320.dp,250.dp).testTag("short-dialog")) {
                    PneuDialogContent(pairingHeader=true,title={Text("Pair with a PC with a very long name?")},
                        text={Column {Text("Guidance that must remain available. ".repeat(30));Text("End of guidance")}},
                        confirmButton={PneuTextButton({retries++}) {Text("Try again")}},
                        dismissButton={PneuTextButton({cancels++}) {Text("Cancel")}})
                }
            }
        }}
        for(scale in listOf(1f,2f)) {
            compose.runOnIdle {fontScale=scale}
            val bounds=compose.onNodeWithTag("short-dialog").fetchSemanticsNode().boundsInRoot
            for(label in listOf("Cancel","Try again")) {
                val action=compose.onNodeWithText(label).assertIsDisplayed().fetchSemanticsNode().boundsInRoot
                assertTrue("$label must fit at font scale $scale",action.top>=bounds.top && action.bottom<=bounds.bottom)
                compose.onNodeWithText(label).performClick()
            }
            compose.onNodeWithText("End of guidance").performScrollTo().assertIsDisplayed()
        }
        assertEquals(2,retries);assertEquals(2,cancels)
    }
    @Test fun nonDismissibleDialogKeepsRecoveryActionsWithOverflowingCopy() {
        var visible by mutableStateOf(true);var dismisses=0
        content {PneuTheme {
            val density=LocalDensity.current
            CompositionLocalProvider(LocalDensity provides Density(density.density,2f)) {
                if(visible)PneuDialog(dismissible=false,onDismissRequest={dismisses++},
                    title={Text("Can't reach the PC with a very long name. ".repeat(30))},
                    text={Column {Text("Recovery guidance. ".repeat(60));Text("End of recovery")}},
                    confirmButton={PneuTextButton({visible=false}) {Text("Try again")}},
                    dismissButton={PneuTextButton({visible=false}) {Text("Cancel")}})
            }
        }}
        compose.onNodeWithText("Cancel").assertIsDisplayed()
        compose.onNodeWithText("Try again").assertIsDisplayed()
        compose.onNodeWithText("End of recovery").performScrollTo().assertIsDisplayed()
        compose.onNodeWithText("Try again").performClick()
        compose.onNodeWithText("Try again").assertDoesNotExist()
        assertEquals(0,dismisses)
    }
    @Test(timeout=20000) fun completedApprovedPairingDoesNotPoisonLaterScannerEntry()=runBlocking<Unit> {
        app.preferences.toggle("setup",true)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val invitation=invitation()
        var candidate by mutableStateOf<String?>(invitationUri(invitation))
        var requested by mutableStateOf<String?>(null)
        content {PneuScreen(requested=requested,consume={requested=null},candidate=candidate,offerPair={candidate=it})}
        compose.onNodeWithText("Pair").performClick()
        compose.waitUntil(5000) {app.prefs.value.pending==invitation}
        assertTrue(app.installBinding(invitation,"f".repeat(32)))
        compose.waitUntil(5000) {compose.onAllNodesWithContentDescription("Settings").fetchSemanticsNodes().isNotEmpty()}
        compose.runOnIdle {requested="pair"}
        compose.waitUntil(5000) {compose.onAllNodesWithText("Scan the QR code on your PC").fetchSemanticsNodes().isNotEmpty()}
        // The old floor would report the previous install as a fresh success
        // and navigate Home immediately (or after the 800 ms success delay).
        compose.mainClock.advanceTimeBy(1200)
        compose.onNodeWithText("Scan the QR code on your PC").assertIsDisplayed()
        compose.onNodeWithContentDescription("Settings").assertDoesNotExist()
        // Re-enter while AnimatedContent still retains the leaving scanner.
        compose.mainClock.autoAdvance=false
        compose.onNodeWithContentDescription("Back").performClick()
        compose.runOnIdle {requested="pair"}
        compose.mainClock.advanceTimeBy(1200)
        compose.onNodeWithText("Scan the QR code on your PC").assertIsDisplayed()
        compose.onNodeWithContentDescription("Settings").assertDoesNotExist()
        compose.mainClock.autoAdvance=true
        compose.onNodeWithContentDescription("Back").performClick()
        compose.waitUntil(5000) {compose.onAllNodesWithContentDescription("Settings").fetchSemanticsNodes().isNotEmpty()}
    }
    @Test fun customControlsShowAnAccentRingOnlyForKeyboardFocus() {
        val tags=listOf("primary","neutral","click","toggle","switch")
        val requesters=tags.associateWith {FocusRequester()}
        var input:InputModeManager?=null;var clicks=0;var checked by mutableStateOf(false)
        content {PneuTheme {
            input=LocalInputModeManager.current
            Column(verticalArrangement=Arrangement.spacedBy(8.dp)) {
                PneuButton({clicks++},Modifier.testTag("primary").focusRequester(requesters.getValue("primary"))) {Text("Pair")}
                PneuNeutralButton({clicks++},Modifier.testTag("neutral").focusRequester(requesters.getValue("neutral"))) {Text("Cancel")}
                Box(Modifier.size(180.dp,48.dp).testTag("click").focusRequester(requesters.getValue("click")).background(Color.White).pneuClick {clicks++})
                Box(Modifier.size(180.dp,48.dp).testTag("toggle").focusRequester(requesters.getValue("toggle")).background(Color.White).pneuToggle(checked,true) {checked=it})
                PneuSwitch(checked,{checked=it},modifier=Modifier.testTag("switch").focusRequester(requesters.getValue("switch")))
                PneuButton({fail("Disabled button activated")},Modifier.testTag("disabled"),enabled=false) {Text("Disabled")}
            }
        }}
        for(tag in tags) {
            val node=compose.onNodeWithTag(tag)
            // Compose's injected MotionEvent bypasses ViewRoot's global mode
            // transition after keyboard input. Establish it explicitly first.
            i.setInTouchMode(true)
            compose.onNodeWithTag("disabled").performTouchInput {click()}
            compose.waitUntil(5000) {input?.inputMode==InputMode.Touch}
            compose.runOnIdle {assertEquals(InputMode.Touch,requireNotNull(input).inputMode)}
            val touch=node.captureToImage().toPixelMap()
            i.sendKeyDownUpSync(android.view.KeyEvent.KEYCODE_TAB)
            compose.waitUntil(5000) {input?.inputMode==InputMode.Keyboard}
            compose.runOnIdle {
                assertEquals(InputMode.Keyboard,requireNotNull(input).inputMode)
                requesters.getValue(tag).requestFocus()
            }
            node.assertIsFocused()
            val focused=node.captureToImage().toPixelMap()
            // The 2 dp stroke runs along the top, inset 2 dp. Check pixels,
            // including primary/checked controls where the halo must contrast.
            val x=focused.width/2
            var changed=false;var accent=false
            for(y in 0 until minOf(focused.height,12)) {
                if(focused[x,y]!=touch[x,y])changed=true
                if(focused[x,y]==Color(0xFF4F5BFF))accent=true
            }
            assertTrue("$tag must visibly indicate keyboard focus",changed && accent)
            node.performKeyInput {pressKey(Key.Enter)}
            i.setInTouchMode(true)
            compose.onNodeWithTag("disabled").performTouchInput {click()}
            compose.waitUntil(5000) {input?.inputMode==InputMode.Touch}
            compose.runOnIdle {assertEquals(InputMode.Touch,requireNotNull(input).inputMode)}
            val afterTouch=node.captureToImage().toPixelMap()
            // Neutral/click/toggle have no stateful fill changes after Enter.
            if(tag in listOf("neutral","click","toggle"))for(y in 0 until minOf(touch.height,12))
                assertEquals("$tag ring must disappear in touch mode",touch[x,y],afterTouch[x,y])
        }
        assertEquals(3,clicks)
        assertFalse(checked)
        // A disabled clickable may have no focus target (and no Focused key).
        // assertIsNotFocused requires that key to exist with a false value.
        compose.onNodeWithTag("disabled").assertIsNotEnabled().assert(SemanticsMatcher("Does not hold keyboard focus") {
            it.config.getOrNull(SemanticsProperties.Focused)!=true
        })
        i.sendKeyDownUpSync(android.view.KeyEvent.KEYCODE_TAB)
        compose.waitUntil(5000) {input?.inputMode==InputMode.Keyboard}
        compose.runOnIdle {
            assertEquals(InputMode.Keyboard,requireNotNull(input).inputMode)
            requesters.getValue("primary").requestFocus()
        }
        // Real traversal follows the same focus targets; no extra focusable
        // drawing layer may intercept Tab or leave a disabled target in order.
        for((from,to) in tags.zipWithNext()) {
            compose.onNodeWithTag(from).performKeyInput {pressKey(Key.Tab)}
            compose.onNodeWithTag(to).assertIsFocused()
        }
    }
    @Test fun disabledControlsAreSkippedByKeyboardAndDpadAndLoseFocusWhenDisabled() {
        val tags=listOf("button","click","toggle","switch")
        val requesters=tags.associateWith {FocusRequester()}
        var enabled by mutableStateOf(true);var input:InputModeManager?=null
        content {PneuTheme {
            input=LocalInputModeManager.current
            Column {
                PneuButton({},Modifier.testTag("start")) {Text("Start")}
                PneuButton({fail("Disabled button activated")},Modifier.testTag("button").focusRequester(requesters.getValue("button")),enabled=enabled) {Text("Button")}
                Box(Modifier.size(180.dp,48.dp).testTag("click").focusRequester(requesters.getValue("click")).pneuClick(enabled) {})
                Box(Modifier.size(180.dp,48.dp).testTag("toggle").focusRequester(requesters.getValue("toggle")).pneuToggle(false,enabled) {})
                PneuSwitch(false,{},enabled,Modifier.testTag("switch").focusRequester(requesters.getValue("switch")))
                PneuButton({},Modifier.testTag("end")) {Text("End")}
            }
        }}
        i.sendKeyDownUpSync(android.view.KeyEvent.KEYCODE_TAB)
        compose.waitUntil(5000) {input?.inputMode==InputMode.Keyboard}
        val notFocused=SemanticsMatcher("Does not hold keyboard focus") {it.config.getOrNull(SemanticsProperties.Focused)!=true}
        for(tag in tags) {
            compose.runOnIdle {enabled=true}
            compose.runOnIdle {requesters.getValue(tag).requestFocus()}
            compose.onNodeWithTag(tag).assertIsFocused()
            compose.runOnIdle {enabled=false}
            compose.onNodeWithTag(tag).assertIsNotEnabled().assert(notFocused)
            // Traverse the disabled group in both directions with both input
            // methods. Only the enabled endpoints may receive focus.
            compose.onNodeWithTag("start").performSemanticsAction(androidx.compose.ui.semantics.SemanticsActions.RequestFocus) {it()}
            compose.onNodeWithTag("start").performKeyInput {pressKey(Key.Tab)}
            compose.onNodeWithTag("end").assertIsFocused()
            compose.onNodeWithTag("end").performKeyInput {keyDown(Key.ShiftLeft);pressKey(Key.Tab);keyUp(Key.ShiftLeft)}
            compose.onNodeWithTag("start").assertIsFocused()
            compose.onNodeWithTag("start").performKeyInput {pressKey(Key.DirectionDown)}
            compose.onNodeWithTag("end").assertIsFocused()
            compose.onNodeWithTag("end").performKeyInput {pressKey(Key.DirectionUp)}
            compose.onNodeWithTag("start").assertIsFocused()
            for(disabled in tags)compose.onNodeWithTag(disabled).assert(notFocused)
        }
    }
    @Test fun disabledContainerRowsKeepEnabledChildActionsInKeyboardAndDpadOrder() {
        var toggleParent by mutableStateOf(false)
        val start=FocusRequester();var input:InputModeManager?=null;var retries=0;var removes=0
        content {PneuTheme {
            input=LocalInputModeManager.current
            Column {
                PneuButton({},Modifier.testTag("start").focusRequester(start)) {Text("Start")}
                val parent=if(toggleParent)Modifier.pneuToggle(false,false) {fail("Disabled container toggled")}
                    else Modifier.pneuClick(false) {fail("Disabled container clicked")}
                Row(Modifier.fillMaxWidth().testTag("parent").then(parent)) {
                    IconButton({retries++},Modifier.testTag("retry")) {Text("Retry")}
                    IconButton({removes++},Modifier.testTag("remove")) {Text("Remove")}
                }
                PneuButton({fail("Disabled standalone control activated")},Modifier.testTag("disabled-alone"),enabled=false) {Text("Disabled")}
                PneuButton({},Modifier.testTag("end")) {Text("End")}
            }
        }}
        i.sendKeyDownUpSync(android.view.KeyEvent.KEYCODE_TAB)
        compose.waitUntil(5000) {input?.inputMode==InputMode.Keyboard}
        val notFocused=SemanticsMatcher("Does not hold keyboard focus") {it.config.getOrNull(SemanticsProperties.Focused)!=true}
        for(toggle in listOf(false,true)) {
            compose.runOnIdle {toggleParent=toggle}
            compose.onNodeWithTag("parent").assertIsNotEnabled().assert(notFocused)
            compose.onNodeWithTag("disabled-alone").assertIsNotEnabled().assert(notFocused)
            compose.runOnIdle {start.requestFocus()}
            val order=listOf("start","retry","remove","end")
            for((from,to) in order.zipWithNext()) {
                compose.onNodeWithTag(from).performKeyInput {pressKey(Key.Tab)}
                compose.onNodeWithTag(to).assertIsFocused().assertIsEnabled()
            }
            for((from,to) in order.reversed().zipWithNext()) {
                compose.onNodeWithTag(from).performKeyInput {keyDown(Key.ShiftLeft);pressKey(Key.Tab);keyUp(Key.ShiftLeft)}
                compose.onNodeWithTag(to).assertIsFocused()
            }
            compose.onNodeWithTag("start").performKeyInput {pressKey(Key.DirectionDown)}
            compose.onNodeWithTag("retry").assertIsFocused().performKeyInput {pressKey(Key.Enter);pressKey(Key.DirectionRight)}
            compose.onNodeWithTag("remove").assertIsFocused().performKeyInput {pressKey(Key.Enter);pressKey(Key.DirectionDown)}
            compose.onNodeWithTag("end").assertIsFocused().performKeyInput {pressKey(Key.DirectionUp)}
            compose.onNodeWithTag("retry").assertIsFocused()
            compose.onNodeWithTag("parent").assert(notFocused)
            compose.onNodeWithTag("disabled-alone").assert(notFocused)
        }
        assertEquals(2,retries);assertEquals(2,removes)
    }
    @Test(timeout=20000) fun rejectedInvitationAndRecoveryDialogSurviveStateRestorationBeforeRetry()=runBlocking<Unit> {
        app.preferences.toggle("setup",true)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val invitation=invitation();val raw=invitationUri(invitation)
        var candidate by mutableStateOf<String?>(raw)
        val restore=StateRestorationTester(compose)
        restore.setContent {PneuScreen(candidate=candidate,offerPair={candidate=it})}
        compose.onNodeWithText("Pair with Twelfth?").assertIsDisplayed()
        compose.onNodeWithText("Pair").performClick()
        compose.waitUntil(5000) {app.prefs.value.pending==invitation}
        DeviceFixture.stopPneuService();app.connection.stop();app.connection.awaitStopped()
        assertTrue(app.cancelPairing(invitation))
        app.connection.state.value=LinkState(pairing=c.getString(R.string.pair_unreachable_title))
        compose.onNodeWithText("Can't reach your PC").assertIsDisplayed()
        compose.onNodeWithText("Try again").assertIsDisplayed()
        restore.emulateSavedInstanceStateRestore()
        compose.onNodeWithText("Can't reach your PC").assertIsDisplayed()
        compose.onNodeWithText("Try again").performClick()
        compose.onNodeWithText("Pair with Twelfth?").assertIsDisplayed()
        assertNull(app.preferences.read().pending);assertNull(app.preferences.read().peer)
        // Restoring the confirmation must not replay the consumed failure or
        // silently approve another pairing attempt.
        restore.emulateSavedInstanceStateRestore()
        compose.onNodeWithText("Pair with Twelfth?").assertIsDisplayed()
        compose.onNodeWithText("Can't reach your PC").assertDoesNotExist()
        assertNull(app.preferences.read().pending)
        compose.onNodeWithText("Cancel").performClick()
    }
    @Test fun scannerInstructionFitsAShortWindowAfterWrapping() {
        val instruction="Your PC's clock says this code expired. Check both clocks and scan the new code."
        content {PneuTheme {Box(Modifier.size(320.dp,220.dp).testTag("scanner-window")) {ScannerFrame(true,false,instruction)}}}
        compose.onNodeWithText(instruction).assertIsDisplayed()
        val window=compose.onNodeWithTag("scanner-window").fetchSemanticsNode().boundsInRoot
        val text=compose.onNodeWithText(instruction).fetchSemanticsNode().boundsInRoot
        assertTrue("Instruction must stay inside the short window",text.top>=window.top && text.bottom<=window.bottom && text.left>=window.left && text.right<=window.right)
    }
    @Test(timeout=20000) fun malformedLinksFromHomeAndSettingsShowScannerFeedback()=runBlocking<Unit> {
        app.preferences.toggle("setup",true)
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        val invitation=invitation()
        assertTrue(app.requestPairing(invitationUri(invitation),null))
        assertTrue(app.installBinding(invitation,"f".repeat(32)))
        var candidate by mutableStateOf<String?>(null)
        content {PneuScreen(candidate=candidate,offerPair={candidate=it})}
        for(fromSettings in listOf(false,true)) {
            compose.waitUntil(5000) {compose.onAllNodesWithContentDescription("Settings").fetchSemanticsNodes().isNotEmpty()}
            if(fromSettings)compose.onNodeWithContentDescription("Settings").performClick()
            compose.runOnIdle {candidate="pneu://pair?v=1"}
            compose.waitUntil(5000) {compose.onAllNodesWithText("That isn't a Pneu code").fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText("That isn't a Pneu code").assertIsDisplayed()
            compose.onNodeWithContentDescription("Back").performClick()
        }
        assertNotNull(app.preferences.read().peer)
        assertNull(app.preferences.read().pending)
    }
    @Test(timeout=20000) fun malformedExpiredMalformedAndRepeatedExpiryEachShowFeedback()=runBlocking<Unit> {
        app.preferences.toggle("setup",true)
        var candidate by mutableStateOf<String?>(null)
        content {PneuScreen(candidate=candidate,offerPair={candidate=it})}
        compose.onNodeWithText("Scan the QR code on your PC").assertIsDisplayed()
        fun shownThenGone(message:String) {
            compose.waitUntil(5000) {compose.onAllNodesWithText(message).fetchSemanticsNodes().isNotEmpty()}
            compose.onNodeWithText(message).assertIsDisplayed()
            compose.mainClock.advanceTimeBy(6000)
            compose.waitUntil(5000) {compose.onAllNodesWithText(message).fetchSemanticsNodes().isEmpty()}
        }
        val malformed="pneu://pair?v=1"
        val expired=invitationUri(invitation().copy(expires=System.currentTimeMillis()-1000))
        val notPneu=c.getString(R.string.pair_not_pneu);val expiredCopy=c.getString(R.string.pair_expired)
        compose.runOnIdle {candidate=malformed}
        shownThenGone(notPneu)
        // The same validation used by an approved invitation, without a
        // transport or peer that could overwrite the error during this test.
        assertFalse(beginPair(c,expired))
        shownThenGone(expiredCopy)
        compose.runOnIdle {candidate=malformed}
        shownThenGone(notPneu)
        assertFalse(beginPair(c,expired))
        shownThenGone(expiredCopy)
        // The connection StateFlow must also publish two identical rejections.
        assertFalse(beginPair(c,expired))
        shownThenGone(expiredCopy)
        assertNull(app.preferences.read().pending)
        assertNull(app.preferences.read().peer)
    }
    @Test fun scannerKeepsItsPaneTitleWithoutDrawingTheOldHeading()=runBlocking<Unit> {
        val prefs=app.preferences.read()
        content {PairScreen(prefs,LinkState(),{},{},{},cameraPreview={_,_->})}
        compose.onNode(SemanticsMatcher.expectValue(SemanticsProperties.PaneTitle,"Pair with PC")).assertExists()
        compose.onNodeWithText("Pair with PC").assertDoesNotExist()
        compose.onNodeWithText("Scan the QR code on your PC").assertIsDisplayed()
        compose.onNodeWithContentDescription("Back").assertHasClickAction()
    }
    @Test fun customSwitchKeepsItsAccessibleStateAndInteraction() {
        var checked by mutableStateOf(false)
        content {PneuTheme {PneuSwitch(checked,{checked=it})}}
        compose.onNode(isToggleable()).assertIsOff().performClick().assertIsOn()
        assertTrue(checked)
    }
    @Test(timeout=20000) fun repeatedCameraResultsCannotReadmitInvitationDuringSuccessTransition()=runBlocking<Unit> {
        val raw="pneu://pair?v=1&name=Tenth&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        val invitation=Invitation.parse(raw)
        assertTrue(app.requestPairing(raw,null))
        var prefs by mutableStateOf(app.preferences.read());var link by mutableStateOf(LinkState(status="Connecting…",pairing="Pairing…"))
        var scan:((String)->Unit)?=null;var admitted=0;var home=0;var scannerSession by mutableIntStateOf(0)
        compose.mainClock.autoAdvance=false
        content {
            CompositionLocalProvider(LocalUiActions provides UiActions {_,_->fail("Unexpected UI failure")}) {
                key(scannerSession) {PairScreen(prefs,link,{home++},{},{admitted++},cameraPreview={_,onCode->scan=onCode})}
            }
        }
        compose.waitForIdle();compose.runOnIdle {requireNotNull(scan)(raw)};assertEquals(0,admitted)
        assertTrue(app.installBinding(invitation,"d".repeat(32)))
        val paired=app.preferences.read()
        // DataStore can publish the binding before either READY or its marker.
        compose.runOnIdle {prefs=paired}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.runOnIdle {requireNotNull(scan)(raw)};assertEquals(0,admitted)
        compose.runOnIdle {link=LinkState(status="Connecting…")}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.runOnIdle {requireNotNull(scan)(raw)};assertEquals(0,admitted)
        compose.runOnIdle {link=LinkState(status="Connected",pairing="Paired with Tenth",pairingPin=invitation.fingerprint)}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        // Both the cleared pending state and changed peer pin reset the old guard.
        compose.runOnIdle {repeat(5) {requireNotNull(scan)(raw)}}
        assertEquals(0,admitted);assertEquals(0,home)
        compose.mainClock.advanceTimeBy(850);compose.waitForIdle();compose.runOnIdle {assertEquals(1,home)}
        // Reopening Pair after Home starts a fresh scanner even if the old
        // session still carries its success marker.
        compose.runOnIdle {scannerSession++}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.runOnIdle {requireNotNull(scan)(raw)};assertEquals(1,admitted)
    }
    @Test(timeout=15000) fun retryOffersTheConfirmedAttemptInsteadOfANewerCameraRead()=runBlocking<Unit> {
        val raw=invitationUri(invitation())
        val scanned=raw.replace("name=Twelfth","name=NewScan")
        val p=app.preferences.read();var link by mutableStateOf(LinkState(status="PC unreachable"))
        var scan:((String)->Unit)?=null;val offered=mutableListOf<String>()
        content {PairScreen(p,link,{},{},{offered.add(it)},lastAttemptedInvitation=raw,cameraPreview={_,code->scan=code})}
        compose.waitForIdle();compose.runOnIdle {requireNotNull(scan)(scanned)}
        assertEquals(listOf(scanned),offered)
        compose.runOnIdle {link=LinkState(status="PC unreachable",pairing="Can't reach your PC")}
        compose.waitUntil(5000) {compose.onAllNodesWithText("Try again").fetchSemanticsNodes().isNotEmpty()}
        compose.onNodeWithText("Try again").performClick()
        compose.waitUntil(5000) {offered.size==2}
        assertEquals(listOf(scanned,raw),offered);assertNull(app.preferences.read().pending)
    }
    private fun invitationUri(invitation:Invitation)="pneu://pair?v=1&name=${invitation.name}&fp=${invitation.fingerprint}&addr=127.0.0.1%3A1&token=${invitation.token}&exp=${invitation.expires}"
    private fun invitation()=Invitation.parse("pneu://pair?v=1&name=Twelfth&fp=${"c".repeat(64)}&addr=127.0.0.1%3A1&token=${"e".repeat(64)}&exp=${System.currentTimeMillis()+300000}")
    private suspend fun disconnectDuringTransition(at:Long) {
        val invitation=invitation()
        assertTrue(app.requestPairing(invitationUri(invitation),null))
        var prefs by mutableStateOf(app.preferences.read());var link by mutableStateOf(LinkState(status="Connecting…"))
        var home=0;var admitted=0;var scan:((String)->Unit)?=null
        compose.mainClock.autoAdvance=false
        content {PairScreen(prefs,link,{home++},{},{admitted++},cameraPreview={_,code->scan=code})}
        compose.waitForIdle()
        assertTrue(app.installBinding(invitation,"f".repeat(32)))
        compose.runOnIdle {prefs=app.prefs.value;link=LinkState(status="Connected",pairing="Paired with Twelfth",pairingPin=invitation.fingerprint)}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.mainClock.advanceTimeBy(at);compose.waitForIdle()
        compose.runOnIdle {assertEquals(0,home);link=LinkState(status="PC unreachable")}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        compose.runOnIdle {assertEquals(1,home);requireNotNull(scan)(invitationUri(invitation))}
        assertEquals(0,admitted)
        compose.mainClock.advanceTimeBy(1000);compose.waitForIdle();assertEquals(1,home)
    }
    @Test(timeout=15000) fun earlyDisconnectEndsDurablePairingTransition()=runBlocking<Unit> {disconnectDuringTransition(100)}
    @Test(timeout=15000) fun lateDisconnectEndsDurablePairingTransition()=runBlocking<Unit> {disconnectDuringTransition(650)}
    @Test(timeout=15000) fun fastPairingFromAnotherScreenRetainsTheApprovedRevisionFloor()=runBlocking<Unit> {
        val baseline=app.preferences.read().pairingRevision
        val invitation=invitation()
        assertTrue(app.requestPairing(invitationUri(invitation),null))
        assertTrue(app.installBinding(invitation,"f".repeat(32)))
        val prefs=app.preferences.read();var home=0
        content {PairScreen(prefs,LinkState(status="PC unreachable"),{home++},{},{},initialPairingRevision=baseline,cameraPreview={_,_->})}
        compose.waitUntil(5000) {home==1}
    }
    @Test(timeout=15000) fun durableInstallNavigatesWithoutPendingOrSuccessMarkerObserved()=runBlocking<Unit> {
        var prefs by mutableStateOf(app.preferences.read());var home=0
        compose.mainClock.autoAdvance=false
        content {PairScreen(prefs,LinkState(status="PC unreachable"),{home++},{},{},cameraPreview={_,_->})}
        compose.waitForIdle()
        val invitation=invitation();assertTrue(app.requestPairing(invitationUri(invitation),null));assertTrue(app.installBinding(invitation,"f".repeat(32)))
        compose.runOnIdle {prefs=app.prefs.value}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle();assertEquals(1,home)
    }
    @Test(timeout=15000) fun cancelledSamePcAttemptDoesNotNavigateAndSuccessfulRepairDoes()=runBlocking<Unit> {
        val invitation=invitation();assertTrue(app.requestPairing(invitationUri(invitation),null));assertTrue(app.installBinding(invitation,"f".repeat(32)))
        var prefs by mutableStateOf(app.preferences.read());var home=0;var scan:((String)->Unit)?=null;var admitted=0
        compose.mainClock.autoAdvance=false
        content {PairScreen(prefs,LinkState(status="PC unreachable"),{home++},{},{admitted++},cameraPreview={_,code->scan=code})}
        compose.waitForIdle()
        assertTrue(app.requestPairing(invitationUri(invitation),invitation.fingerprint));val pending=app.preferences.read();compose.runOnIdle {prefs=pending}
        compose.mainClock.advanceTimeByFrame();compose.waitForIdle()
        app.cancelPairing(invitation);val cancelled=app.preferences.read()
        compose.runOnIdle {prefs=cancelled};compose.mainClock.advanceTimeBy(1000);compose.waitForIdle()
        compose.runOnIdle {assertEquals(0,home);requireNotNull(scan)(invitationUri(invitation))};assertEquals(1,admitted)
        assertTrue(app.requestPairing(invitationUri(invitation),invitation.fingerprint));assertTrue(app.installBinding(invitation,"f".repeat(32)))
        compose.runOnIdle {prefs=app.prefs.value};compose.mainClock.advanceTimeByFrame();compose.waitForIdle();assertEquals(1,home)
    }

}
