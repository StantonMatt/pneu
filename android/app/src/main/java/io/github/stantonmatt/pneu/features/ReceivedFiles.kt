package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/** Shared durable storage authority; live chunk state belongs to each connection.
 * Binding/policy locks fence publication against unpair and direction changes. */
class ReceivedFiles internal constructor(private val app:AppGraph,
    private val storage:ReceiveStorage=ReceivedMediaStore(app.context),
    peerLimit:Int=QueueDao.RECEIVE_PEER_LIMIT,globalLimit:Int=QueueDao.RECEIVE_GLOBAL_LIMIT) {
    private val journal=ReceiveJournal(app.queue,storage,peerLimit,globalLimit)
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
            storage.cleanOrphans(app.queue.unfinishedReceivedFiles().map {it.uri}.toSet())
        }
        effects.recoverClipboardMarker()
        recovered=true
    }
    private suspend fun recoverRow(row:ReceivedFile) {
        val p=app.preferences.read();val peer=p.devices.find {it.deviceId==row.peerId}
        val allowed=peer!=null && ReceivePolicy.newTransfer(row.kind,p.paused,peer.clipboardReceive,
            ReceivePolicy.automaticEnabled(peer,p.deviceId),mesh=true)==null
        journal.recover(row,allowPublication=allowed)
    }
    internal fun forget(peer:String)=journal.forget(peer)
    internal fun forgetAll() {for(row in app.queue.unfinishedReceivedFiles())journal.deletePartial(row)}
    /** Direct storage fixtures have no live connection; production captures its token. */
    @androidx.annotation.VisibleForTesting
    internal fun connection(binding:DeliveryBinding,mesh:Boolean,current:()->Boolean):FileReceiveMachine =
        connection(CapturedAuthority(binding),mesh,current)
    internal fun connection(authority:CapturedAuthority,mesh:Boolean,current:()->Boolean):FileReceiveMachine {
        val binding=authority.binding
        return FileReceiveMachine(ReceiveBackend {metadata->
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
            if(retained==FileAdmission.RETAINED_CONFLICT)return@ReceiveBackend ReceiveAdmission(reply=ReceiveReply("id_conflict"))
            if(retained==FileAdmission.RETAINED_REPLAY) {
                return@ReceiveBackend ReceiveAdmission(reply=ReceiveReply(deduplicated=true))
            }
            recover()
            app.withAuthority(authority) {
                app.preferences.policy.locked {
                    if(!current())throw ReceiveViolation("closing")
                    var admission=journal.admission(binding.pcId,metadata)
                    if(admission==FileAdmission.UNFINISHED) {
                        recoverRow(requireNotNull(app.queue.receivedFile(binding.pcId,metadata.id)))
                        admission=journal.admission(binding.pcId,metadata)
                    }
                    when(admission) {
                        FileAdmission.RETAINED_REPLAY->ReceiveAdmission(reply=ReceiveReply(deduplicated=true))
                        FileAdmission.RETAINED_CONFLICT->ReceiveAdmission(reply=ReceiveReply("id_conflict"))
                        FileAdmission.UNFINISHED->ReceiveAdmission(reply=ReceiveReply("storage_failed"))
                        FileAdmission.NEW->{
                            val p=app.preferences.read();val peer=requireNotNull(p.devices.find {it.deviceId==binding.pcId})
                            val refusal=ReceivePolicy.newTransfer(metadata.kind,p.paused,peer.clipboardReceive,ReceivePolicy.automaticEnabled(peer,p.deviceId),mesh)
                            when {
                                refusal!=null->ReceiveAdmission(reply=refusal)
                                !ReceivePolicy.acceptsMime(metadata.mime)->ReceiveAdmission(reply=ReceiveReply("unsupported_kind"))
                                else->prepare(authority,peer.name,metadata,mesh,current)
                            }
                        }
                    }
                }
            } ?: throw ReceiveViolation("closing")
        })
    }
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
