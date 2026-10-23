package io.github.stantonmatt.pneu.features

import com.google.gson.JsonObject
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.CancellationException
import java.io.IOException
import java.security.MessageDigest

/** All known wire fields, before storage-name sanitizing. Unknown extensions do not
 * participate in receipt equality. Optional fields retain absent/present identity. */
data class ReceivedMetadata(val id:String,val name:String,val mime:String,val size:Long,val hash:String,
    val kind:String,val createdAt:Long,val mediaId:String?=null,val hlc:String?=null,val source:String?=null) {
    val canonical:String get()=json("transfer_id" to id,"file_name" to name,"mime" to mime,"size" to size,
        "sha256" to hash,"kind" to kind,"created_at" to createdAt).apply {
        mediaId?.let {addProperty("source_media_id",it)}
        hlc?.let {addProperty("hlc",it.toBigInteger());addProperty("source_device",source)}
    }.toString()
    companion object {
        fun parse(body:JsonObject):ReceivedMetadata {
            Protocol.validate(Type.FILE_BEGIN,body)
            return ReceivedMetadata(body["transfer_id"].asString,body["file_name"].asString,body["mime"].asString,
                body["size"].asLong,body["sha256"].asString,body["kind"].asString,body["created_at"].asLong,
                body["source_media_id"]?.asString,body["hlc"]?.asString?.toBigInteger()?.toString(),body["source_device"]?.asString)
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
}
internal data class ReceiveAdmission(val sink:ReceiveSink?=null,val reply:ReceiveReply?=null)
internal fun interface ReceiveBackend { suspend fun admit(metadata:ReceivedMetadata):ReceiveAdmission }

/** One slot per authenticated connection (v1 permits no pipelined BEGINs).
 * No chunk queue: the read loop waits for each write, bounding memory to one
 * protocol frame and applying TCP backpressure. Different peers use separate slots. */
internal class FileReceiveMachine(private val backend:ReceiveBackend,private val failure:(Exception)->String=ReceiveErrors::code) {
    private data class Slot(val metadata:ReceivedMetadata,var sink:ReceiveSink?,var reply:ReceiveReply?,
        var received:Long=0,val digest:MessageDigest=MessageDigest.getInstance("SHA-256"))
    private var slot:Slot?=null
    val active get()=slot!=null
    val metadata get()=slot?.takeIf {it.reply==null}?.metadata
    val receivedBytes get()=slot?.received ?: 0
    suspend fun begin(body:JsonObject) {
        if(slot!=null)throw ReceiveViolation()
        val meta=try {ReceivedMetadata.parse(body)}catch(_:IllegalArgumentException) {throw ReceiveViolation("malformed_message")}
        val admission=try {backend.admit(meta)}catch(e:CancellationException) {throw e}
            catch(e:ReceiveViolation) {throw e}
            catch(e:Exception) {ReceiveAdmission(reply=ReceiveReply(failure(e)))}
        slot=Slot(meta,admission.sink,admission.reply)
        check((admission.sink==null)!=(admission.reply==null))
    }
    suspend fun chunk(frame:Frame.Chunk) {
        val current=slot ?: throw ReceiveViolation()
        if(frame.id!=current.metadata.id || frame.offset!=current.received || frame.data.size !in 1..CHUNK_SIZE ||
            frame.data.size.toLong()>current.metadata.size-current.received)throw ReceiveViolation()
        current.received+=frame.data.size
        if(current.reply==null)current.sink?.let {sink->
            try {sink.write(frame.data);current.digest.update(frame.data)}
            catch(e:CancellationException) {throw e}
            catch(e:Exception) {
                current.reply=ReceiveReply(failure(e))
                // Keep the drain slot even if cleanup itself is temporarily unavailable.
                try {sink.abort()}catch(cancelled:CancellationException) {throw cancelled}catch(_:Exception) {}
            }
        }
    }
    suspend fun end(id:String):Frame.Control {
        val current=slot ?: throw ReceiveViolation()
        if(id!=current.metadata.id)throw ReceiveViolation()
        val sink=current.sink
        try {
            val result=when {
                current.received!=current.metadata.size->ReceiveReply("size_mismatch")
                current.reply!=null->requireNotNull(current.reply)
                current.digest.digest().hex()!=current.metadata.hash->ReceiveReply("hash_mismatch")
                else->{requireNotNull(sink).seal();sink.commit()}
            }
            return result.frame(id)
        }catch(e:CancellationException) {throw e}
        catch(e:ReceiveViolation) {throw e}
        catch(e:Exception) {return ReceiveReply(failure(e)).frame(id)}
        finally {
            slot=null
            try {sink?.abort()}catch(e:CancellationException) {throw e}catch(_:Exception) {}
        }
    }
    suspend fun disconnect() {
        val current=slot;slot=null
        current?.sink?.abort()
    }
}
