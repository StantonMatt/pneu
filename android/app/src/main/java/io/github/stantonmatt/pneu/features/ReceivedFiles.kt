package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.hex
import kotlinx.coroutines.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

private class CorruptReceivePrefix:java.io.IOException()

/** Shared durable storage authority; live chunk state belongs to each connection.
 * Binding/policy locks fence publication against unpair and direction changes. */
class ReceivedFiles internal constructor(private val app:AppGraph,
    private val storage:ReceiveStorage=ReceivedMediaStore(app.context),
    peerLimit:Int=QueueDao.RECEIVE_PEER_LIMIT,globalLimit:Int=QueueDao.RECEIVE_GLOBAL_LIMIT,
    private val spool:ReceiveSpool=ReceiveSpool(java.io.File(app.context.filesDir,"receive-partials")),
    private val spoolSpace:()->Long={spool.root.usableSpace}) {
    private val journal=ReceiveJournal(app.queue,storage,peerLimit,globalLimit,spool::delete)
    private val leases=java.util.concurrent.ConcurrentHashMap<String,()->Boolean>()
    private fun key(peer:String,id:String)="$peer:$id"
    private val recovery=Mutex()
    private var recovered=false
    private val effects=ReceivedFileEffects(app)
    /** Data hook for Recent. Sender name is captured for history and survives a
     * later rename. This flow contains only successfully published files. */
    val recent get()=app.queue.recentReceivedFiles()
    internal suspend fun recover()=recovery.withLock {
        if(recovered)return@withLock
        app.withBindingLifecycle {
            val p=app.preferences.read()
            for(row in app.queue.unfinishedReceivedFiles()) {
                val binding=DeliveryBinding.from(p,row.peerId)?.takeIf {it.generation==row.generation}
                app.preferences.policy.locked {
                    if(binding!=null)recoverRow(row)
                    else journal.recover(row,allowPublication=false)
                }
            }
            evictPartials(null)
            spool.clean(app.queue.unfinishedReceivedFiles().map {it.stagePath}.toSet())
            storage.cleanOrphans(app.queue.unfinishedReceivedFiles().map {it.uri}.toSet())
        }
        effects.recoverClipboardMarker()
        recovered=true
    }
    private suspend fun recoverRow(row:ReceivedFile,preservePartial:Boolean=true) {
        val p=app.preferences.read();val peer=p.devices.find {it.deviceId==row.peerId}
        val allowed=peer!=null && ReceivePolicy.newTransfer(row.kind,p.paused,peer.clipboardReceive,
            ReceivePolicy.automaticEnabled(peer,p.deviceId),mesh=true)==null
        if(row.stagePath.isNotEmpty() && row.state=="writing" && preservePartial && allowed && spool.reconcile(row)) {
            if(row.uri.isNotEmpty()) {if(row.catalogVersion==storage.version())storage.deletePending(row.uri);row.uri=""}
            app.queue.updateReceivedFile(row)
        } else {
            journal.recover(row,allowPublication=allowed)
            if(row.state=="complete" && row.stagePath.isNotEmpty())spool.delete(row)
        }
    }
    internal fun forget(peer:String)=journal.forget(peer)
    internal fun forgetAll() {for(row in app.queue.unfinishedReceivedFiles())journal.deletePartial(row)}
    /** Direct storage fixtures have no live connection; production captures its token. */
    @androidx.annotation.VisibleForTesting
    internal fun connection(binding:DeliveryBinding,mesh:Boolean,current:()->Boolean):FileReceiveMachine =
        connection(CapturedAuthority(binding),mesh,current=current)
    internal fun connection(authority:CapturedAuthority,mesh:Boolean,resumeCapable:Boolean=false,current:()->Boolean):FileReceiveMachine {
        val binding=authority.binding
        val legacy:suspend (ReceivedMetadata,()->Boolean)->ReceiveAdmission=legacy@{metadata,cancelled->
            // Clock/identity validation precedes receipts, observations and policy.
            val stamp=if(mesh && metadata.hlc!=null) {
                if(metadata.source!=binding.pcId)throw ReceiveViolation("malformed_message")
                ClipboardStamp(metadata.hlc.toBigInteger(),binding.pcId).also {
                    try {MeshClock.validate(it.hlc)}catch(_:MeshViolation) {throw ReceiveViolation("malformed_message")}
                }
            } else null
            if(mesh && metadata.kind=="clipboard_image" && stamp==null)throw ReceiveViolation("malformed_message")
            // A retained conflict/replay must not depend on recovery of some
            // unrelated partial file or optional clipboard action.
            val retained=app.withAuthority(authority) {app.preferences.policy.locked {
                if(!current())throw ReceiveViolation("closing")
                if(stamp!=null)app.preferences.observeClipboard(stamp)
                journal.admission(binding.pcId,metadata)
            }} ?: throw ReceiveViolation("closing")
            if(retained==FileAdmission.RETAINED_CONFLICT)return@legacy ReceiveAdmission(reply=ReceiveReply("id_conflict"))
            if(retained==FileAdmission.RETAINED_REPLAY) {
                return@legacy ReceiveAdmission(reply=ReceiveReply(deduplicated=true))
            }
            recover()
            app.withAuthority(authority) {
                app.preferences.policy.locked {
                    if(!current())throw ReceiveViolation("closing")
                    var admission=journal.admission(binding.pcId,metadata)
                    if(admission==FileAdmission.UNFINISHED) {
                        recoverRow(requireNotNull(app.queue.receivedFile(binding.pcId,metadata.id)),preservePartial=false)
                        admission=journal.admission(binding.pcId,metadata)
                    }
                    when(admission) {
                        FileAdmission.RETAINED_REPLAY->ReceiveAdmission(reply=ReceiveReply(deduplicated=true))
                        FileAdmission.RETAINED_CONFLICT->ReceiveAdmission(reply=ReceiveReply("id_conflict"))
                        FileAdmission.UNFINISHED->{
                            ReceiveDiagnostics.warn(binding.pcId,metadata.id,metadata.kind,"unfinished_recovery")
                            ReceiveAdmission(reply=ReceiveReply("storage_failed"))
                        }
                        FileAdmission.NEW->{
                            val p=app.preferences.read();val peer=requireNotNull(p.devices.find {it.deviceId==binding.pcId})
                            val refusal=ReceivePolicy.newTransfer(metadata.kind,p.paused,peer.clipboardReceive,ReceivePolicy.automaticEnabled(peer,p.deviceId),mesh)
                            when {
                                refusal!=null->ReceiveAdmission(reply=refusal)
                                !ReceivePolicy.acceptsMime(metadata.mime)->ReceiveAdmission(reply=ReceiveReply("unsupported_kind"))
                                else->prepare(authority,peer.name,metadata,mesh) {current() && !cancelled()}
                            }
                        }
                    }
                }
            } ?: throw ReceiveViolation("closing")
        }
        return FileReceiveMachine(ReceiveBackend {error("Fenced legacy backend required")},legacyBackend=legacy,resumeCapable=resumeCapable,peerId=binding.pcId,
            resumeBackend={meta,cancelled->admitResume(authority,mesh,current,meta,cancelled)},
            cancelPartial={id->app.withBinding(binding) {
                app.preferences.policy.locked {
                    val row=app.queue.receivedFile(binding.pcId,id)
                    if(row!=null && row.generation==binding.generation && row.state=="writing") {
                        journal.abandon(row)
                    }
                }
            };Unit})
    }
    private suspend fun admitResume(authority:CapturedAuthority,mesh:Boolean,current:()->Boolean,
        metadata:ReceivedMetadata,cancelled:()->Boolean):ReceiveAdmission {
        if(metadata.sensitive)return ReceiveAdmission(reply=ReceiveReply("unsupported_kind"))
        val binding=authority.binding
        if(mesh && metadata.hlc!=null) {
            if(metadata.source!=binding.pcId)throw ReceiveViolation("malformed_message")
            try {MeshClock.validate(metadata.hlc.toBigInteger())}catch(_:MeshViolation) {throw ReceiveViolation("malformed_message")}
        }
        if(mesh && metadata.kind=="clipboard_image" && metadata.hlc==null)throw ReceiveViolation("malformed_message")
        recover()
        val leaseKey=key(binding.pcId,metadata.id)
        var row:ReceivedFile?=null
        val early=app.withAuthority(authority) {app.preferences.policy.locked {
            if(!current() || cancelled())throw ReceiveViolation("closing")
            if(metadata.hlc!=null)app.preferences.observeClipboard(ClipboardStamp(metadata.hlc.toBigInteger(),binding.pcId))
            var admission=journal.admission(binding.pcId,metadata)
            if(admission==FileAdmission.UNFINISHED) {
                val prior=requireNotNull(app.queue.receivedFile(binding.pcId,metadata.id))
                if(leases.containsKey(leaseKey)) {
                    ReceiveDiagnostics.warn(binding.pcId,metadata.id,metadata.kind,"lease_conflict")
                    return@locked ReceiveAdmission(reply=ReceiveReply("storage_failed"))
                }
                recoverRow(prior)
                admission=journal.admission(binding.pcId,metadata)
            }
            if(admission==FileAdmission.RETAINED_CONFLICT)return@locked ReceiveAdmission(reply=ReceiveReply("id_conflict"))
            if(admission==FileAdmission.RETAINED_REPLAY)return@locked ReceiveAdmission(reply=ReceiveReply(deduplicated=true))
            val p=app.preferences.read();val peer=requireNotNull(p.devices.find {it.deviceId==binding.pcId})
            val refusal=ReceivePolicy.newTransfer(metadata.kind,p.paused,peer.clipboardReceive,ReceivePolicy.automaticEnabled(peer,p.deviceId),mesh)
            if(refusal!=null)return@locked ReceiveAdmission(reply=refusal)
            if(!ReceivePolicy.acceptsMime(metadata.mime))return@locked ReceiveAdmission(reply=ReceiveReply("unsupported_kind"))
            if(leases.containsKey(leaseKey)) {
                ReceiveDiagnostics.warn(binding.pcId,metadata.id,metadata.kind,"lease_conflict")
                return@locked ReceiveAdmission(reply=ReceiveReply("storage_failed"))
            }
            evictPartials(binding.pcId.takeIf {app.queue.receivedFile(binding.pcId,metadata.id)==null},binding.pcId,metadata)
            row=app.queue.receivedFile(binding.pcId,metadata.id)
            // The retained prefix already consumes space. Budget the suffix plus
            // the entire publication copy, and check the destination volume too.
            val remaining=metadata.size-(row?.checkpoint ?: 0)
            if(remaining>spoolSpace()-SPACE_RESERVE-metadata.size)throw ReceiveStorageException("disk_full")
            storage.checkSpace(metadata.size,SPACE_RESERVE)
            if(row==null) {
                val created=ReceivedFile().apply {
                    peerId=binding.pcId;transferId=metadata.id;this.metadata=metadata.canonical;generation=binding.generation
                    fileName=ReceivePolicy.storageName(metadata.name);mime=metadata.mime;kind=metadata.kind;senderName=peer.name
                    size=metadata.size;receivedAt=System.currentTimeMillis();catalogVersion=storage.version()
                }
                journal.reserve(created,metadata)
                try {spool.create(created);app.queue.updateReceivedFile(created)}
                catch(e:Exception) {if(created.stagePath.isNotEmpty())spool.delete(created);journal.abandon(created);throw e}
                row=created
            }
            check(leases.putIfAbsent(leaseKey,cancelled)==null)
            ReceiveAdmission() // Non-terminal marker; storage work happens outside authority locks.
        }} ?: throw ReceiveViolation("closing")
        if(early.reply!=null)return early
        val stage=requireNotNull(row)
        var channel:java.nio.channels.FileChannel?=null
        try {
            channel=spool.open(stage);channel.truncate(stage.checkpoint);channel.position(0)
            val output=channel
            val digest=java.security.MessageDigest.getInstance("SHA-256")
            val hashed=withTimeoutOrNull(85_000) {
                val bytes=java.nio.ByteBuffer.allocate(io.github.stantonmatt.pneu.protocol.CHUNK_SIZE)
                var remaining=stage.checkpoint
                while(remaining>0) {
                    currentCoroutineContext().ensureActive()
                    if(cancelled())throw ReceiveStorageException("storage_failed")
                    bytes.clear();bytes.limit(minOf(bytes.capacity().toLong(),remaining).toInt())
                    val n=output.read(bytes);if(n<=0)throw ReceiveStorageException("storage_failed")
                    digest.update(bytes.array(),0,n);remaining-=n
                }
                true
            } ?: false
            if(!hashed)throw ReceiveStorageException("storage_failed")
            if((digest.clone() as java.security.MessageDigest).digest().hex()!=stage.checkpointHash)throw CorruptReceivePrefix()
            val checkpointDigest=digest.clone() as java.security.MessageDigest
            return ReceiveAdmission(offset=stage.checkpoint,digest=digest,sink=object:ReceiveSink {
                private var closed=false
                private var sealed=false
                private var count=stage.checkpoint
                private var checkpointAt=android.os.SystemClock.elapsedRealtime()
                private fun close() {if(!closed) {closed=true;output.close()}}
                private fun checkpoint() {
                    if(count==stage.checkpoint)return // Reconnect/BEGIN never renews TTL.
                    if(cancelled())throw ReceiveStorageException("storage_failed")
                    output.force(true)
                    if(!spool.matches(stage))throw ReceiveStorageException("storage_failed")
                    val now=System.currentTimeMillis()
                    if(now<stage.wallHighWater)throw ReceiveStorageException("storage_failed")
                    stage.checkpoint=count;stage.progressWall=now;stage.wallHighWater=now
                    stage.checkpointHash=(checkpointDigest.clone() as java.security.MessageDigest).digest().hex()
                    if(app.queue.checkpointReceivedFile(stage.peerId,stage.transferId,stage.generation,stage.stagePath,stage.stageIdentity,
                            stage.checkpoint,stage.checkpointHash,stage.progressWall,stage.wallHighWater)!=1)throw ReceiveStorageException("storage_failed")
                    spool.markProgress(stage);checkpointAt=android.os.SystemClock.elapsedRealtime()
                }
                override fun write(bytes:ByteArray) {
                    if(cancelled())throw ReceiveStorageException("storage_failed")
                    if(spool.root.usableSpace<SPACE_RESERVE+bytes.size)throw ReceiveStorageException("disk_full")
                    val now=System.currentTimeMillis()
                    if(now<stage.wallHighWater)throw ReceiveStorageException("storage_failed")
                    stage.wallHighWater=now
                    ReceiveSpool.write(output,bytes);checkpointDigest.update(bytes);count+=bytes.size
                    if(count-stage.checkpoint>=ReceiveSpool.CHECKPOINT_BYTES || android.os.SystemClock.elapsedRealtime()-checkpointAt>=5000)checkpoint()
                }
                override fun seal() {checkpoint();output.force(true);sealed=true}
                override suspend fun commit():ReceiveReply {
                    check(sealed)
                    // Copy to an owned pending row outside the publication fence.
                    // CANCEL remains prompt while a slow provider copies the spool.
                    val version=storage.version()
                    val (uri,name)=storage.create(metadata)
                    if(storage.version()!=version)throw ReceiveStorageException("storage_failed")
                    stage.catalogVersion=version;stage.uri=uri;stage.fileName=name
                    try {
                        app.withAuthority(authority) {app.preferences.policy.locked {
                            if(cancelled() || !current() || app.queue.receivedFile(binding.pcId,metadata.id)?.stagePath!=stage.stagePath)throw ReceiveViolation("closing")
                            app.queue.updateReceivedFile(stage)
                        }} ?: throw ReceiveViolation("closing")
                        storage.open(uri).use {destination->
                            output.position(0);val bytes=java.nio.ByteBuffer.allocate(io.github.stantonmatt.pneu.protocol.CHUNK_SIZE)
                            while(true) {
                                currentCoroutineContext().ensureActive()
                                if(cancelled())throw ReceiveStorageException("storage_failed")
                                bytes.clear();val n=output.read(bytes);if(n<0)break
                                destination.write(bytes.array().copyOf(n))
                            }
                            destination.sync()
                        }
                        return app.withAuthority(authority) {app.preferences.policy.locked {
                            if(cancelled() || !current())throw ReceiveViolation("closing")
                            val p=app.preferences.read();val peer=requireNotNull(p.devices.find {it.deviceId==binding.pcId})
                            val refusal=ReceivePolicy.newTransfer(metadata.kind,p.paused,peer.clipboardReceive,ReceivePolicy.automaticEnabled(peer,p.deviceId),mesh)
                            if(refusal!=null)return@locked refusal
                            withContext(NonCancellable) {
                                stage.receivedAt=System.currentTimeMillis();journal.verified(stage);journal.publish(stage)
                            }
                            effects.offer(stage,metadata,mesh,authority);ReceiveReply()
                        }} ?: throw ReceiveViolation("closing")
                    }catch(e:Exception) {if(stage.catalogVersion==storage.version())storage.deletePending(uri);throw e}
                }
                override suspend fun abort()=withContext(NonCancellable) {
                    try {
                        try {close()}finally {
                            app.withBinding(binding) {app.preferences.policy.locked {journal.abandon(stage)}}
                            // Completion retains its receipt and may still own a spool.
                            if(stage.state=="complete")spool.delete(stage)
                        }
                    }finally {leases.remove(leaseKey,cancelled)}
                    Unit
                }
                override suspend fun disconnect()=withContext(NonCancellable) {
                    try {
                        if(cancelled()) {abort();return@withContext}
                        app.withBinding(binding) {app.preferences.policy.locked {
                            val retained=app.queue.receivedFile(binding.pcId,metadata.id)
                            if(retained?.state=="writing" && retained.stagePath==stage.stagePath) {
                                if(stage.uri.isNotEmpty()) {if(stage.catalogVersion==storage.version())storage.deletePending(stage.uri);stage.uri="";app.queue.updateReceivedFile(stage)}
                                checkpoint()
                            }
                        }}
                    }finally {try {close()}finally {leases.remove(leaseKey,cancelled)}}
                }
            })
        }catch(e:Exception) {
            try {
                try {channel?.close()}finally {
                    app.withBinding(binding) {app.preferences.policy.locked {journal.abandon(stage)}}
                }
            }finally {leases.remove(leaseKey,cancelled)}
            if(e is CorruptReceivePrefix && !cancelled())return admitResume(authority,mesh,current,metadata,cancelled)
            throw e
        }
    }
    internal suspend fun maintainPartials()=app.withBindingLifecycle {
        app.preferences.policy.locked {
            evictPartials(null)
            val now=System.currentTimeMillis()
            app.queue.unfinishedReceivedFiles().filter {it.stagePath.isNotEmpty() && it.state=="writing" && !leases.containsKey(key(it.peerId,it.transferId))}.forEach {row->
                row.wallHighWater=maxOf(row.wallHighWater,now);app.queue.updateReceivedFile(row)
            }
        }
    }
    private fun evictPartials(peer:String?,diagnosticPeer:String?=peer,metadata:ReceivedMetadata?=null) {
        val inactive=app.queue.unfinishedReceivedFiles().filter {it.state=="writing" && it.stagePath.isNotEmpty() && !leases.containsKey(key(it.peerId,it.transferId))}.sortedBy {it.progressWall}.toMutableList()
        fun discard(row:ReceivedFile) {journal.abandon(row);inactive.remove(row)}
        inactive.toList().filter {spool.expired(it)}.forEach(::discard)
        val live=app.queue.unfinishedReceivedFiles().filter {leases.containsKey(key(it.peerId,it.transferId))}
        val extra=if(peer==null)0 else 1
        fun peerFull()=peer!=null && inactive.count {it.peerId==peer}+live.count {it.peerId==peer}+extra>8
        while(inactive.isNotEmpty() && (inactive.sumOf {it.checkpoint}>ReceiveSpool.BYTE_LIMIT || inactive.size+live.size+extra>64 || peerFull())) {
            discard(if(peerFull())inactive.firstOrNull {it.peerId==peer} ?: inactive.first() else inactive.first())
        }
        if(live.size+extra>64 || peer!=null && live.count {it.peerId==peer}+extra>8) {
            ReceiveDiagnostics.warn(diagnosticPeer ?: "unknown",metadata?.id ?: "none",metadata?.kind ?: "file","partial_capacity",
                counts=" peer_count=${live.count {it.peerId==diagnosticPeer}} peer_limit=8 global_count=${live.size} global_limit=64 requested=$extra")
            throw ReceiveStorageException("storage_failed")
        }
    }
    companion object {internal const val SPACE_RESERVE=16L*1024*1024}
    private fun prepare(authority:CapturedAuthority,name:String,metadata:ReceivedMetadata,mesh:Boolean,current:()->Boolean):ReceiveAdmission {
        val binding=authority.binding
        val row=ReceivedFile().apply {
            peerId=binding.pcId;transferId=metadata.id;this.metadata=metadata.canonical;generation=binding.generation
            fileName=ReceivePolicy.storageName(metadata.name);mime=metadata.mime.lowercase(java.util.Locale.ROOT);kind=metadata.kind;senderName=name
            size=metadata.size;receivedAt=System.currentTimeMillis();catalogVersion=storage.version()
        }
        journal.reserve(row,metadata)
        var output:ReceiveOutput?=null
        try {
            val (uri,actualName)=storage.create(metadata);row.uri=uri;row.fileName=actualName
            // URI is durable before opening/writing, and necessarily before publication.
            app.queue.updateReceivedFile(row)
            output=storage.open(uri)
            return ReceiveAdmission(sink=object:ReceiveSink {
                private var closed=false
                private fun close() {if(!closed) {closed=true;output.close()}}
                override fun write(bytes:ByteArray)=output.write(bytes)
                override fun seal() {output.sync();close()}
                override suspend fun commit():ReceiveReply {
                    return app.withAuthority(authority) {
                        app.preferences.policy.locked {
                            if(!current())throw ReceiveViolation("closing")
                            val p=app.preferences.read();val peer=requireNotNull(p.devices.find {it.deviceId==binding.pcId})
                            val refusal=ReceivePolicy.newTransfer(metadata.kind,p.paused,peer.clipboardReceive,ReceivePolicy.automaticEnabled(peer,p.deviceId),mesh)
                            if(refusal!=null)return@locked refusal
                            // Once publication starts, cancellation cannot drop its
                            // durable follow-up. The journal also survives process death.
                            withContext(NonCancellable) {
                                row.receivedAt=System.currentTimeMillis();journal.verified(row);journal.publish(row)
                            }
                            effects.offer(row,metadata,mesh,authority)
                            ReceiveReply()
                        }
                    } ?: throw ReceiveViolation("closing")
                }
                override suspend fun abort()=withContext(NonCancellable) {
                    try {close()}finally {app.withBinding(binding) {journal.abandon(row)}}
                }
            })
        }catch(e:Exception) {
            runCatching {output?.close()}
            // If URI bookkeeping failed, remove only this operation's pending row.
            runCatching {journal.deletePartial(row)}
            runCatching {journal.abandon(row)}
            throw e
        }
    }
}
