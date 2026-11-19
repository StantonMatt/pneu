package io.github.stantonmatt.pneu.features

import com.google.gson.JsonObject
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.CancellationException
import java.io.IOException
import java.security.MessageDigest

/** All known wire fields, before storage-name sanitizing. Unknown extensions do not
 * participate in receipt equality. Optional fields retain absent/present identity. */
data class ReceivedMetadata(val id:String,val name:String,val mime:String,val size:Long,val hash:String,
    val kind:String,val createdAt:Long,val mediaId:String?=null,val hlc:String?=null,val source:String?=null,val sensitive:Boolean=false,val folder:Folder?=null) {
    val canonical:String get()=json("transfer_id" to id,"file_name" to name,"mime" to mime,"size" to size,
        "sha256" to hash,"kind" to kind,"created_at" to createdAt).apply {
        folder?.let {add("folder",json("manifest_len" to it.manifestLen))}
        if(sensitive)addProperty("sensitive",true)
        mediaId?.let {addProperty("source_media_id",it)}
        hlc?.let {addProperty("hlc",it.toBigInteger());addProperty("source_device",source)}
    }.toString()
    companion object {
        fun parse(body:JsonObject):ReceivedMetadata {
            Protocol.validate(Type.FILE_BEGIN,body)
            return ReceivedMetadata(body["transfer_id"].asString,body["file_name"].asString,body["mime"].asString,
                body["size"].asLong,body["sha256"].asString,body["kind"].asString,body["created_at"].asLong,
                body["source_media_id"]?.asString,body["hlc"]?.asString?.toBigInteger()?.toString(),body["source_device"]?.asString,body["kind"].asString=="clipboard_image" && body["sensitive"]?.asBoolean==true,body["folder"]?.asJsonObject?.let {Folder(it["manifest_len"].asLong)})
        }
    }
}
internal class ReceiveViolation(val code:String="unexpected_message"):IOException(code)
internal data class ReceiveReply(val code:String?=null,val deduplicated:Boolean=false) {
    fun frame(id:String)=if(code==null)Frame.Control(Type.ACK,json("transfer_id" to id,"deduplicated" to deduplicated))
        else Frame.Control(Type.NACK,json("transfer_id" to id,"code" to code,"retryable" to (code !in setOf("id_conflict","unsupported_kind"))))
}
internal interface ReceiveSink {
    fun write(bytes:ByteArray)
    /** Flush and fsync before the durable verified journal entry is written. */
    fun seal()
    suspend fun commit():ReceiveReply
    /** Must preserve a verified publication journal, even if final bookkeeping failed. */
    suspend fun abort()
    suspend fun disconnect()=abort()
}
internal data class ReceiveAdmission(val sink:ReceiveSink?=null,val reply:ReceiveReply?=null,val offset:Long=0,val digest:MessageDigest=MessageDigest.getInstance("SHA-256"))
internal fun interface ReceiveBackend { suspend fun admit(metadata:ReceivedMetadata):ReceiveAdmission }

/** Serialized by the connection's bounded file worker. Cancellation is signalled
 * directly by the reader so prefix hashing never delays a CANCEL fence. */
