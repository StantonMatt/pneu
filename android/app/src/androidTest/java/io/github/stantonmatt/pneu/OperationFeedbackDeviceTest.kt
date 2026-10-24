package io.github.stantonmatt.pneu

import android.Manifest
import android.app.Application
import android.content.*
import android.net.Uri
import androidx.activity.ComponentActivity
import androidx.activity.compose.LocalActivityResultRegistryOwner
import androidx.activity.result.*
import androidx.activity.result.contract.ActivityResultContract
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.core.app.ActivityOptionsCompat
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.ui.PneuTheme
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** UI owners observe failures, including operations outliving their Activity. */
@RunWith(AndroidJUnit4::class)
class OperationFeedbackDeviceTest {
    @get:Rule val compose=createAndroidComposeRule<ComponentActivity>()
    private val i=InstrumentationRegistry.getInstrumentation()
    private val c=i.targetContext
    private val app=c.graph
    private val a="a".repeat(32)
    private val b="b".repeat(32)
    private val provider=Uri.parse("content://io.github.stantonmatt.pneu.test.review")
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        for(permission in listOf(Manifest.permission.ACCESS_LOCAL_NETWORK,Manifest.permission.POST_NOTIFICATIONS))
            i.uiAutomation.grantRuntimePermission(c.packageName,permission)
        app.preferences.toggle("setup",true);app.preferences.toggle("screenshots",false);app.preferences.toggle("queue",true)
        bind(a)
    }
    @After fun cleanup()=runBlocking<Unit> {
        app.staging.beforePayloadWrite={}
        c.contentResolver.call(provider,"release",null,null)
        DeviceFixture.reset()
    }
    private suspend fun bind(id:String) {
        val raw="pneu://pair?v=1&name=Computer%20${id.first()}&fp=${id.first().toString().repeat(64)}&addr=127.0.0.1%3A1&token=${"c".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,ConfirmedBinding.from(app.preferences.read())))
        assertTrue(app.installBinding(Invitation.parse(raw),id))
        withTimeout(5000) {app.prefs.first {it.devices.any {peer->peer.deviceId==id}}}
    }
    private fun toast(action:()->Unit) {
        val expected=c.getString(R.string.toast_cant_read)
        val event=i.uiAutomation.executeAndWaitForEvent(action,{it.text.any {text->text.toString().contains(expected)}},10000)
        try {assertTrue(event.text.any {it.toString().contains(expected)})}finally {event.recycle()}
    }
    @Test(timeout=30000) fun unreadableDeviceFileReportsAsynchronousAdmissionFailure()=runBlocking<Unit> {
        val authority=CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),a)))
        lateinit var operation:ManualEntryViewModel
        toast {i.runOnMainSync {operation=sendFilesTo(compose.activity,authority,listOf(provider.buildUpon().appendPath("error").build()))}}
        val result=withTimeout(5000) {operation.result.filterNotNull().first()}
        assertEquals(c.getString(R.string.toast_cant_read),result.message)
        assertEquals(listOf<String?>(null),operation.completed);assertTrue(app.queue.all().isEmpty())
    }
    @Test(timeout=30000) fun quotaFailureStillReportsAfterPickerActivityRecreation()=runBlocking<Unit> {
        val authority=CapturedAuthority(requireNotNull(DeliveryBinding.from(app.preferences.read(),a)))
        val entered=CountDownLatch(1);val release=CountDownLatch(1)
        app.staging.beforePayloadWrite={entered.countDown();check(release.await(10,TimeUnit.SECONDS));throw LocalCapacityException()}
        lateinit var operation:ManualEntryViewModel
        try {
            i.runOnMainSync {operation=sendFilesTo(compose.activity,authority,listOf(provider.buildUpon().appendPath("image").build()))}
            assertTrue(entered.await(5,TimeUnit.SECONDS))
            compose.activityRule.scenario.recreate()
            toast {release.countDown()}
            withTimeout(5000) {operation.result.filterNotNull().first()}
            assertEquals(listOf<String?>(null),operation.completed);assertTrue(app.queue.all().isEmpty())
            assertTrue(app.preferences.read().localCapacity)
        }finally {release.countDown()}
    }
    @Test(timeout=30000) fun pickerLaunchFailureUsesTheExistingErrorDialog() {
        val registry=object:ActivityResultRegistry() {
            override fun <I,O> onLaunch(requestCode:Int,contract:ActivityResultContract<I,O>,input:I,options:ActivityOptionsCompat?) {
                throw ActivityNotFoundException("Synthetic picker unavailable")
            }
        }
        val owner=object:ActivityResultRegistryOwner {override val activityResultRegistry=registry}
        compose.setContent {CompositionLocalProvider(LocalActivityResultRegistryOwner provides owner) {PneuScreen(DEVICE+a)}}
        compose.onNodeWithText("Send files").performScrollTo().performClick()
        compose.onNodeWithText(c.getString(R.string.toast_cant_read)).assertIsDisplayed()
    }
    @Test(timeout=30000) fun rawUriHomeEligibilityMatchesSendAndBackgroundEntriesRemainAvailable()=runBlocking<Unit> {
        DeviceFixture.connectLocalPeer(app)
        val p=app.preferences.read()
        val clip=ClipData.newRawUri("image",provider.buildUpon().appendPath("image").build()).apply {
            description.extras=android.os.PersistableBundle().apply {putBoolean("com.android.systemui.SUPPRESS_CLIPBOARD_OVERLAY",true)}
        }
        assertTrue(clip.description.hasMimeType("text/uri-list"))
        assertTrue(ClipboardSendPolicy.available(c,clip,p,app.connection.devices.value))
        assertTrue("Notification and tile use recipient availability without background clipboard reads",app.connection.clipboardAvailable(p))
        assertTrue(TilePolicy.present(p,app.connection.state.value.connectionStatus,"",clipboard=true,clipboardAvailable=app.connection.clipboardAvailable(p)).available)
        compose.setContent {CompositionLocalProvider(LocalUiActions provides UiActions {message,_->fail(message)}) {
            val link by app.connection.state.collectAsState()
            PneuTheme {HomeScreen(p,link,emptyList(),Health(true,true,true,true),Modifier,{},{},{})}
        }}
        compose.runOnUiThread {c.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)}
        compose.waitUntil(5000) {runCatching {compose.onNodeWithText("Send clipboard").assertIsEnabled()}.isSuccess}
        val operation=ManualEntryViewModel(c.applicationContext as Application)
        operation.captureClipboardAdmission(app.captureAdmission())
        i.runOnMainSync {operation.clipboard(clip)}
        val result=withTimeout(10000) {operation.result.filterNotNull().first()}
        assertNull(result.message);assertEquals(1,operation.completed.size)
        assertEquals("clipboard_image",app.queue.find(operation.completed.single()).kind)
    }
    private fun show(failures:MutableList<String>,content:@Composable ()->Unit) {
        compose.setContent {CompositionLocalProvider(LocalUiActions provides UiActions {message,_->failures+=message}) {PneuTheme {content()}}}
    }
    @Test(timeout=30000) fun renameFailureKeepsTheDialogAndPersistedName()=runBlocking<Unit> {
        val p=app.preferences.read();val failures=mutableListOf<String>()
        // A legacy invalid display name reaches the same action boundary as a
        // persistence exception; the dialog remains until the action succeeds.
        show(failures) {SettingsScreen(p.copy(selfName="x".repeat(129)),Health(true,true,true,true),0,{},{})}
        compose.onNodeWithText("x".repeat(129)).performScrollTo().performClick()
        compose.onNodeWithText("Save").performClick()
        compose.waitUntil(5000) {failures.isNotEmpty()}
        assertEquals(listOf(c.getString(R.string.ui_error_setting)),failures)
        compose.onNodeWithText("Rename this phone").assertIsDisplayed()
        assertEquals(p.selfName,app.preferences.read().selfName)
        compose.onNode(hasSetTextAction()).performTextReplacement("Review phone")
        compose.onNodeWithText("Save").performClick()
        compose.waitUntil(5000) {compose.onAllNodesWithText("Rename this phone").fetchSemanticsNodes().isEmpty()}
        assertEquals("Review phone",app.preferences.read().selfName)
    }
    private fun row(peer:String,state:String)=Outgoing().apply {
        mediaId="feedback";transferId=randomId();peerId=peer;this.state=state;kind="file";fileName="Feedback file"
        bindingGeneration=app.prefs.value.devices.first {it.deviceId==peer}.generation
    }
    @Test(timeout=30000) fun recentRowAndRecipientRetryAndStopSurfaceRejectedSnapshots()=runBlocking<Unit> {
        bind(b)
        val p=app.preferences.read();val failures=mutableListOf<String>()
        val outcomes=RecentProjection.items(listOf(row(a,"failed"),row(b,"waiting")),p,emptyMap()).associateBy {it.row.mediaId}
        val rows=outcomes.values.map {it.row}
        show(failures) {HomeScreen(p,LinkState(),rows,Health(true,true,true,true),Modifier,{},{},{},outcomes)}
        compose.onNodeWithContentDescription("Retry").performScrollTo().performClick()
        compose.waitUntil(5000) {failures.size==1}
        compose.onNodeWithContentDescription("Remove").performClick()
        compose.waitUntil(5000) {failures.size==2}
        compose.onNodeWithText("Feedback file").performClick()
        compose.onNode(hasContentDescription("Retry") and hasAnyAncestor(isDialog())).performClick()
        compose.waitUntil(5000) {failures.size==3}
        compose.onNodeWithContentDescription("Don't send").performClick()
        compose.waitUntil(5000) {failures.size==4}
        assertEquals(listOf(R.string.ui_error_retry_item,R.string.ui_error_remove_item,R.string.ui_error_retry_item,R.string.ui_error_remove_item).map(c::getString),failures)
        assertTrue(app.queue.all().isEmpty())
    }
    @Test(timeout=30000) fun recentMissingOriginalAndReceivedOpenReportFailure()=runBlocking<Unit> {
        val p=app.preferences.read();val failures=mutableListOf<String>()
        val original=row(a,"sent").apply {automatic=true;uri="content://media/external_primary/images/media/9223372036854775807"}
        show(failures) {HomeScreen(p,LinkState(),listOf(original),Health(true,true,true,true),Modifier,{},{},{})}
        compose.onNodeWithText("Feedback file").performScrollTo().performClick()
        compose.waitUntil(5000) {failures.isNotEmpty()}
        assertEquals(listOf(c.getString(R.string.toast_cant_read)),failures)
        val missing=ReceivedFile().apply {peerId=a;transferId=randomId();generation=1;catalogVersion="stale";uri=original.uri;metadata="{}"}
        toast {c.startActivity(ReceivedFileActions.intent(c,missing).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))}
    }
}
