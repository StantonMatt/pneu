package io.github.stantonmatt.pneu

import android.Manifest
import android.content.*
import androidx.datastore.preferences.core.edit
import io.github.stantonmatt.pneu.data.PeerRegistry
import android.content.pm.PackageManager
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import java.io.IOException
import java.io.File
import java.util.concurrent.atomic.*

@RunWith(AndroidJUnit4::class)
class SixthReviewStorageDeviceTest {
    private val i=InstrumentationRegistry.getInstrumentation();private val c=i.targetContext;private val app=c.graph
    private val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
    @Before fun setup()=runBlocking<Unit> {
        DeviceFixture.reset()
        UiDevice.getInstance(i).pressHome();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.initialized.await();delay(250);app.forgetBinding()
        i.uiAutomation.grantRuntimePermission(c.packageName,Manifest.permission.ACCESS_LOCAL_NETWORK)
        app.preferences.toggle("wifi",true);app.preferences.toggle("tailscale",true);app.preferences.toggle("mobile",true)
    }
    @After fun cleanup()=runBlocking<Unit> {c.stopService(Intent(c,PneuService::class.java));app.connection.stop();scope.cancel();app.preferences.beforeMutation={};app.forgetBinding()}
    private fun grantOperation(uri:Uri,operation:String) {
        c.startActivity(Intent().setComponent(ComponentName(c.packageName+".test",FixtureCallerActivity::class.java.name))
            .putExtra("uri",uri.toString()).putExtra("grantOperation",operation).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }
    @Test(timeout=25000) fun startupReleasesLegacyGrantsAndRetriesIdentityAndPendingResetWithoutRestart()=runBlocking<Unit> {
        val uri=Uri.parse("content://io.github.stantonmatt.pneu.test.sources/legacy-${randomId()}")
        grantOperation(uri,"legacy")
        withTimeout(5000) {while(c.checkUriPermission(uri,android.os.Process.myPid(),c.applicationInfo.uid,Intent.FLAG_GRANT_READ_URI_PERMISSION)!=PackageManager.PERMISSION_GRANTED)delay(20)}
        // Reproduce an older build's durable read/write grant, never production admission.
        c.contentResolver.takePersistableUriPermission(uri,Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        assertTrue(c.contentResolver.persistedUriPermissions.any {it.uri==uri && it.isReadPermission && it.isWritePermission})
        val raw="pneu://pair?v=1&name=Reset&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
        val peerId=app.preferences.read().peer!!.deviceId
        app.preferences.store.edit {PeerRegistry.reset(it,peerId)}
        val identityCalls=AtomicInteger();val resetCalls=AtomicInteger();val allowIdentity=AtomicBoolean(false);val allowReset=AtomicBoolean(false)
        val failingStore=object:androidx.datastore.core.DataStore<androidx.datastore.preferences.core.Preferences> {
            override val data=app.preferences.store.data
            override suspend fun updateData(transform:suspend (androidx.datastore.preferences.core.Preferences)->androidx.datastore.preferences.core.Preferences)=
                app.preferences.store.updateData {before->
                    val after=transform(before)
                    if(peerId in PeerRegistry.resets(before) && peerId !in PeerRegistry.resets(after)) {
                        resetCalls.incrementAndGet()
                        if(!allowReset.get())throw IOException("Synthetic final peer reset write failure")
                    }
                    after
                }
        }
        val preferences=Preferences(c,failingStore).apply {beforeMutation={key->
            if(key=="ensureIdentity") {identityCalls.incrementAndGet();if(!allowIdentity.get())throw IOException("Synthetic identity failure")}
        }}
        val secondary=AppGraph(c,preferences)
        try {
            withTimeout(5000) {while(identityCalls.get()==0)delay(20)}
            assertFalse(secondary.initialized.isCompleted)
            assertTrue(c.contentResolver.persistedUriPermissions.none {it.uri==uri})
            allowIdentity.set(true);withTimeout(6000) {secondary.initialized.await()}
            assertTrue(identityCalls.get()>=2);assertEquals(setOf(peerId),preferences.read().resetPeers)
            // Initialized is not poisoned by cleanup; the same manager retries even unpaired.
            secondary.connection.start(scope)
            withTimeout(6000) {while(resetCalls.get()<2)delay(20)}
            allowReset.set(true);secondary.connection.kick()
            withTimeout(6000) {preferences.flow.first {it.resetPeers.isEmpty()}}
            assertTrue(secondary.initialized.isCompleted && !secondary.initialized.isCancelled)
        } finally {
            allowIdentity.set(true);allowReset.set(true);secondary.connection.stop();secondary.connection.awaitStopped();secondary.scope.cancel();secondary.scope.coroutineContext[Job]!!.join();secondary.database.close()
            grantOperation(uri,"revoke")
        }
    }
    @Test(timeout=18000) fun lanToggleAndOwnerStopReleaseActualMulticastAndDiscoveryResources()=runBlocking<Unit> {
        val raw="pneu://pair?v=1&name=Discovery&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
        app.connection.start(scope)
        withTimeout(5000) {while(!app.connection.discoveryActive || !app.connection.multicastHeld)delay(20)}
        app.preferences.toggle("wifi",false)
        withTimeout(5000) {while(app.connection.discoveryActive || app.connection.multicastHeld)delay(20)}
        assertEquals(0,app.connection.discoveryWatchCount)
        assertTrue(app.preferences.flow.first().tailscale)
        app.preferences.toggle("wifi",true)
        withTimeout(5000) {while(!app.connection.discoveryActive || !app.connection.multicastHeld)delay(20)}
        app.connection.stop();assertFalse(app.connection.discoveryActive);assertFalse(app.connection.multicastHeld)
        app.preferences.toggle("paused",true);delay(250)
        assertFalse(app.connection.discoveryActive);assertFalse(app.connection.multicastHeld);assertEquals(0,app.connection.discoveryWatchCount)
    }
    @Test(timeout=12000) fun manualQueueAndHistoryKeepOnlyPrivateUrisEvenWhenPreviewFails()=runBlocking<Unit> {
        val raw="pneu://pair?v=1&name=Sources&fp=${"a".repeat(64)}&addr=127.0.0.1%3A1&token=${"b".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
        assertTrue(app.requestPairing(raw,null));assertTrue(app.installBinding(Invitation.parse(raw),"d".repeat(32)))
        val source=Uri.parse("content://io.github.stantonmatt.pneu.test.review/image")
        // A named offline send remains allowed with the default queue policy off.
        val manual=ManualEntries.file(app,source,"file",DeliveryBinding.from(app.preferences.read()));assertEquals("",manual.uri);assertEquals("",app.queue.find(manual.mediaId)!!.uri)
        manual.state="sent"
        assertTrue(Staging(app).completeFile(manual))
        assertEquals("",app.queue.find(manual.mediaId)!!.uri) // Never fall back to the provider.
        val privateUri="content://${c.packageName}.files/shared-recent/kept.bin"
        suspend fun row(automatic:Boolean,uri:String)=Outgoing().apply {mediaId=randomId();transferId=randomId();state="sent";this.automatic=automatic;kind="file";this.uri=uri}.also {assertTrue(app.enqueueFixture(it))}
        val oldManual=row(false,source.toString());val cached=row(false,privateUri);val media=row(true,source.toString())
        app.recoverOutgoing()
        assertEquals("",app.queue.find(oldManual.mediaId)!!.uri);assertEquals("",app.queue.find(cached.mediaId)!!.uri);assertEquals(source.toString(),app.queue.find(media.mediaId)!!.uri)
    }
}
