package io.github.stantonmatt.pneu

import android.app.Application
import android.content.Context
import androidx.room.withTransaction
import androidx.room.Room
import io.github.stantonmatt.pneu.connection.*
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import io.github.stantonmatt.pneu.features.*

class PneuApp : Application() {
    lateinit var graph: AppGraph
    override fun onCreate() { super.onCreate(); StatusNotificationChannel.create(this); graph = AppGraph(this) }
}
val Context.graph get() = (applicationContext as PneuApp).graph
class AppGraph(val context: Context,val preferences: Preferences=Preferences(context),
               val scope:CoroutineScope=CoroutineScope(SupervisorJob() + Dispatchers.IO)) {
    private val snapshots=PreferenceSnapshots(preferences.flow,scope)
    val prefs=snapshots.state
    suspend fun awaitPrefs():Prefs=snapshots.awaitLoaded()
    internal fun captureBinding(select:(Prefs)->DeliveryBinding?={DeliveryBinding.from(it)}):Deferred<DeliveryBinding?> =snapshots.captureBinding(select)
    internal fun captureAdmission(select:(Prefs)->DeliveryBinding?={DeliveryBinding.from(it)}):Deferred<FrozenAdmission> =snapshots.capture {p->
        FrozenAdmission(select(p),recipients(p,"text"),recipients(p,"clipboard_image"),recipients(p,"file"),recipients(p,"file",selected=p.devices.map {it.deviceId}.toSet()))
    }
    val database = Room.databaseBuilder(context, QueueDatabase::class.java, "outgoing.db").addMigrations(object : androidx.room.migration.Migration(1,2) {
        override fun migrate(db: androidx.sqlite.db.SupportSQLiteDatabase) {
            db.execSQL("CREATE INDEX IF NOT EXISTS index_outgoing_state_createdAt ON outgoing(state,createdAt)")
        }
    }, object : androidx.room.migration.Migration(2,3) {
        override fun migrate(db: androidx.sqlite.db.SupportSQLiteDatabase) {
            db.execSQL("ALTER TABLE outgoing ADD COLUMN kind TEXT NOT NULL DEFAULT 'screenshot'")
            db.execSQL("ALTER TABLE outgoing ADD COLUMN origin TEXT NOT NULL DEFAULT 'clipboard'")
            db.execSQL("ALTER TABLE outgoing ADD COLUMN text TEXT NOT NULL DEFAULT ''")
            db.execSQL("ALTER TABLE outgoing ADD COLUMN automatic INTEGER NOT NULL DEFAULT 1")
            db.execSQL("ALTER TABLE outgoing ADD COLUMN attempted INTEGER NOT NULL DEFAULT 0")
            db.execSQL("ALTER TABLE outgoing ADD COLUMN optimized INTEGER NOT NULL DEFAULT 0")
            db.execSQL("ALTER TABLE outgoing ADD COLUMN orderId INTEGER NOT NULL DEFAULT 0")
            db.execSQL("UPDATE outgoing SET orderId=rowid")
            db.execSQL("CREATE TABLE IF NOT EXISTS receipts (peerId TEXT NOT NULL, transferId TEXT NOT NULL, hash TEXT NOT NULL, origin TEXT NOT NULL, state TEXT NOT NULL, createdAt INTEGER NOT NULL, PRIMARY KEY(peerId,transferId))")
        }
    }, object : androidx.room.migration.Migration(3,4) {
        override fun migrate(db: androidx.sqlite.db.SupportSQLiteDatabase) { db.execSQL("ALTER TABLE outgoing ADD COLUMN sourceHash TEXT NOT NULL DEFAULT ''");db.execSQL("CREATE INDEX IF NOT EXISTS index_outgoing_state_orderId ON outgoing(state,orderId)") }
    }, object : androidx.room.migration.Migration(4,5) {
        override fun migrate(db: androidx.sqlite.db.SupportSQLiteDatabase) {
            db.execSQL("ALTER TABLE outgoing ADD COLUMN revision INTEGER NOT NULL DEFAULT 0")
            db.execSQL("ALTER TABLE outgoing ADD COLUMN rejectedCapabilities TEXT NOT NULL DEFAULT ''")
            db.execSQL("UPDATE receipts SET state='applied' WHERE state='complete'")
            db.execSQL("UPDATE outgoing SET state='superseded' WHERE kind='text' AND state IN ('waiting','sending','failed') AND orderId < (SELECT MAX(orderId) FROM outgoing WHERE kind='text' AND state IN ('waiting','sending','failed'))")
        }
    }, QueueMigrations.clipboardWindow, QueueMigrations.outboundWindow, QueueMigrations.recipients, QueueMigrations.peerWatermarks, QueueMigrations.pendingAdmissions, QueueMigrations.receivedFiles).addCallback(object : androidx.room.RoomDatabase.Callback() {
        override fun onOpen(db: androidx.sqlite.db.SupportSQLiteDatabase) { db.execSQL("PRAGMA synchronous=FULL") }
    }).build()
    val queue = database.queue()
    val queueMutex = Mutex()
    /** HLC and legacy order are assigned in one durable serialized edit. */
    suspend fun newTextOrder(apply:Boolean=true):Pair<ClipboardStamp,Long> =newTextOrders(1,apply).single()
    suspend fun newTextOrders(count:Int,apply:Boolean=true):List<Pair<ClipboardStamp,Long>> =withContext(Dispatchers.IO) {
        initialized.await();preferences.originateTexts(count,queue.latestTextTime(),apply)
    }
    suspend fun newTextTimestamp():Long=newTextOrder().second
    private val bindingMutex=Mutex()
    // Tombstones live for this process, just like attempts. Only the captured
    // target is compared; another computer's lifecycle cannot cancel a scan.
    private val lifecycle=mutableMapOf<String,Long>()
    private val outcomes=mutableMapOf<String,Long>()
    private fun invalidateOutcomes(id:String) {outcomes[id]=Math.addExact(outcomes[id] ?: 0,1)}
    internal fun changed(peer:Peer) {
        connection.revoked(peer)
        for(key in listOf("id:${peer.deviceId}","pin:${peer.pin}"))lifecycle[key]=Math.addExact(lifecycle[key] ?: 0,1)
        invalidateOutcomes(peer.deviceId)
    }
    internal suspend fun captureOutcome(peerId:String):ConnectionOutcome=bindingMutex.withLock {ConnectionOutcome(peerId,outcomes[peerId] ?: 0)}
    internal suspend fun connectionView(pin:String?=null):ConnectionView=bindingMutex.withLock {
        val p=BindingTransitions.scoped(preferences.read(),pin)
        val a=connection.attemptFor(pin)
        ConnectionView(p,a,p.peer?.let {ConnectionOutcome(it.deviceId,outcomes[it.deviceId] ?: 0)})
    }
    private fun currentOutcome(token:ConnectionOutcome?)=token==null || outcomes.getOrDefault(token.peerId,0)==token.revision
    val initialized=CompletableDeferred<Unit>()
    val recent = queue.recent().recoverReads().stateIn(scope, SharingStarted.Eagerly, emptyList())
    val waitingCount = queue.waitingCount().recoverReads().stateIn(scope, SharingStarted.Eagerly, 0)
    val work = queue.work().recoverReads().stateIn(scope, SharingStarted.Eagerly, emptyList())
    @Volatile internal var onMediaObserverReady:()->Unit={}
    val mediaRefresh=MutableStateFlow(0L)
    fun requestMediaScan() {mediaRefresh.update {it+1}}
    val staging = Staging(this)
    val clipboard = ClipboardDelivery(this)
    // Receive data hook; the designer-owned Recent ViewModel can combine this flow.
    val receivedFiles = ReceivedFiles(this)
    val receivedRecent get()=receivedFiles.recent
    val connection = Connections(this)
    internal val tileInputs=combine(prefs,connection.state,connection.devices) {p,s,devices->
        TilePolicy.inputs(p,s,devices,connection.clipboardAvailable(p))
    }.distinctUntilChanged().shareIn(scope,SharingStarted.Eagerly,replay=1)
    fun recipients(p:Prefs,kind:String,automatic:Boolean=false,selected:Set<String>?=null):List<DeliveryBinding> {
        val online=p.devices.filter {connection.connected(it.deviceId)}.map {it.deviceId}.toSet()
        return RecipientSelection.freeze(p,online,kind,automatic,selected).filter {target->target.pcId !in online ||
            connection.supports(target.pcId,if(kind=="text")"clipboard_text" else "files") &&
            (kind!="clipboard_image" || connection.supports(target.pcId,"clipboard_image"))}.mapNotNull {target->
                if(kind in setOf("text","clipboard_image") || automatic && !p.queue)connection.freezeSession(target) else target
            }
    }
    internal fun recipientCurrent(target:DeliveryBinding,p:Prefs)=CapturedAuthority(target).stillValid(p,connection.generation(target)) &&
        (target.sessionId==null || connection.ownsSession(target))
    internal fun hasRecipient(row:Outgoing,binding:DeliveryBinding?,p:Prefs)=
        (row.recipients ?: listOfNotNull(binding)).any {recipientCurrent(it,p)}
    /** Fixtures restore already-admitted snapshots/history; production automatic
     * entry points must pass the scan stamp to enqueue below. */
    @androidx.annotation.VisibleForTesting
    internal suspend fun enqueueFixture(row:Outgoing):Boolean=bindingMutex.withLock {
        enqueueCurrent(row,DeliveryBinding.from(preferences.read()))
    }
    suspend fun enqueue(row: Outgoing,binding:DeliveryBinding?,observation:AdmissionToken?=null): Boolean = bindingMutex.withLock {
        val admitted=if(row.automatic)observation!=null && preferences.publishAutomatic(observation,queueMutex) {p->enqueueLocked(row,binding,p)} else enqueueCurrent(row,binding)
        if(!admitted && row.automatic)requestMediaScan()
        admitted
    }
    private suspend fun enqueueCurrent(row:Outgoing,binding:DeliveryBinding?):Boolean=queueMutex.withLock {
        enqueueLocked(row,binding,preferences.read())
    }
    /** Caller holds the queue lock, and for automatic work also the policy lock. */
    private suspend fun enqueueLocked(row:Outgoing,binding:DeliveryBinding?,p:Prefs):Boolean {
        if(row.automatic && (row.state=="skipped" || row.recipients?.isEmpty()==true)) {
            if(binding==null || !binding.matches(p))return false
            QueueAdmission.retireAutomatic(database,row);return true
        }
        if(!hasRecipient(row,binding,p))return false
        if(row.automatic && queue.seen(row.mediaId)!=0)return false
        val captured=row.recipients ?: listOfNotNull(binding)
        // A provider can label a text file as application/octet-stream. Apply
        // provenance to the actual staged bytes regardless of advertised MIME.
        val provenance=row.sendProvenance?.takeUnless {it.needsHash} ?: ClipboardSendPolicy.provenance(p,
            if(row.kind=="text")io.github.stantonmatt.pneu.protocol.sha256(row.text.toByteArray()) else row.hash,
            owned=row.sendProvenance?.received==true,markedSource=row.sendProvenance?.source.orEmpty())
        row.sendProvenance=provenance
        val recipients=ClipboardSendPolicy.targets(p,captured,provenance,row.automatic)
        row.recipients=recipients.filter {recipientCurrent(it,p)}
        if(row.recipients.isEmpty()) {
            if(!row.automatic)throw ShareFailure("no_recipient")
            // Suppression is terminal, not an invalidated snapshot to retry.
            QueueAdmission.retireAutomatic(database,row);return true
        }
        val inserted=try {QueueAdmission.admit(database,row)}
        catch(e:LocalCapacityException) {preferences.toggle("localCapacity",true);throw e}
        maintainTerminalLocked()
        return inserted
    }
    fun maintainTerminalLocked()=PostCommit.maintain {pruneTerminalLocked()}
    fun invalidateSleeping() {scope.launch {PostCommit.maintainSuspending {preferences.toggle("neverSleeping",false)}}}
    suspend fun recoverOutgoing()=withContext(Dispatchers.IO) {queueMutex.withLock {
        queue.forgetManualSources("")
        queue.recover();maintainTerminalLocked()
    }}
    /** A serial sender calls this only between transfers, never during its own receipt wait.
     * Preserve the other lane's active slot. Avoid no-op writes/Room wakeups when idle. */
    internal suspend fun recoverDeliveryLane(text:Boolean,binding:DeliveryBinding)=queueMutex.withLock {
        if(queue.sendingBoundLane(binding.pcId,binding.generation,text)>0)queue.recoverBound(binding.pcId,binding.generation,text)
    }
    @androidx.annotation.VisibleForTesting
    suspend fun recoverDeliveryLane(text:Boolean,peerId:String) {
        DeliveryBinding.from(preferences.read(),peerId)?.let {recoverDeliveryLane(text,it)}
    }
    /** Caller holds queueMutex. Evict payload history but retain automatic media IDs. */
    fun pruneTerminalLocked() {
        val evicted=queue.evictedTerminal()
        database.runInTransaction {evicted.forEach {
            if(it.automatic)queue.insertSeen(MediaSeen().apply {mediaId=it.mediaId})
            queue.deleteOutgoing(it.mediaId)
        }}
        evicted.forEach {if(it.path.isNotEmpty())java.io.File(it.path).delete()}
    }
    private suspend fun clearPeerDelivery(peerId:String)=queueMutex.withLock {
        receivedFiles.forget(peerId)
        database.runInTransaction {queue.clearPeer(peerId)}
        pruneOrphansLocked()
        releaseTerminalBytesLocked()
    }
    private fun releaseTerminalBytesLocked() {queue.all().distinctBy {it.path}.filter {it.path.endsWith(".bin") && queue.hasPath(it.path)==0}.forEach {java.io.File(it.path).delete()}}
    internal suspend fun abandonClipboard(binding:DeliveryBinding,sessionOnly:Boolean=false)=queueMutex.withLock {
        if(sessionOnly)queue.dropPeerSessionClipboard(binding.pcId,binding.generation) else queue.dropPeerClipboard(binding.pcId,binding.generation)
        releaseTerminalBytesLocked()
    }
    /** Explicit whole-fixture reset; production revocation only calls clearPeerDelivery. */
    @androidx.annotation.VisibleForTesting
    internal suspend fun clearDelivery()=queueMutex.withLock {
        queue.all().forEach {if(it.path.isNotEmpty())java.io.File(it.path).delete()}
        receivedFiles.forgetAll();queue.clear();queue.clearReceipts();staging.clearThumbnails()
    }
    @androidx.annotation.VisibleForTesting
    internal suspend fun recoverDeliveryLane(text:Boolean)=recoverDeliveryLane(text,preferences.read().peer?.deviceId.orEmpty())
    private fun pruneOrphansLocked() {queue.orphanArtifacts().forEach {artifact->
        queue.deleteArtifact(artifact.mediaId)
        if(artifact.path.isNotEmpty())java.io.File(artifact.path).delete()
    }}
    suspend fun setPairSwitch(peerId:String,key:String,value:Boolean)=withContext(Dispatchers.IO) {bindingMutex.withLock {
        setPairSwitchLocked(peerId,key,value)
    }}
    internal suspend fun setPairSwitch(authority:CapturedAuthority,key:String,value:Boolean):Boolean=withContext(Dispatchers.IO) {withAuthority(authority) {
        setPairSwitchLocked(authority.binding.pcId,key,value);true
    } ?: false}
    private suspend fun setPairSwitchLocked(peerId:String,key:String,value:Boolean) {
        // Policy -> queue is the same lock order as admission. A failed queue
        // update leaves preferences untouched; a failed preference edit rolls
        // back the queue. Finish both commits even if the UI owner goes away.
        withContext(NonCancellable) {preferences.policy.locked {queueMutex.withLock {
            database.withTransaction {
                if(key=="sendClipboard" && !value)queue.supersedePeerText(peerId)
                preferences.setPairSwitchLocked(peerId,key,value)
            }
        }}}
    }
    suspend fun updateDelivery(row:Outgoing):Boolean=queueMutex.withLock {
        val current=queue.delivery(row.mediaId,row.peerId)
        if(current==null || current.state in setOf("discarded","skipped","sent","superseded") || current.revision!=row.revision || current.transferId!=row.transferId || current.bindingGeneration!=row.bindingGeneration)false
        else {
            if(row.kind=="text" && row.state in setOf("waiting","failed") && queue.newerPeerText(row.peerId,row.orderId)>0)row.state="superseded"
            queue.update(row);maintainTerminalLocked();true
        }
    }
    /** Short pre-emission commit. No socket I/O runs under the binding/queue locks. */
    internal suspend fun beginDelivery(row:Outgoing,binding:DeliveryBinding,
                                      eligible:(Prefs,Outgoing?,Boolean)->Boolean,begin:(Prefs)->Boolean):Boolean=bindingMutex.withLock {preferences.policy.locked {
        queueMutex.withLock commit@{
            val p=preferences.read()
            if(!binding.matches(p) || row.peerId!=binding.pcId || row.bindingGeneration!=binding.generation)return@commit false
            val current=queue.delivery(row.mediaId,row.peerId)
            if(!eligible(p,current,row.kind=="text" && queue.newerPeerText(row.peerId,row.orderId)>0))return@commit false
            if(!begin(p))return@commit false
            row.state="sending";row.attempted=true
            queue.update(row);true
        }
    }}
    suspend fun <T> withBinding(binding:DeliveryBinding?,body:suspend ()->T):T?=bindingMutex.withLock {
        val current=preferences.read()
        if(binding==null || !binding.matches(current))null else body()
    }
    internal suspend fun <T> withBindingLifecycle(body:suspend ()->T):T=bindingMutex.withLock {body()}
    /** Connection admission and deferred effects share the binding lifecycle lock. */
    internal suspend fun <T> withAuthority(authority:CapturedAuthority,body:suspend ()->T):T?=withBinding(authority.binding) {
        if(authority.stillValid(preferences.read(),connection.generation(authority.binding)))body() else null
    }
    suspend fun checkpoint(binding:DeliveryBinding,video:Boolean,version:String,generation:Long,pairId:Long?=null,floor:Long?=null,expected:AdmissionToken?=null):Boolean=bindingMutex.withLock {
        val committed=binding.matches(preferences.read()) && preferences.checkpoint(video,version,generation,pairId,floor,expected)
        if(!committed)requestMediaScan()
        committed
    }
    private val deliveryResets=DeliveryResets(
        read={preferences.read()},
        resolve={p->queueMutex.withLock {queue.resolveLegacy(p);pruneOrphansLocked()}},
        clearLegacy={clearDelivery()},finishLegacy={preferences.toggle("deliveryReset",false)},
        clearPeer={clearPeerDelivery(it)},finishPeer={preferences.finishPeerReset(it)}
    )
    internal suspend fun completePeerReset(id:String)=deliveryResets.peer(id)
    suspend fun reconcileDelivery()=withContext(Dispatchers.IO) {bindingMutex.withLock {
        preferences.migrateSettingsReceipts(queue)
        deliveryResets.reconcile()
        preferences.finishMembersIntents(queue)
        val recovered=preferences.read()
        connection.attempt.value?.takeIf {!it.matches(recovered,lifecycle)}?.let {endPairing(it)}
        reconcileSetupCandidatesLocked()
        // Recover a process death between the DataStore and Room commits.
        queueMutex.withLock {preferences.read().devices.filter {!it.clipboardSend}.forEach {queue.supersedePeerText(it.deviceId)}}
    }}
    suspend fun loseBinding(expected:Peer?,expectedPhoneId:String?=null,internalOutcome:ConnectionOutcome?=null):Boolean=withContext(NonCancellable) {bindingMutex.withLock {
        val current=preferences.read()
        if(!currentOutcome(internalOutcome) || expected==null || expectedPhoneId!=null && current.deviceId!=expectedPhoneId ||
            current.devices.none {it.deviceId==expected.deviceId && it.pin==expected.pin && it.generation==expected.generation})return@withLock false
        if(current.devices.find {it.deviceId==expected.deviceId}?.setupScoped==true)return@withLock false
        preferences.forget(expected.name,expected.deviceId)
        changed(expected)
        connection.bindingChanged(lost=true,peerId=expected.deviceId)
        completePeerReset(expected.deviceId)
        true
    }}
    suspend fun requestPairing(uri:String,expected:ConfirmedBinding,setupEnabled:Boolean=true):Boolean = withContext(Dispatchers.IO) {bindingMutex.withLock {
        val p=preferences.read()
        if(!expected.matches(p))return@withLock false
        startPairing(uri,p,setupEnabled)
    }}

    // Fixture callers explicitly supply their already-selected pin.
    suspend fun requestPairing(uri:String,expectedPin:String?):Boolean = withContext(Dispatchers.IO) {bindingMutex.withLock {
        val p=preferences.read()
        if(p.peer?.pin!=expectedPin)return@withLock false
        startPairing(uri,p)
    }}

    private fun startPairing(uri:String,p:Prefs,setupEnabled:Boolean=true):Boolean {
        if(connection.attempt.value!=null)return false
        val invitation=io.github.stantonmatt.pneu.protocol.Invitation.parse(uri)
        val prior=p.devices.find {it.pin==invitation.fingerprint}
        connection.clearPairingResult()
        if(prior!=null && !prior.pairingLost && connection.connected(prior.deviceId)) {
            connection.pairingFailed(context.getString(R.string.pair_already_paired,prior.name))
            return true
        }
        prior?.let {invalidateOutcomes(it.deviceId)}
        connection.setAttempt(PairingAttempt(io.github.stantonmatt.pneu.protocol.randomId(),invitation,p,lifecycle.toMap(),setupEnabled=setupEnabled && p.membership.roster.count {it.active}<17))
        // Invalidates the old transport before any further outcome can commit.
        connection.retryPeer(invitation.fingerprint)
        return true
    }
    suspend fun cancelPairing(expected:PairingAttempt?=connection.attempt.value):Boolean=withContext(Dispatchers.IO) {bindingMutex.withLock {
        if(expected==null) {
            if(connection.attempt.value!=null)return@withLock false
        } else if(!endPairing(expected))return@withLock false
        connection.clearPairingResult(expected?.invitation?.fingerprint)
        true
    }}

    internal fun endPairing(expected:PairingAttempt?,message:String?=null):Boolean {
        if(expected==null || connection.attempt.value?.id!=expected.id)return false
        // Revoke first. Closing is local and never sends UNPAIR.
        connection.setAttempt(null)
        if(message!=null)connection.pairingFailed(message)
        connection.retryPeer(expected.invitation.fingerprint)
        scope.launch {reconcileSetupCandidates()}
        return true
    }
    internal suspend fun rejectPairing(expected:PairingAttempt,status:ConnectionStatus):Boolean=withContext(NonCancellable) {bindingMutex.withLock {
        if(!endPairing(expected))false else {connection.pairingFailed(status);true}
    }}
    internal suspend fun rejectPairing(expected:PairingAttempt,message:String):Boolean=withContext(NonCancellable) {bindingMutex.withLock {endPairing(expected,message)}}
    internal suspend fun pairingHello(expected:PairingAttempt,pcId:String):PairingAttempt?=bindingMutex.withLock {
        val a=connection.attempt.value?.takeIf {it.id==expected.id} ?: return@withLock null
        if(a.deviceId!=null && a.deviceId!=pcId) {endPairing(a,"Conflicting peer identity");return@withLock null}
        val resolved=a.copy(deviceId=pcId)
        val p=preferences.read()
        val prior=try {PairingIdentity.prior(p.devices,a.invitation,pcId,p.deviceId)}
            catch(e:IllegalArgumentException) {endPairing(a,requireNotNull(e.message));return@withLock null}
        if(!resolved.matches(p,lifecycle)) {endPairing(a,"Pairing cancelled");return@withLock null}
        if(prior!=null && prior.pin!=a.invitation.fingerprint && !prior.pairingLost) {
            endPairing(a,"Already paired as ${prior.name}. Unpair it first.");return@withLock null
        }
        if(a.deviceId==null)invalidateOutcomes(pcId)
        connection.setAttempt(resolved)
        resolved
    }
    suspend fun retryConnection(expected:ConfirmedBinding,attempt:PairingAttempt?,feedback:Boolean=false):Boolean=withContext(Dispatchers.IO) {bindingMutex.withLock {
        val p=preferences.read()
        if(attempt!=null) {
            if(connection.attempt.value?.id!=attempt.id)return@withLock false
        } else if(connection.attempt.value!=null || !expected.matches(p))return@withLock false
        if(feedback)connection.state.update {it.withPairing("Pairing…")}
        connection.retryPeer(attempt?.invitation?.fingerprint ?: expected.pin);true
    }}

    suspend fun retryConnections(expected:List<ConfirmedBinding>):Boolean=withContext(Dispatchers.IO) {bindingMutex.withLock {
        val current=preferences.read()
        if(expected.isEmpty() || expected.any {!it.matches(current)})return@withLock false
        expected.forEach {connection.retryPeer(it.pin)}
        true
    }}

    @androidx.annotation.VisibleForTesting
    suspend fun installBinding(invitation:io.github.stantonmatt.pneu.protocol.Invitation,pcId:String):Boolean {
        val attempt=connection.attempt.value?.takeIf {it.invitation==invitation} ?: return false
        val resolved=pairingHello(attempt,pcId) ?: return false
        return installSessionBinding(resolved,pcId,resolved.scanned.deviceId)!=null
    }
    internal suspend fun installSessionBinding(attempt:PairingAttempt,pcId:String,
                                               phoneId:String,setupHandshake:SetupHandshake?=null,onOutcome:(ConnectionOutcome)->Unit={},onInstalled:(DeliveryBinding)->Unit={}):DeliveryBinding?=bindingMutex.withLock {
        val current=connection.attempt.value?.takeIf {it.id==attempt.id && it.deviceId==pcId} ?: return@withLock null
        val p=preferences.read()
        if(p.deviceId!=phoneId || pcId==phoneId || !current.matches(p,lifecycle))return@withLock null
        val replacement=current.prior?.let {ConfirmedBinding.from(current.scanned,it.deviceId)}
        var installed:DeliveryBinding?=null
        if(!preferences.paired(current.invitation,pcId,replacement,setupChoice=current.setupEnabled,setupFence=current.scanned.membership,beforeInstall={data,binding->
            setupHandshake?.let {h->
                PeerRegistry.write(data,PeerRegistry.read(data).map {peer->if(peer.deviceId==binding.pcId && peer.pin==binding.pin)
                    peer.copy(name=h.remoteName,setupSupported=h.remote!=null,remoteSetupScope=h.remote?.scope,
                        addresses=(h.remoteAddresses+peer.addresses).distinct().take(16)) else peer})
                SetupTransactions.tokenHello(data,binding,h.remote,h.self)
            }
        }) {installed=it;onInstalled(it)})return@withLock null
        // Durable installation consumed the attempt. Cleanup retries use resetPeers;
        // neither cleanup failure nor a lost READY can install it again.
        connection.installed(current,requireNotNull(installed))
        connection.setAttempt(null)
        if(current.prior?.let {it.pin!=current.invitation.fingerprint}==true || current.prior==null)
            p.devices.find {it.deviceId==pcId}?.let(::changed) ?: invalidateOutcomes(pcId)
        onOutcome(ConnectionOutcome(pcId,outcomes[pcId] ?: 0))
        for(id in preferences.read().resetPeers)completePeerReset(id)
        scope.launch {reconcileSetupCandidates()}
        installed?.takeIf {it.matches(preferences.read())}
    }
    suspend fun forgetBinding(expected:Peer?=null,expectedPhoneId:String?=null,rotateUnpaired:Boolean=true,
                              confirmed:ConfirmedBinding?=null,internalOutcome:ConnectionOutcome?=null,remoteUnpair:Boolean=false,setupMember:ConfirmedSetupMember?=null,beforeRemoval:()->Unit={}):Boolean=withContext(Dispatchers.IO+NonCancellable) {bindingMutex.withLock {
        val p=preferences.read()
        if(!currentOutcome(internalOutcome) || confirmed!=null && !confirmed.matches(p) || expectedPhoneId!=null && p.deviceId!=expectedPhoneId)return@withLock false
        val id=expected?.deviceId ?: confirmed?.pcId ?: p.peer?.deviceId
        val peer=p.devices.find {it.deviceId==id}
        if(expected!=null && (peer?.pin!=expected.pin || peer.generation!=expected.generation))return@withLock false
        if(peer==null && confirmed?.pcId!=null) {
            val key="id:${confirmed.pcId}"
            lifecycle[key]=Math.addExact(lifecycle[key] ?: 0,1)
            invalidateOutcomes(confirmed.pcId)
            connection.attempt.value?.takeIf {it.targetId==confirmed.pcId}?.let {endPairing(it)}
            completePeerReset(confirmed.pcId);return@withLock true
        }
        val rosterMember=p.membership.roster.find {it.deviceId==id && it.active && (peer==null || it.fingerprint==peer.pin)}
        if(!remoteUnpair && (setupMember!=null && (!setupMember.matches(p) || setupMember.member!=rosterMember) ||
            rosterMember!=null && setupMember==null))return@withLock false
        beforeRemoval()
        if(rosterMember!=null && !remoteUnpair)preferences.edit {SetupTransactions.remove(it,rosterMember.key,p.membership.generation)}
        else preferences.forget(peerId=id)
        peer?.let(::changed)
        connection.attempt.value?.takeIf {it.targetId==id && (peer==null || it.invitation.fingerprint==peer.pin)}?.let {endPairing(it)}
        connection.bindingChanged(peerId=id)
        if(id!=null) {
            // Setup removal is committed. resetPeers retains cleanup failures for recovery.
            if(rosterMember!=null && !remoteUnpair)try {completePeerReset(id)}
            catch(e:Exception) {android.util.Log.w("PneuRemovalCleanup",e.javaClass.simpleName)}
            else completePeerReset(id)
        }
        true
    }}
    init {
        scope.launch {tileInputs.collect {TileRefresh.request(context)}}
        scope.launch {
            PostCommit.maintain {ManualEntries.clearLegacyShortcuts(context)}
            java.io.File(context.cacheDir,"shared-recent").deleteRecursively()
            LegacyUriGrants.release(context.contentResolver)
            // A transient identity/storage failure must not permanently poison
            // initialized. Reset/queue recovery is also retried by each dial.
            while(isActive) {
                try {preferences.ensureIdentity();break}
                catch(e:CancellationException) {throw e}
                catch(e:Exception) {android.util.Log.w("PneuInitialize",e.javaClass.simpleName);delay(2000)}
            }
            try {receivedFiles.recover();clipboard.reconcile();reconcileDelivery();recoverOutgoing()}
            catch(e:CancellationException) {throw e}
            catch(e:Exception) {android.util.Log.w("PneuRecovery",e.javaClass.simpleName)}
            initialized.complete(Unit)
        }
        scope.launch {
            initialized.await()
            prefs.filter {it.loaded}.map {it.resetPeers to it.membership.receiptIntents}.distinctUntilChanged().collect {pending->
                if(pending.first.isNotEmpty() || pending.second.isNotEmpty())while(isActive) {
                    try {reconcileDelivery();break}
                    catch(e:CancellationException) {throw e}
                    catch(e:Exception) {android.util.Log.w("PneuSetupRecovery",e.javaClass.simpleName);delay(60000)}
                }
            }
        }
        resumeSetupLinkDiscovery()
        scope.launch { prefs.filter {it.loaded && it.deviceId.isNotEmpty()}.distinctUntilChangedBy {Triple(it.peer,it.shareSheet,it.pairingLost)}.collect {p->PostCommit.maintain {ManualEntries.configureShareTarget(context,p.shareSheet);if(!p.pairingLost)context.getSystemService(android.app.NotificationManager::class.java).cancel(2)} } }
    }
    /** A Recent row represents its artifact's complete, originally frozen operation. */
    suspend fun confirmArtifact(p:Prefs,item:ConfirmedItem):ConfirmedQueue=withContext(Dispatchers.IO) {queueMutex.withLock {
        val rows=queue.deliveries(item.mediaId)
        check(rows.any(item::matches)) {"Queue item changed"}
        ConfirmedQueue.artifact(p,item,rows)
    }}
    /** No later query may expand the set the user selected or target a newer row. */
    suspend fun discard(selected:ConfirmedQueue,clearCapacity:Boolean=false):Boolean=withContext(Dispatchers.IO) {bindingMutex.withLock {
        if(!selected.matches(preferences.read()))return@withLock false
        val removed=queueMutex.withLock commit@{
            val rows=selected.items.map {item->queue.delivery(item.mediaId,item.peerId)?.takeIf(item::matches) ?: return@commit false}
            database.runInTransaction {rows.forEach {it.state="discarded";it.revision++;queue.update(it)}}
            rows.forEach {connection.cancelTransfer(it.mediaId,it.peerId);if(queue.hasPath(it.path)==0)java.io.File(it.path).delete()}
            maintainTerminalLocked();true
        }
        if(removed && clearCapacity)preferences.toggle("localCapacity",false)
        removed
    }}
    suspend fun discardAll(selected:ConfirmedQueue=ConfirmedQueue.from(prefs.value,work.value)):Boolean=discard(selected,clearCapacity=true)
    suspend fun retry(id:String,expected:ConfirmedQueue?=null):Boolean=withContext(Dispatchers.IO) {bindingMutex.withLock {
        if(expected!=null && !expected.matches(preferences.read()))return@withLock false
        val retried=queueMutex.withLock commit@{
            if(expected!=null && (expected.items.isEmpty() || expected.items.any {it.mediaId!=id}))return@commit false
            val rows=if(expected!=null)expected.resolve(queue.deliveries(id)) ?: return@commit false
                else queue.deliveries(id).filter {it.state in setOf("waiting","failed")}
            val retryable=rows.filter {it.state in setOf("waiting","failed")}
            if(retryable.isEmpty())return@commit false
            database.runInTransaction {retryable.forEach {row->
                row.state=if(row.kind=="text" && queue.newerPeerText(row.peerId,row.orderId)>0)"superseded" else "waiting"
                row.error="";row.nextAttempt=0;row.mismatches=0;row.revision++;row.rejectedCapabilities=""
                queue.update(row)
            }}
            maintainTerminalLocked();true
        }
        if(retried)connection.kick()
        retried
    }}
}