internal class FileReceiveMachine(private val backend:ReceiveBackend,
    private val resumeCapable:Boolean=false,
    private val resumeBackend:suspend (ReceivedMetadata,()->Boolean)->ReceiveAdmission={meta,_->backend.admit(meta)},
    private val cancelPartial:suspend (String)->Unit={},
    private val legacyBackend:suspend (ReceivedMetadata,()->Boolean)->ReceiveAdmission={meta,_->backend.admit(meta)},
    private val peerId:String="unknown",
    private val failure:(Exception)->String=ReceiveErrors::code) {
    private data class Slot(val metadata:ReceivedMetadata,var sink:ReceiveSink?,var reply:ReceiveReply?,
        var received:Long=0,val resumable:Boolean=false,val digest:MessageDigest=MessageDigest.getInstance("SHA-256"))
    private var slot:Slot?=null
    private fun failureCode(metadata:ReceivedMetadata,phase:String,error:Exception):String {
        val code=failure(error)
        if(code=="storage_failed")ReceiveDiagnostics.warn(peerId,metadata.id,metadata.kind,phase,error)
        return code
    }
    private class Operation {
        @Volatile var cancelled=false
        @Volatile var failed=false
    }
    // One running operation plus the bounded receive lane's queued controls.
    private val operations=java.util.concurrent.ConcurrentHashMap<String,Operation>()
    @Synchronized fun register(id:String) {
        if(operations.containsKey(id) || operations.size>=12)throw ReceiveViolation()
        operations[id]=Operation()
    }
    fun isPending(id:String)=operations.containsKey(id)
    fun isCancelled(id:String)=operations[id]?.cancelled==true
    internal val pendingCount get()=operations.size
    fun requestFailure(id:String) {operations[id]?.failed=true}
    @Synchronized fun requestCancel(id:String):Boolean {
        val operation=operations[id] ?: return false
        if(operation.cancelled)return false
        operation.cancelled=true
        return true
    }
    @Synchronized private fun retire(id:String) {
        // A queued CANCEL owns retirement once the reader has fenced the job.
        if(!isCancelled(id))operations.remove(id)
    }
    suspend fun purgeCancelled(id:String)=cancelPartial(id)
    val active get()=slot!=null
    val metadata get()=slot?.takeIf {it.reply==null}?.metadata
    val receivedBytes get()=slot?.received ?: 0
    suspend fun begin(body:JsonObject):Frame.Control? {
        if(slot!=null)throw ReceiveViolation()
        val meta=try {ReceivedMetadata.parse(body)}catch(_:IllegalArgumentException) {throw ReceiveViolation("malformed_message")}
        val resume=body["resume"]?.asBoolean==true
        if((resume && !resumeCapable) || body.has("folder"))throw ReceiveViolation()
        if(!isPending(meta.id))register(meta.id)
        val operation=requireNotNull(operations[meta.id])
        if(operation.cancelled)throw ReceiveViolation()
        val admission=try {if(resume)resumeBackend(meta) {operation.cancelled} else legacyBackend(meta) {operation.cancelled}}
            catch(e:CancellationException) {throw e}
            catch(e:ReceiveViolation) {throw e}
            catch(e:Exception) {ReceiveAdmission(reply=ReceiveReply(failureCode(meta,"admit",e)))}
        if(operation.cancelled) {admission.sink?.abort();return null}
        check((admission.sink==null)!=(admission.reply==null))
        if(resume && admission.reply?.code!=null) {retire(meta.id);return admission.reply.frame(meta.id)}
        val offset=if(resume && admission.reply?.deduplicated==true)meta.size else admission.offset
        check(offset in 0..meta.size)
        slot=Slot(meta,admission.sink,admission.reply,offset,resume,admission.digest)
        return if(resume)Frame.Control(Type.FILE_READY,json("transfer_id" to meta.id,"offset" to offset)) else null
    }
    suspend fun chunk(frame:Frame.Chunk) {
        // Frames preceding CANCEL may already be queued on the file worker.
        if(isCancelled(frame.id) || operations[frame.id]?.failed==true)return
        val current=slot ?: throw ReceiveViolation()
        if(frame.id!=current.metadata.id || frame.offset!=current.received || frame.data.size !in 1..CHUNK_SIZE ||
            frame.data.size.toLong()>current.metadata.size-current.received)throw ReceiveViolation()
        current.received+=frame.data.size
        if(current.reply==null)current.sink?.let {sink->
            try {sink.write(frame.data);current.digest.update(frame.data)}
            catch(e:CancellationException) {throw e}
            catch(e:Exception) {
                current.reply=ReceiveReply(failureCode(current.metadata,"write",e))
                try {sink.abort()}catch(cancelled:CancellationException) {throw cancelled}catch(_:Exception) {}
            }
        }
    }
    suspend fun end(id:String):Frame.Control {
        if(isCancelled(id))throw ReceiveViolation("closing")
        val current=slot ?: throw ReceiveViolation()
        if(id!=current.metadata.id)throw ReceiveViolation()
        val sink=current.sink
        var preserve=false
        var phase="seal"
        try {
            val result=when {
                operations[id]?.failed==true->ReceiveReply("storage_failed")
                // READY(size) replay has no chunks and no live digest/sink.
                current.resumable && current.reply?.deduplicated==true->requireNotNull(current.reply)
                current.received!=current.metadata.size->ReceiveReply("size_mismatch")
                current.reply!=null->requireNotNull(current.reply)
                current.digest.digest().hex()!=current.metadata.hash->ReceiveReply("hash_mismatch")
                else->{requireNotNull(sink).seal();phase="commit";sink.commit()}
            }
            return result.frame(id)
        }catch(e:CancellationException) {preserve=true;throw e}
        catch(e:ReceiveViolation) {throw e}
        catch(e:Exception) {return ReceiveReply(failureCode(current.metadata,phase,e)).frame(id)}
        finally {
            slot=null;retire(id)
            try {if(preserve)sink?.disconnect() else sink?.abort()}catch(e:CancellationException) {throw e}catch(_:Exception) {}
        }
    }
    suspend fun cancel(id:String) {
        requestCancel(id)
        try {
            val current=slot?.takeIf {it.metadata.id==id}
            if(current!=null) {slot=null;current.sink?.abort()}
            cancelPartial(id)
        }finally {operations.remove(id)}
    }
    suspend fun disconnect() {
        val current=slot;slot=null
        try {if(current!=null) {if(isCancelled(current.metadata.id))current.sink?.abort() else current.sink?.disconnect()}}
        finally {operations.clear()}
    }
}
