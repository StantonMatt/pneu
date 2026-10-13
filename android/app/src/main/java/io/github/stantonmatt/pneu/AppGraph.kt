package io.github.stantonmatt.pneu

import android.app.Application
import android.content.Context
import androidx.room.Room
import io.github.stantonmatt.pneu.connection.ConnectionManager
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import io.github.stantonmatt.pneu.features.*

class PneuApp : Application() {
    lateinit var graph: AppGraph
    override fun onCreate() { super.onCreate(); graph = AppGraph(this) }
}
val Context.graph get() = (applicationContext as PneuApp).graph
class AppGraph(val context: Context,val preferences: Preferences=Preferences(context)) {
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val snapshots=PreferenceSnapshots(preferences.flow,scope)
    val prefs=snapshots.state
    suspend fun awaitPrefs():Prefs=snapshots.awaitLoaded()
    internal fun captureBinding(select:(Prefs)->DeliveryBinding?={DeliveryBinding.from(it)}):Deferred<DeliveryBinding?> =snapshots.captureBinding(select)
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
    }, QueueMigrations.clipboardWindow, QueueMigrations.outboundWindow).addCallback(object : androidx.room.RoomDatabase.Callback() {
        override fun onOpen(db: androidx.sqlite.db.SupportSQLiteDatabase) { db.execSQL("PRAGMA synchronous=FULL") }
    }).build()
    val queue = database.queue()
    val queueMutex = Mutex()
    private var reservedTextTime=0L
    /** Assign order before validation/admission can suspend or acquire other locks. */
    suspend fun newTextTimestamp():Long=withContext(Dispatchers.IO) {
        initialized.await()
        queueMutex.withLock {
            maxOf(System.currentTimeMillis(),reservedTextTime+1,queue.latestTextTime()+1).also {reservedTextTime=it}
        }
    }
    private val bindingMutex=Mutex()
    val initialized=CompletableDeferred<Unit>()
    val recent = queue.recent().recoverReads().stateIn(scope, SharingStarted.Eagerly, emptyList())
    val waitingCount = queue.waitingCount().recoverReads().stateIn(scope, SharingStarted.Eagerly, 0)
    val work = queue.work().recoverReads().stateIn(scope, SharingStarted.Eagerly, emptyList())
    @Volatile internal var onMediaObserverReady:()->Unit={}
    val mediaRefresh=MutableStateFlow(0L)
    fun requestMediaScan() {mediaRefresh.update {it+1}}
    val staging = Staging(this)
    val clipboard = ClipboardDelivery(this)
    val connection = ConnectionManager(this)
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
        if(binding==null || !binding.matches(p))return false
        if(row.automatic && queue.seen(row.mediaId)!=0)return false
        if(row.state=="waiting" && queue.pendingCount()-(if(row.kind=="text")queue.pending().count {it.kind=="text"} else 0)>=2048) {preferences.toggle("localCapacity",true);throw LocalCapacityException()}
        var inserted=false
        database.runInTransaction {
            if(row.kind=="text") {
                // Lock acquisition order is not text creation order. Never
                // promote an old value over newer work, including after eviction.
                if(row.createdAt<=queue.latestTextTime())row.state="superseded"
                else queue.supersedeText()
            }
            row.orderId=queue.nextOrder();inserted=queue.insert(row)!=-1L
            if(inserted) {
                queue.rememberOutgoing(if(row.kind=="text")row.createdAt else 0,row.orderId)
                if(row.automatic)queue.insertSeen(MediaSeen().apply {mediaId=row.mediaId})
            }
        }
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
    suspend fun recoverDeliveryLane(text:Boolean)=queueMutex.withLock {
        if(queue.sendingInLane(text)>0)queue.recoverLane(text)
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
    suspend fun clearDelivery() = queueMutex.withLock {
        queue.all().forEach { java.io.File(it.path).delete() };queue.clear();queue.clearReceipts()
        staging.clearThumbnails()
        preferences.setString("lastReceivedHash","");preferences.setString("lastSentHash","")
    }
    suspend fun updateDelivery(row:Outgoing):Boolean=queueMutex.withLock {
        val current=queue.find(row.mediaId)
        if(current==null || current.state in setOf("discarded","skipped","sent","superseded") || current.revision!=row.revision || current.transferId!=row.transferId)false
        else {
            if(row.kind=="text" && row.state in setOf("waiting","failed") && queue.newerText(row.orderId)>0)row.state="superseded"
            queue.update(row);maintainTerminalLocked();true
        }
    }
    /** Short pre-emission commit. No socket I/O runs under the binding/queue locks. */
    internal suspend fun beginDelivery(row:Outgoing,binding:DeliveryBinding,
                                      eligible:(Prefs,Outgoing?,Boolean)->Boolean,begin:()->Boolean):Boolean=bindingMutex.withLock {preferences.policy.locked {
        queueMutex.withLock commit@{
            val p=preferences.read()
            if(!binding.matches(p))return@commit false
            val current=queue.find(row.mediaId)
            if(!eligible(p,current,row.kind=="text" && queue.newerText(row.orderId)>0))return@commit false
            if(!begin())return@commit false
            row.state="sending";row.attempted=true
            queue.update(row);true
        }
    }}
    suspend fun <T> withBinding(binding:DeliveryBinding?,body:suspend ()->T):T?=bindingMutex.withLock {
        val current=preferences.read()
        if(binding==null || !binding.matches(current))null else body()
    }
    suspend fun checkpoint(binding:DeliveryBinding,video:Boolean,version:String,generation:Long,pairId:Long?=null,floor:Long?=null,expected:AdmissionToken?=null):Boolean=bindingMutex.withLock {
        val committed=binding.matches(preferences.read()) && preferences.checkpoint(video,version,generation,pairId,floor,expected)
        if(!committed)requestMediaScan()
        committed
    }
    suspend fun reconcileDelivery() = withContext(Dispatchers.IO) { bindingMutex.withLock {
        if(preferences.read().deliveryReset) {
            clearDelivery();preferences.toggle("deliveryReset",false)
        }
    } }
    suspend fun loseBinding(expected: Peer?,expectedPhoneId:String?=null): Boolean = withContext(NonCancellable) { bindingMutex.withLock {
        val current=preferences.read()
        if(expected==null || current.peer!=expected || current.pending!=null || expectedPhoneId!=null && current.deviceId!=expectedPhoneId)return@withLock false
        staging.abortSources()
        preferences.forget(expected.name);connection.bindingChanged(lost=true)
        clearDelivery();preferences.toggle("deliveryReset",false)
        storageDeadline {prefs.first {it.peer==null && it.pairingLost && !it.deliveryReset}}
        true
    } }
    suspend fun requestPairing(uri:String,expected:ConfirmedBinding):Boolean = bindingMutex.withLock {
        if(!expected.matches(preferences.read()))return@withLock false
        preferences.pending(uri);connection.bindingChanged();connection.retry();true
    }
    // Fixture/connection callers explicitly supply their already-selected pin.
    suspend fun requestPairing(uri: String,expectedPin: String?): Boolean = bindingMutex.withLock {
        if(preferences.read().peer?.pin!=expectedPin)return@withLock false
        preferences.pending(uri);connection.bindingChanged();connection.retry();true
    }
    suspend fun cancelPairing(expected: io.github.stantonmatt.pneu.protocol.Invitation?=null):Boolean = bindingMutex.withLock {
        if(!preferences.cancelPending(expected))return@withLock false
        connection.retry();true
    }
    suspend fun cancelPairing(expected:io.github.stantonmatt.pneu.protocol.Invitation?,confirmed:ConfirmedBinding):Boolean=bindingMutex.withLock {
        val current=preferences.read()
        if(!confirmed.matches(current) || current.pending!=expected)return@withLock false
        if(!preferences.cancelPending(expected))return@withLock false
        connection.retry();true
    }
    suspend fun retryConnection(expected:ConfirmedBinding,pending:io.github.stantonmatt.pneu.protocol.Invitation?,feedback:Boolean=false):Boolean=bindingMutex.withLock {
        val current=preferences.read()
        if(!expected.matches(current) || current.pending!=pending)return@withLock false
        if(feedback)connection.state.update {it.withPairing("Pairing…")}
        connection.retry();true
    }
    suspend fun installBinding(invitation: io.github.stantonmatt.pneu.protocol.Invitation,pcId: String): Boolean = bindingMutex.withLock {
        if(!preferences.paired(invitation,pcId,invitation))return@withLock false
        connection.bindingChanged()
        // Durable binding/reset journal first. Any interrupted cleanup is completed before delivery.
        if(preferences.read().deliveryReset) {clearDelivery();preferences.toggle("deliveryReset",false)}
        storageDeadline {prefs.first {it.pending==null && it.peer?.pin==invitation.fingerprint && !it.deliveryReset}}
        true
    }
    suspend fun forgetBinding(expected: Peer?=null,expectedPhoneId:String?=null,rotateUnpaired:Boolean=true,confirmed:ConfirmedBinding?=null,beforeRemoval:()->Unit={}) = withContext(NonCancellable) { bindingMutex.withLock {
        val current=preferences.read()
        if(confirmed!=null && !confirmed.matches(current))return@withLock false
        if(expected!=null && current.peer!=expected || expectedPhoneId!=null && (current.deviceId!=expectedPhoneId || current.peer!=expected))return@withLock false

        beforeRemoval()
        staging.abortSources()
        // A retry after durable revocation/cleanup is idempotent. Keep the
        // authoritative check under the binding lock so replacement cannot win
        // between an unpaired UI snapshot and a successful no-op cleanup.
        if(rotateUnpaired || current.peer!=null || current.pending!=null) {
            preferences.forget();connection.bindingChanged()
        }
        if(preferences.read().deliveryReset) {clearDelivery();preferences.toggle("deliveryReset",false)}
        storageDeadline {prefs.first {it.peer==null && it.pending==null && !it.deliveryReset}}
        true
    } }
    init {
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
            try {reconcileDelivery();recoverOutgoing()}
            catch(e:CancellationException) {throw e}
            catch(e:Exception) {android.util.Log.w("PneuRecovery",e.javaClass.simpleName)}
            initialized.complete(Unit)
        }
        scope.launch { prefs.filter {it.loaded && it.deviceId.isNotEmpty()}.distinctUntilChangedBy {Triple(it.peer,it.shareSheet,it.pairingLost)}.collect {p->PostCommit.maintain {ManualEntries.configureShareTarget(context,p.shareSheet);if(!p.pairingLost)context.getSystemService(android.app.NotificationManager::class.java).cancel(2)} } }
    }
    /** No later query may expand the set the user selected or target a newer row. */
    suspend fun discard(selected:ConfirmedQueue,clearCapacity:Boolean=false):Boolean=withContext(Dispatchers.IO) {bindingMutex.withLock {
        if(!selected.binding.matches(preferences.read()))return@withLock false
        val removed=queueMutex.withLock commit@{
            val rows=selected.items.map {item->queue.find(item.mediaId)?.takeIf(item::matches) ?: return@commit false}
            database.runInTransaction {rows.forEach {it.state="discarded";it.revision++;queue.update(it)}}
            rows.forEach {connection.cancelTransfer(it.mediaId);java.io.File(it.path).delete()}
            maintainTerminalLocked();true
        }
        if(removed && clearCapacity)preferences.toggle("localCapacity",false)
        removed
    }}
    suspend fun discardAll(selected:ConfirmedQueue=ConfirmedQueue.from(prefs.value,work.value)):Boolean=discard(selected,clearCapacity=true)
    suspend fun retry(id:String,expected:ConfirmedQueue?=null):Boolean=withContext(Dispatchers.IO) {bindingMutex.withLock {
        if(expected!=null && !expected.binding.matches(preferences.read()))return@withLock false
        val retried=queueMutex.withLock commit@{
            val row=queue.find(id)?.takeIf {it.state in setOf("waiting","failed")} ?: return@commit false
            if(expected!=null && (expected.items.size!=1 || !expected.items.single().matches(row)))return@commit false
            row.state=if(row.kind=="text" && queue.newerText(row.orderId)>0)"superseded" else "waiting"
            row.error="";row.nextAttempt=0;row.mismatches=0;row.revision++;row.rejectedCapabilities=""
            queue.update(row);maintainTerminalLocked();true
        }
        if(retried)connection.kick()
        retried
    }}
}
