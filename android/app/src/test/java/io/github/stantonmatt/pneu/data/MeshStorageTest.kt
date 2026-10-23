package io.github.stantonmatt.pneu.data

import android.app.Application
import androidx.datastore.preferences.core.*
import androidx.datastore.preferences.preferencesDataStoreFile
import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import androidx.room.Room
import androidx.sqlite.db.*
import androidx.sqlite.db.framework.FrameworkSQLiteOpenHelperFactory
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class MeshStorageTest {
    private val phone="a".repeat(32);private val pc="b".repeat(32);private val other="c".repeat(32)
    private val context get()=RuntimeEnvironment.getApplication()
    private fun peer(id:String)=Peer(id,"f".repeat(64),id,emptyList())
    private fun Invitation.toUri()="pneu://pair?v=1&name=$name&fp=$fingerprint&addr=${addresses.first()}&token=$token&exp=$expires"
    private fun db()=Room.inMemoryDatabaseBuilder(context,QueueDatabase::class.java).build()
    @Test fun pendingAdmissionSurvivesDatabaseReopenAndCannotAcquireNewRecipientsOrOrder()=runBlocking<Unit>(Dispatchers.IO) {
        val name="pending-admission.db";context.deleteDatabase(name)
        fun open()=Room.databaseBuilder(context,QueueDatabase::class.java,name).build()
        val first=Outgoing().apply {
            mediaId="discovered";transferId="original";hlc="123";sourceDevice=phone;admissionEpoch=8
            recipients=listOf(DeliveryBinding(phone,"pin",pc,7,"original-session"))
        }
        val initial=open()
        try {initial.queue().freezeAutomaticAdmission(first)}finally {initial.close()}
        val room=open()
        try {
            val changed=Outgoing().apply {mediaId=first.mediaId;transferId="replacement";hlc="999";recipients=listOf(DeliveryBinding(phone,"pin",other,8,"later-session"))}
            val restored=room.queue().freezeAutomaticAdmission(changed)
            assertEquals(first.transferId,restored.transferId);assertEquals(first.hlc,restored.hlc)
            assertEquals(first.sourceDevice,restored.sourceDevice);assertEquals(first.recipients,restored.recipients)
            assertEquals(8L,restored.admissionEpoch)
            room.queue().clear();assertNull(room.queue().pendingAdmission(first.mediaId))
        }finally {room.close()}
        context.deleteDatabase(name)
    }
    @Test fun legacyCleanupDropsSessionClipboardButRetainsDeliberatelyRestoredFixturesAndFiles()=runBlocking<Unit>(Dispatchers.IO) {
        val room=db()
        try {
            val queue=room.queue()
            for((id,kind,session) in listOf(Triple("live-text","text","old"),Triple("live-image","clipboard_image","old"),Triple("fixture","text",""),Triple("shared","file",""))) {
                queue.insert(Outgoing().apply {mediaId=id;peerId=pc;bindingGeneration=7;this.kind=kind;sessionId=session;automatic=false})
            }
            queue.dropPeerSessionClipboard(pc,7)
            assertEquals(setOf("live-text","live-image"),queue.all().filter {it.state=="superseded"}.map {it.mediaId}.toSet())
            assertEquals(setOf("fixture","shared"),queue.pending().map {it.mediaId}.toSet())
        }finally {room.close()}
    }
    @Test fun roomSevenToEightPreservesRowsAndQuarantinesTheOriginalRecipient()=runBlocking<Unit>(Dispatchers.IO) {
        val name="mesh-migration.db";context.deleteDatabase(name)
        val helper=FrameworkSQLiteOpenHelperFactory().create(SupportSQLiteOpenHelper.Configuration.builder(context).name(name).callback(object:SupportSQLiteOpenHelper.Callback(7) {
            override fun onCreate(db:SupportSQLiteDatabase) {
                db.execSQL("CREATE TABLE outgoing (mediaId TEXT NOT NULL PRIMARY KEY,transferId TEXT NOT NULL,uri TEXT NOT NULL,path TEXT NOT NULL,fileName TEXT NOT NULL,mime TEXT NOT NULL,hash TEXT NOT NULL,state TEXT NOT NULL,error TEXT NOT NULL,kind TEXT NOT NULL DEFAULT 'screenshot',origin TEXT NOT NULL DEFAULT 'clipboard',text TEXT NOT NULL DEFAULT '',automatic INTEGER NOT NULL DEFAULT 1,attempted INTEGER NOT NULL DEFAULT 0,optimized INTEGER NOT NULL DEFAULT 0,orderId INTEGER NOT NULL DEFAULT 0,sourceHash TEXT NOT NULL DEFAULT '',revision INTEGER NOT NULL DEFAULT 0,rejectedCapabilities TEXT NOT NULL DEFAULT '',size INTEGER NOT NULL,createdAt INTEGER NOT NULL,nextAttempt INTEGER NOT NULL,mismatches INTEGER NOT NULL)")
                db.execSQL("CREATE TABLE receipts (peerId TEXT NOT NULL,transferId TEXT NOT NULL,hash TEXT NOT NULL,origin TEXT NOT NULL,state TEXT NOT NULL,createdAt INTEGER NOT NULL,PRIMARY KEY(peerId,transferId))")
                db.execSQL("CREATE TABLE clipboard_clock (peerId TEXT NOT NULL PRIMARY KEY,lastApplied INTEGER NOT NULL)")
                db.execSQL("CREATE TABLE outbound_clock (id INTEGER NOT NULL PRIMARY KEY,textTime INTEGER NOT NULL,lastOrder INTEGER NOT NULL)")
                db.execSQL("CREATE TABLE media_seen (mediaId TEXT NOT NULL PRIMARY KEY)")
                db.execSQL("INSERT INTO outgoing(mediaId,transferId,uri,path,fileName,mime,hash,state,error,size,createdAt,nextAttempt,mismatches) VALUES ('old','transfer','content://source','/private/bytes','file.png','image/png','hash','failed','busy',42,123,456,2)")
                db.execSQL("INSERT INTO outgoing(mediaId,transferId,uri,path,fileName,mime,hash,state,error,kind,size,createdAt,nextAttempt,mismatches) VALUES ('old-text','text-transfer','','','','text/plain','hash','waiting','','text',0,124,0,0)")
            }
            override fun onUpgrade(db:SupportSQLiteDatabase,oldVersion:Int,newVersion:Int)=error("Unexpected upgrade")
        }).build())
        helper.writableDatabase;helper.close()
        val room=Room.databaseBuilder(context,QueueDatabase::class.java,name).addMigrations(QueueMigrations.recipients,QueueMigrations.peerWatermarks,QueueMigrations.pendingAdmissions,QueueMigrations.receivedFiles).build()
        try {
            val queue=room.queue();val row=queue.all().single {it.mediaId=="old"}
            assertEquals("Unfenced clipboard work cannot survive upgrade", "superseded",queue.find("old-text").state)
            assertEquals("",row.peerId);assertEquals("failed",row.state);assertEquals(456L,row.nextAttempt);assertEquals(2,row.mismatches)
            assertEquals("/private/bytes",row.path);assertEquals(42L,row.size)
            queue.bindLegacy(pc,7)
            assertEquals(7L,queue.delivery("old",pc).bindingGeneration);assertNull(queue.delivery("old",other))
        }finally {room.close();context.deleteDatabase(name)}
    }
    @Test fun recipientAckAndUnpairDoNotCompleteOrDeleteAnotherRecipientsPayload()=runBlocking<Unit>(Dispatchers.IO) {
        val room=db()
        try {
            val queue=room.queue()
            fun row(peer:String)=Outgoing().apply {mediaId="media";transferId="transfer";peerId=peer;path="/shared/bytes";automatic=false}
            queue.insert(row(pc));queue.insert(row(other))
            queue.update(queue.delivery("media",pc).apply {state="sent"})
            assertEquals("waiting",queue.delivery("media",other).state);assertEquals(1,queue.hasPath("/shared/bytes"))
            queue.insertReceipt(Receipt().apply {peerId=pc;transferId="one"})
            queue.insertReceipt(Receipt().apply {peerId=other;transferId="two"})
            queue.clearPeer(pc)
            assertEquals(1,queue.all().size);assertEquals(other,queue.all().single().peerId)
            assertNotNull(queue.receipt(other,"two"));assertNull(queue.receipt(pc,"one"));assertEquals(1,queue.hasPath("/shared/bytes"))
        }finally {room.close()}
    }
    @Test fun clipboardPruningLeavesPermanentReceiptsUntouched()=runBlocking<Unit>(Dispatchers.IO) {
        val room=db()
        try {
            val queue=room.queue();queue.insertReceipt(Receipt().apply {peerId=pc;transferId="settings";origin="peer_settings"})
            queue.insertReceipt(Receipt().apply {peerId=pc;transferId="file";origin="file"})
            queue.insertReceipt(Receipt().apply {peerId=pc;transferId="unfinished";state="reserved";createdAt=-1})
            repeat(80) {i->queue.completeReceipt(Receipt().apply {peerId=pc;transferId="clip$i";createdAt=i.toLong();state="applied"})}
            assertEquals(67,queue.receiptCount());assertEquals("reserved",queue.receipt(pc,"unfinished").state);assertNotNull(queue.receipt(pc,"settings"));assertNotNull(queue.receipt(pc,"file"))
        }finally {room.close()}
    }
    private fun withPrefs(body:suspend (Preferences,QueueDao)->Unit)=runBlocking<Unit>(Dispatchers.IO) {
        val owner=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val file=File(context.cacheDir,"mesh-${randomId()}.preferences_pb")
        val store=PreferenceDataStoreFactory.create(scope=owner) {file}
        try {
            store.edit {it[stringPreferencesKey("deviceId")]=phone;it[stringPreferencesKey("tlsAlias")]="existing-key"
                it[stringPreferencesKey("pin")]="f".repeat(64);it[stringPreferencesKey("pcId")]=pc;it[stringPreferencesKey("pcName")]="Old PC"}
            val room=db()
            try {body(Preferences(context,store),room.queue())}finally {room.close()}
        }finally {owner.cancel();owner.coroutineContext[Job]?.join();file.delete()}
    }
    @Test fun effectiveClipboardRegisterFencesAllUnfinishedSourcesAndStampTies()=withPrefs {prefs,_->
        prefs.read()
        val base=System.currentTimeMillis().toBigInteger()
        prefs.store.edit {MeshClock.applied(it,ClipboardStamp(base,phone))}
        prefs.recordClipboardEffect(ClipboardStamp(base+10.toBigInteger(),pc),"first",pc,0,"one")
        prefs.recordClipboardEffect(ClipboardStamp(base+20.toBigInteger(),pc),"newest",pc,0,"two")
        prefs.recordClipboardEffect(ClipboardStamp(base+15.toBigInteger(),other),"middle",other,0,"three")
        assertFalse(prefs.clipboardWins(ClipboardStamp(base+19.toBigInteger(),other)))
        assertFalse(prefs.clipboardWins(ClipboardStamp(base+20.toBigInteger(),phone)))
        assertFalse(prefs.clipboardWins(ClipboardStamp(base+20.toBigInteger(),pc)))
        assertTrue(prefs.clipboardWins(ClipboardStamp(base+20.toBigInteger(),other)))
        assertTrue(prefs.clipboardWins(ClipboardStamp(base+21.toBigInteger(),phone)))
        // Any storage success, even an unrelated setting, persists every effect.
        prefs.setLong("unrelated",7)
        assertEquals(ClipboardStamp(base+20.toBigInteger(),pc),MeshClock.register(prefs.store.data.first()))
        assertNull(prefs.clipboardEffect(pc,0,"two"))
        assertEquals("newest",prefs.read().lastReceivedHash)
        assertEquals(pc,prefs.read().recentClipboardSources["newest"])
        assertEquals(setOf("one","two"),prefs.unfinishedClipboardCompletions(pc,0))
    }
    @Test fun failedReconciliationRetainsFenceUntilStorageRecovers()=withPrefs {prefs,_->
        prefs.read()
        var failWrites=true
        val failing=object:androidx.datastore.core.DataStore<androidx.datastore.preferences.core.Preferences> {
            override val data=prefs.store.data
            override suspend fun updateData(transform:suspend (androidx.datastore.preferences.core.Preferences)->androidx.datastore.preferences.core.Preferences):androidx.datastore.preferences.core.Preferences {
                if(failWrites)throw java.io.IOException("injected storage outage")
                return prefs.store.updateData(transform)
            }
        }
        val receiver=Preferences(context,failing)
        val stamp=ClipboardStamp(System.currentTimeMillis().toBigInteger()+1000.toBigInteger(),pc)
        receiver.recordClipboardEffect(stamp,"retained",pc,0,"one")
        try {receiver.reconcileClipboardEffects();fail("Expected write failure")}catch(_:java.io.IOException) {}
        assertFalse(receiver.clipboardWins(stamp.copy(hlc=stamp.hlc-1.toBigInteger(),source=other)))
        assertEquals(stamp,receiver.clipboardEffect(pc,0,"one"))
        failWrites=false
        receiver.setString("selfName","Recovered")
        assertEquals(stamp,MeshClock.register(prefs.store.data.first()))
        assertNull(receiver.clipboardEffect(pc,0,"one"))
    }
    @Test fun localTextBatchImageAndLegacyAllocationExceedUnfinishedEffects()=withPrefs {prefs,_->
        prefs.read()
        var floor=System.currentTimeMillis().toBigInteger()+1000.toBigInteger()
        fun pending() {
            floor+=1000.toBigInteger()
            prefs.recordClipboardEffect(ClipboardStamp(floor,pc),"received-$floor",pc,0,"$floor")
        }
        pending()
        val text=prefs.originateTexts(3,0)
        assertTrue(text.all {it.first.hlc>floor})
        assertTrue(text.zipWithNext().all {(a,b)->b.first>a.first && b.second>a.second})
        pending();assertTrue(prefs.originateClipboard().hlc>floor) // clipboard image
        pending();assertTrue(prefs.originateClipboard(false).hlc>floor) // automatic image
        pending();assertTrue(prefs.legacyClipboardStamp(pc).hlc>floor)
        prefs.store.edit {it.remove(stringPreferencesKey("meshSeen"))}
        assertTrue(prefs.originateClipboard().hlc>floor)
    }
    @Test fun dataStoreMigrationRollsBackAndUnpairKeepsPhoneIdentityAndOtherPair()=withPrefs {prefs,queue->
        try {prefs.store.edit {PeerRegistry.migrate(it);throw java.io.IOException("Interrupted commit")};fail("Expected rollback")}
        catch(_:java.io.IOException) {}
        assertNull(prefs.store.data.first()[stringPreferencesKey("peers")])
        val migrated=prefs.read();assertEquals(pc,migrated.devices.single().deviceId)
        prefs.store.edit {PeerRegistry.write(it,PeerRegistry.read(it)+peer(other))}
        prefs.forget(peerId=pc)
        val remaining=prefs.read()
        assertEquals(listOf(other),remaining.devices.map {it.deviceId});assertEquals(setOf(pc),remaining.resetPeers)
        assertEquals(phone,remaining.deviceId);assertEquals("existing-key",remaining.tlsAlias)
    }
    @Test fun settingsPendingSnapshotSurvivesEditsAndAckOnlyConfirmsThatSnapshot()=withPrefs {prefs,queue->
        val binding=DeliveryBinding.from(prefs.read())!!
        val first=prefs.prepareSettings(binding)
        prefs.setPairSwitch(pc,"sendClipboard",false)
        assertEquals(first,prefs.prepareSettings(binding)) // exact lost-ACK replay
        prefs.acknowledgeSettings(binding,first)
        assertTrue(PairSettings.pending(prefs.read().devices.single()))
        val changed=prefs.prepareSettings(binding);assertNotEquals(first["transfer_id"],changed["transfer_id"])
        assertFalse(prefs.receiveSettings(binding,changed,queue));assertTrue(prefs.receiveSettings(binding,changed,queue))
        prefs.acknowledgeSettings(binding,changed)
        assertFalse(PairSettings.pending(prefs.read().devices.single()))
    }
    @Test fun pairingAddsAndRepairInvalidatesOnlyTheSelectedComputer()=withPrefs {prefs,queue->
        val first=DeliveryBinding.from(prefs.read())!!
        prefs.setPairSwitch(pc,"sendClipboard",false)
        fun invitation(id:String,pin:Char)=Invitation(1,id,pin.toString().repeat(64),listOf(Endpoint("192.168.1.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        assertTrue(prefs.paired(invitation(other,'e'),other))
        val second=DeliveryBinding.from(prefs.read(),other)!!
        assertTrue(first.matches(prefs.read()))
        assertTrue(prefs.paired(invitation(pc,'f'),pc,replacement=ConfirmedBinding.from(prefs.read(),pc)))
        val repaired=prefs.read()
        assertTrue(first.matches(repaired));assertTrue(second.matches(repaired))
        assertFalse(repaired.devices.first {it.deviceId==pc}.clipboardSend)
        assertEquals(listOf(pc,other),repaired.devices.map {it.deviceId})
        assertEquals(phone,repaired.deviceId);assertEquals("existing-key",repaired.tlsAlias)
    }
    @Test fun changedCertificateCollisionCannotReplaceAHealthyBinding()=withPrefs {prefs,queue->
        val before=prefs.read()
        val invitation=Invitation(1,"Clone","e".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        val pending=prefs.store.data.first()
        assertFalse("A fence cannot authorize a healthy replacement",prefs.paired(invitation,pc))
        assertEquals(pending,prefs.store.data.first())
        assertEquals(before.devices,prefs.read().devices)
    }
    @Test fun sameCertificateWithDifferentAuthenticatedIdFailsClosed()=withPrefs {prefs,queue->
        prefs.read()
        val invitation=Invitation(1,"Changed ID","f".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        val before=prefs.store.data.first()
        assertThrows(IllegalArgumentException::class.java) {runBlocking {prefs.paired(invitation,other)}}
        assertEquals(before,prefs.store.data.first())
    }
    @Test fun fullRegistryCanAuthenticateNewCertificateBeforeDecidingCapacity()=withPrefs {prefs,queue->
        prefs.read()
        prefs.store.edit {p->PeerRegistry.write(p,(0 until PeerRegistry.MAX_PEERS).map {i->
            Peer("Computer $i",i.toString(16).padStart(64,'0'),i.toString(16).padStart(32,'0'),emptyList(),generation=i+1L)
        })}
        val before=prefs.read().devices
        val invitation=Invitation(1,"Repair","e".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        assertEquals(before[7],PairingIdentity.prior(before,invitation,before[7].deviceId,phone))
        assertEquals(before,prefs.read().devices)
    }

    @Test fun lostCertificateRepairAtCapacityReplacesOnlyTheCapturedBinding()=withPrefs {prefs,queue->
        prefs.read()
        prefs.store.edit {p->PeerRegistry.write(p,(0 until PeerRegistry.MAX_PEERS).map {i->
            Peer("Computer $i",i.toString(16).padStart(64,'0'),i.toString(16).padStart(32,'0'),emptyList(),generation=i+1L)
        })}
        prefs.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).mapIndexed {i,peer->if(i==7)peer.copy(pairingLost=true,clipboardSend=false) else peer})}
        val before=prefs.read();val selected=before.devices[7]
        val invitation=Invitation(1,"Repaired","e".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        val approved=ConfirmedBinding.from(prefs.read(),selected.deviceId)
        assertEquals(selected.name,approved.pcName)
        assertFalse(prefs.paired(invitation,selected.deviceId))
        assertTrue(prefs.paired(invitation,selected.deviceId,approved))
        val after=prefs.read()
        assertEquals(16,after.devices.size)
        assertEquals(before.devices.filter {it!=selected},after.devices.filter {it.deviceId!=selected.deviceId})
        assertEquals(invitation.fingerprint,after.devices[7].pin)
        assertEquals(setOf(selected.deviceId),after.resetPeers)
    }
    @Test fun staleOrUnrelatedReplacementApprovalNeverChangesTrustOrResetJournal()=withPrefs {prefs,queue->
        val original=prefs.read();val approval=ConfirmedBinding.from(original,pc)
        val invitation=Invitation(1,"Clone","e".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        prefs.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).map {it.copy(generation=it.generation+1)})}
        val before=prefs.store.data.first()
        assertFalse(prefs.paired(invitation,pc,approval))
        assertFalse(prefs.paired(invitation,pc,approval.copy(pcId=other)))
        assertFalse(prefs.paired(invitation,other,approval))
        assertEquals(before,prefs.store.data.first())
        assertFalse(prefs.paired(invitation,pc,ConfirmedBinding.from(prefs.read(),pc)))
        assertEquals(original.devices.single().pin,prefs.read().devices.single().pin)
        assertTrue(prefs.read().resetPeers.isEmpty())
    }
    @Test fun conflictingFingerprintAndIdCannotReplaceEitherBindingEvenWithApproval()=withPrefs {prefs,queue->
        prefs.read()
        prefs.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p)+peer(other).copy(pin="e".repeat(64)))}
        val invitation=Invitation(1,"Conflict","e".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        val before=prefs.store.data.first()
        try {prefs.paired(invitation,pc,replacement=ConfirmedBinding.from(prefs.read(),pc));fail("Expected conflicting identity")}
        catch(e:IllegalArgumentException) {assertEquals("Conflicting peer identity",e.message)}
        assertEquals(before,prefs.store.data.first())
    }
    @Test fun genuinelyNewComputerAtCapacityFailsOnlyAfterAuthenticatedIdentityResolution()=withPrefs {prefs,queue->
        prefs.read()
        prefs.store.edit {p->PeerRegistry.write(p,(0 until PeerRegistry.MAX_PEERS).map {i->
            Peer("Computer $i",i.toString(16).padStart(64,'0'),i.toString(16).padStart(32,'0'),emptyList(),generation=i+1L)
        })}
        val invitation=Invitation(1,"New","e".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        val before=prefs.store.data.first()
        try {prefs.paired(invitation,"f".repeat(32));fail("Expected capacity refusal")}
        catch(e:IllegalArgumentException) {assertEquals("Computer limit reached",e.message)}
        assertEquals(before,prefs.store.data.first())
    }
    @Test fun knownCertificateCannotChangeDeviceIdEvenWithAFence()=withPrefs {prefs,queue->
        val before=prefs.read()
        val invitation=Invitation(1,"Changed ID","f".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        try {prefs.paired(invitation,other,replacement=ConfirmedBinding.from(before,pc));fail("Expected identity violation")}
        catch(e:IllegalArgumentException) {assertEquals("Conflicting peer identity",e.message)}
        assertEquals(before.devices,prefs.read().devices);assertTrue(prefs.read().resetPeers.isEmpty())
    }

    @Test fun receivedHashesAreBoundedAndSettingsIdConflictsDoNotMutateState()=withPrefs {prefs,queue->
        repeat(70) {prefs.rememberClipboardHash("hash$it")}
        assertEquals(64,prefs.read().recentClipboardHashes.size);assertEquals("hash6",prefs.read().recentClipboardHashes.first())
        val binding=DeliveryBinding.from(prefs.read())!!;val snapshot=prefs.prepareSettings(binding)
        prefs.receiveSettings(binding,snapshot,queue)
        val before=prefs.store.data.first()
        val changed=snapshot.deepCopy();changed["settings"].asJsonArray.first().asJsonObject.addProperty("hlc",2)
        try {prefs.receiveSettings(binding,changed,queue);fail("Expected retained-ID conflict")}catch(_:Preferences.SettingsConflict) {}
        assertEquals(before,prefs.store.data.first())
    }

    @Test fun recoveringOneTextLaneCannotResetAnotherPeersSendingRowOrReplayAnOlderValue()=runBlocking<Unit>(Dispatchers.IO) {
        val room=db()
        try {
            val queue=room.queue()
            fun row(id:String,peer:String,order:Long,state:String)=Outgoing().apply {mediaId=id;transferId=id;peerId=peer;kind="text";orderId=order;this.state=state}
            queue.insert(row("old",pc,1,"sending"));queue.insert(row("new",pc,2,"waiting"));queue.insert(row("other",other,3,"sending"))
            queue.recoverPeer(pc,true)
            assertEquals("superseded",queue.delivery("old",pc).state)
            assertEquals("waiting",queue.delivery("new",pc).state);assertEquals("sending",queue.delivery("other",other).state)
        }finally {room.close()}
    }

    @Test fun sameTrustedComputerRepairRetainsConnectionCache()=withPrefs {prefs,queue->
        val original=DeliveryBinding.from(prefs.read())!!
        assertTrue(prefs.connected(original,Endpoint("192.0.2.1",47821),1234))
        val invitation=Invitation(1,"Repaired","f".repeat(64),listOf(Endpoint("192.0.2.2",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        assertTrue(prefs.paired(invitation,pc,replacement=ConfirmedBinding.from(prefs.read(),pc)))
        val repaired=prefs.read().devices.single()
        assertEquals("192.0.2.1:47821",repaired.lastLan);assertEquals(1234L,repaired.lastConnected)
        assertEquals(original.generation,repaired.generation)
    }
    @Test fun legacyPendingRecordIsDiscardedAndNeverRestored()=withPrefs {prefs,queue->
        prefs.store.edit {it[stringPreferencesKey("pending")]="old invitation"}
        Preferences(context,prefs.store).read()
        assertNull(prefs.store.data.first()[stringPreferencesKey("pending")])
    }

    @Test fun textSupersessionAndEvictionWatermarksArePerRecipient()=runBlocking<Unit>(Dispatchers.IO) {
        val room=db()
        try {
            val queue=room.queue()
            fun row(id:String,target:String,time:Long)=Outgoing().apply {mediaId=id;transferId=id;kind="text";peerId=target;createdAt=time;automatic=false}
            queue.admit(row("a100",pc,100));queue.admit(row("b101",other,101))
            assertEquals("waiting",queue.delivery("a100",pc).state)
            queue.admit(row("both102",pc,102));queue.admit(row("both102",other,102))
            assertEquals("superseded",queue.delivery("a100",pc).state)
            assertEquals("superseded",queue.delivery("b101",other).state)
            queue.deleteOutgoing("both102")
            queue.admit(row("late99",pc,99));assertEquals("superseded",queue.delivery("late99",pc).state)
            queue.admit(row("c90","unrelated",90));assertEquals("waiting",queue.delivery("c90","unrelated").state)
            assertEquals(102L,queue.peerTextTime(pc));assertEquals(102L,queue.peerTextTime(other))
        }finally {room.close()}
    }
    @Test fun oldSessionTeardownCannotSupersedeReplacementGenerationOrOtherRecipients()=runBlocking<Unit>(Dispatchers.IO) {
        val room=db()
        try {
            val queue=room.queue()
            fun row(id:String,target:String,generation:Long,kind:String)=Outgoing().apply {mediaId=id;transferId=id;peerId=target;bindingGeneration=generation;this.kind=kind}
            queue.insert(row("old",pc,1,"text"));queue.insert(row("new",pc,2,"text"))
            queue.insert(row("image",pc,2,"clipboard_image"));queue.insert(row("other",other,1,"text"))
            queue.dropPeerClipboard(pc,1)
            assertEquals("superseded",queue.delivery("old",pc).state)
            for(id in listOf("new","image"))assertEquals("waiting",queue.delivery(id,pc).state)
            assertEquals("waiting",queue.delivery("other",other).state)
        }finally {room.close()}
    }
    @Test fun settingsReceiptsRecoverEveryCrossStoreCrashAndRejectConflictingReplay()=withPrefs {prefs,queue->
        val binding=DeliveryBinding.from(prefs.read())!!
        val snapshot=prefs.prepareSettings(binding)
        for(step in listOf("settingsReserved","settingsMerged")) {
            val incoming=snapshot.deepCopy().apply {addProperty("transfer_id",randomId())}
            prefs.beforeMutation={if(it==step)throw java.io.IOException("Crash")}
            try {prefs.receiveSettings(binding,incoming,queue);fail("Expected interruption")}catch(_:java.io.IOException) {}
            val id=incoming["transfer_id"].asString
            assertEquals("pending",queue.receipt(pc,id).state)
            prefs.beforeMutation={}
            prefs.receiveSettings(binding,incoming,queue)
            assertEquals("applied",queue.receipt(pc,id).state)
            assertTrue(prefs.receiveSettings(binding,incoming,queue))
            val conflicting=incoming.deepCopy();conflicting["settings"].asJsonArray.first().asJsonObject.addProperty("hlc",3)
            val before=prefs.store.data.first()
            try {prefs.receiveSettings(binding,conflicting,queue);fail("Conflict")}catch(_:Preferences.SettingsConflict) {}
            assertEquals(before,prefs.store.data.first())
        }
    }
    @Test fun settingsReceiptsPassFormerLifetimeLimitAndReconnectsReuseAcknowledgedSnapshot()=withPrefs {prefs,queue->
        val binding=DeliveryBinding.from(prefs.read())!!;val snapshot=prefs.prepareSettings(binding)
        assertFalse(prefs.receiveSettings(binding,snapshot,queue))
        prefs.acknowledgeSettings(binding,snapshot)
        repeat(4100) {i->queue.insertReceipt(Receipt().apply {peerId=pc;transferId="filler$i";origin="peer_settings";state="applied"})}
        repeat(8) {
            val reconnect=Preferences(context,prefs.store).prepareSettings(binding)
            assertEquals(snapshot,reconnect)
            assertTrue(prefs.receiveSettings(binding,reconnect,queue))
            prefs.acknowledgeSettings(binding,reconnect)
        }
        val extra=snapshot.deepCopy().apply {addProperty("transfer_id",randomId())}
        assertFalse(prefs.receiveSettings(binding,extra,queue))
        assertEquals(4102,queue.settingsReceiptCount(pc))
        val conflict=snapshot.deepCopy().apply {getAsJsonArray("settings")[0].asJsonObject.addProperty("enabled",false)}
        try {prefs.receiveSettings(binding,conflict,queue);fail("Conflict after former cap")}catch(_:Preferences.SettingsConflict) {}
        assertFalse(prefs.store.data.first()[stringPreferencesKey("peers")]!!.contains("settingsReceipts"))
    }
    @Test fun batchReservationsExcludeConcurrentOriginationAndRollbackTogether()=withPrefs {prefs,_->
        prefs.read()
        val entered=java.util.concurrent.CountDownLatch(1);val release=java.util.concurrent.CountDownLatch(1)
        prefs.beforeMutation={if(it=="textOrderReserved:0") {entered.countDown();check(release.await(5,java.util.concurrent.TimeUnit.SECONDS))}}
        coroutineScope {
            val batch=async(Dispatchers.IO) {prefs.originateTexts(3,0)}
            try {
                assertTrue(withContext(Dispatchers.IO) {entered.await(5,java.util.concurrent.TimeUnit.SECONDS)})
                val started=CompletableDeferred<Unit>()
                val newer=async(Dispatchers.Default) {started.complete(Unit);prefs.originateClipboard()}
                started.await();yield();assertFalse(newer.isCompleted)
                release.countDown()
                val orders=batch.await();val later=newer.await()
                assertTrue(orders.all {it.first<later})
                assertTrue(orders.zipWithNext().all {(a,b)->a.second<b.second})
            }finally {release.countDown();prefs.beforeMutation={}}
        }
        val before=prefs.store.data.first()
        prefs.beforeMutation={if(it=="textOrderReserved:1")throw java.io.IOException("Batch edit failed")}
        try {prefs.originateTexts(3,0);fail("Expected failure")}catch(_:java.io.IOException) {}finally {prefs.beforeMutation={}}
        assertEquals(before,prefs.store.data.first())
    }
    @Test fun legacyReceiptExportIsIdempotentAndDoesNotErasePermanentHistory()=withPrefs {prefs,queue->
        prefs.read();prefs.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).map {it.copy(settingsReceipts=mapOf("old" to "hash"))})}
        prefs.migrateSettingsReceipts(queue);prefs.migrateSettingsReceipts(queue)
        assertEquals(1,queue.settingsReceiptCount(pc));assertEquals("hash",queue.receipt(pc,"old").hash)
        assertFalse(prefs.store.data.first()[stringPreferencesKey("peers")]!!.contains("settingsReceipts"))
    }
    @Test fun localSettingsEditsArePerSwitchAndSeparateFromBootstrapAndLegacySupport()=withPrefs {prefs,queue->
        val binding=DeliveryBinding.from(prefs.read())!!
        assertFalse(PairSettings.pending(prefs.read().devices.single()))
        val bootstrap=prefs.prepareSettings(binding)
        prefs.setPairSwitch(pc,"sendClipboard",false)
        prefs.setPairSwitch(pc,"automaticMedia",false)
        prefs.acknowledgeSettings(binding,bootstrap)
        assertEquals(setOf("$phone/$pc/clipboard","$phone/$pc/automatic_media"),prefs.read().devices.single().localSettingsEdits)
        val changed=prefs.prepareSettings(binding)
        prefs.setPairSwitch(pc,"receiveClipboard",false)
        prefs.acknowledgeSettings(binding,changed)
        assertEquals(setOf("$pc/$phone/clipboard"),prefs.read().devices.single().localSettingsEdits)
        prefs.hello(binding,"Old PC",emptyList(),mesh=false)
        assertEquals(false,prefs.read().devices.single().settingsSupported)
    }
    @Test fun textClockAndLegacyOrderRemainTogetherUnderConcurrencyAndRestart()=withPrefs {prefs,queue->
        prefs.read()
        val values=coroutineScope {(1..50).map {async(Dispatchers.Default) {prefs.originateText(0)}}.awaitAll()}
        val ordered=values.sortedBy {it.first.hlc}
        assertEquals(50,ordered.map {it.first.hlc}.toSet().size)
        assertTrue(ordered.zipWithNext().all {(a,b)->a.second<b.second})
        val restored=Preferences(context,prefs.store).originateText(0)
        assertTrue(restored.first.hlc>ordered.last().first.hlc);assertTrue(restored.second>ordered.last().second)
    }

    @Test fun unpairAndRepairBeforeReconciliationCannotStrandTheOriginalQuarantineAtAnyCrashStep()=withPrefs {prefs,queue->
        prefs.pairingBoundary={CatalogBoundary("catalog",0,0,0)}
        val original=prefs.read();assertEquals(pc,original.legacyQueuePeer)
        val invitation=Invitation(1,"Repair","f".repeat(64),listOf(Endpoint("192.0.2.1",47821)),"d".repeat(64),System.currentTimeMillis()+60000)
        for(repair in listOf(false,true))for(crash in 0..3) {
            if(prefs.read().devices.isEmpty())prefs.paired(invitation,pc)
            queue.clear();queue.insert(Outgoing().apply {mediaId="legacy";transferId="legacy";peerId=""})
            if(repair) {
                val current=prefs.read();val old=current.devices.single()
                prefs.store.edit {PeerRegistry.write(it,listOf(old.copy(pairingLost=true)))}
                val next=invitation.copy(fingerprint=if(old.pin==invitation.fingerprint)"e".repeat(64) else invitation.fingerprint)
                prefs.paired(next,pc,replacement=ConfirmedBinding.from(prefs.read(),pc))
            } else prefs.forget(peerId=pc)
            var step=0
            fun interrupted() {if(step++==crash)throw java.io.IOException("Crash after step")}
            val resets=DeliveryResets(
                read={prefs.read()},
                resolve={p->queue.resolveLegacy(p);interrupted()},
                clearLegacy={queue.clear();queue.clearReceipts()},finishLegacy={prefs.toggle("deliveryReset",false)},
                clearPeer={queue.clearPeer(it);interrupted()},finishPeer={prefs.finishPeerReset(it);interrupted()})
            try {resets.peer(pc)}catch(_:java.io.IOException) {}
            // Simulate a fresh coordinator/process reading only durable state.
            DeliveryResets(read={Preferences(context,prefs.store).read()},resolve={queue.resolveLegacy(it)},
                clearLegacy={queue.clear();queue.clearReceipts()},finishLegacy={prefs.toggle("deliveryReset",false)},
                clearPeer={queue.clearPeer(it)},finishPeer={prefs.finishPeerReset(it)}).reconcile()
            assertEquals(0,queue.pendingCount());assertTrue(queue.orphanArtifacts().isNotEmpty())
            assertTrue(prefs.read().resetPeers.isEmpty());assertNull(queue.delivery("legacy",pc));assertNull(queue.delivery("legacy",""))
        }
    }
    @Test fun legacyFullResetWithoutAnyPeerRetainsItsMarkerThroughEveryCleanupCrash()=withPrefs {prefs,queue->
        prefs.read();prefs.forget(peerId=pc)
        for(crash in 0..2) {
            prefs.store.edit {it[booleanPreferencesKey("deliveryReset")]=true}
            queue.insert(Outgoing().apply {mediaId="legacy";transferId="legacy"})
            queue.insertReceipt(Receipt().apply {peerId=pc;transferId="old"})
            var step=0
            fun interrupted() {if(step++==crash)throw java.io.IOException("Crash")}
            fun journal(fault:Boolean)=DeliveryResets(read={prefs.read()},resolve={queue.resolveLegacy(it)},
                clearLegacy={queue.clear();if(fault)interrupted();queue.clearReceipts();if(fault)interrupted()},
                finishLegacy={prefs.toggle("deliveryReset",false);if(fault)interrupted()},
                clearPeer={queue.clearPeer(it)},finishPeer={prefs.finishPeerReset(it)})
            try {journal(true).reconcile()}catch(_:java.io.IOException) {}
            if(crash<2)assertTrue(prefs.read().deliveryReset)
            journal(false).reconcile()
            assertFalse(prefs.read().deliveryReset);assertEquals(0,queue.receiptCount());assertTrue(queue.all().isEmpty())
        }
    }

    @Test fun oldLaneRecoveryCannotResetOrSupersedeTheReplacementGenerationsSendingRows()=runBlocking<Unit>(Dispatchers.IO) {
        val room=db()
        try {
            val queue=room.queue()
            fun row(id:String,generation:Long,order:Long)=Outgoing().apply {mediaId=id;transferId=id;peerId=pc;bindingGeneration=generation;kind="text";state="sending";orderId=order}
            queue.insert(row("old",1,1));queue.insert(row("replacement",2,2))
            queue.recoverBound(pc,1,true);queue.supersedeBoundText(pc,1)
            assertEquals("superseded",queue.delivery("old",pc).state)
            assertEquals("sending",queue.delivery("replacement",pc).state)
        }finally {room.close()}
    }

    @Test fun unpairBeforeTheFirstReadMigratesAndRevokesTheOriginalOwnerInOneEdit()=withPrefs {prefs,queue->
        prefs.forget(peerId=pc)
        val restored=Preferences(context,prefs.store).read()
        assertTrue(restored.devices.isEmpty());assertEquals(pc,restored.legacyQueuePeer)
        assertTrue(pc in restored.resetPeers)
    }

}
