package io.github.stantonmatt.pneu.protocol

import com.google.gson.JsonParser
import org.junit.Assert.*
import org.junit.Test
import java.io.*

class ProtocolTest {
    private val vectors=JsonParser.parseReader(InputStreamReader(javaClass.getResourceAsStream("/v1.json")!!)).asJsonObject
    private fun decode(hex: String)=Protocol.read(ByteArrayInputStream(hex.unhex()))
    @Test fun everyValidFrameAndEncoderRoundTrip() {
        for(entry in vectors["frames"].asJsonArray) {
            val v=entry.asJsonObject; val decoded=v["decoded"].asJsonObject
            // Two legacy positive fixtures predate normative kind=file. Their original
            // bytes are intentionally rejected; exercise their migrated semantic frame too.
            if(decoded["type"].asString=="file_begin" && decoded["payload"].asJsonObject["kind"].asString=="shared") {
                assertTrue(runCatching {decode(v["frame_hex"].asString)}.isFailure)
                val migrated=decoded["payload"].asJsonObject.deepCopy().apply {addProperty("kind","file")}
                val bytes=ByteArrayOutputStream(); Protocol.write(bytes,Frame.Control(Type.FILE_BEGIN,migrated))
                assertEquals(migrated,(Protocol.read(ByteArrayInputStream(bytes.toByteArray())) as Frame.Control).body)
                continue
            }
            val frame=decode(v["frame_hex"].asString)
            when(frame) {
                is Frame.Control -> {assertEquals(v["name"].asString,decoded["type"].asString,frame.type.label);assertEquals(decoded["payload"],frame.body)}
                is Frame.Chunk -> {
                    val payload=decoded["payload"].asJsonObject
                    assertEquals(payload["transfer_id"].asString,frame.id);assertEquals(payload["offset"].asLong,frame.offset)
                    assertEquals(payload["data"].asString,frame.data.hex())
                }
            }
            val output=ByteArrayOutputStream();Protocol.write(output,frame)
            val copy=Protocol.read(ByteArrayInputStream(output.toByteArray()))
            if(frame is Frame.Control) assertEquals(frame,copy) else {
                copy as Frame.Chunk;frame as Frame.Chunk
                assertEquals(frame.id,copy.id);assertEquals(frame.offset,copy.offset);assertArrayEquals(frame.data,copy.data)
            }
        }
    }
    @Test fun everyInvalidFrameRejected() {
        for(entry in vectors["invalid_frames"].asJsonArray) {
            val v=entry.asJsonObject
            assertTrue(v["name"].asString,runCatching {decode(v["frame_hex"].asString)}.isFailure)
        }
    }
    @Test fun transferVectorsReconstructAndHash() {
        val frames=vectors["frames"].asJsonArray.associate { it.asJsonObject["name"].asString to it.asJsonObject["frame_hex"].asString }
        for(entry in vectors["transfers"].asJsonArray) {
            val v=entry.asJsonObject;val bytes=ByteArrayOutputStream()
            for(name in v["frames"].asJsonArray) {
                if(name.asString in setOf("file_begin","empty_begin")) continue
                val frame=decode(frames.getValue(name.asString))
                if(frame is Frame.Chunk) {assertEquals(bytes.size().toLong(),frame.offset);bytes.write(frame.data)}
            }
            assertEquals(v["data_hex"].asString,bytes.toByteArray().hex());assertEquals(v["size"].asLong,bytes.size().toLong());assertEquals(v["sha256"].asString,sha256(bytes.toByteArray()))
        }
    }
    @Test fun everyInvitationAndInvalidInvitation() {
        for(entry in vectors["pairing_uris"].asJsonArray) {
            val v=entry.asJsonObject;val parsed=Invitation.parse(v["uri"].asString);val expected=v["decoded"].asJsonObject
            assertEquals(expected["version"].asLong,parsed.version);assertEquals(expected["name"].asString,parsed.name)
            assertEquals(expected["fingerprint"].asString,parsed.fingerprint);assertEquals(expected["token"].asString,parsed.token)
            assertEquals(expected["expires_at"].asLong,parsed.expires);assertEquals(expected["addresses"].asJsonArray.map {it.asString},parsed.addresses.map {it.toString()})
        }
        for(entry in vectors["invalid_pairing_uris"].asJsonArray) {
            val v=entry.asJsonObject;assertTrue(v["name"].asString,runCatching {Invitation.parse(v["uri"].asString)}.isFailure)
        }
        for(entry in vectors["fingerprints"].asJsonArray) {val v=entry.asJsonObject;assertEquals(v["sha256"].asString,sha256(v["der_hex"].asString.unhex()))}
    }
    @Test fun newMessagesAndFields() {
        val id="01".repeat(16)
        for(frame in listOf(
            Frame.Control(Type.STATUS,json("paused" to true)),Frame.Control(Type.UNPAIR,json()),
            Frame.Control(Type.CLIPBOARD,json("transfer_id" to id,"text" to "","created_at" to 0,"origin" to "share")),
            Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "disk_full","retryable" to true)),
            Frame.Control(Type.NACK,json("transfer_id" to id,"code" to "not_writable","retryable" to true)),
            Frame.Control(Type.FILE_BEGIN,json("transfer_id" to id,"file_name" to "archive.zip","mime" to "application/zip","size" to 0,"sha256" to sha256(byteArrayOf()),"kind" to "file","created_at" to 0))
        )) {val bytes=ByteArrayOutputStream();Protocol.write(bytes,frame);assertEquals(frame,Protocol.read(ByteArrayInputStream(bytes.toByteArray())))}
        assertTrue(runCatching {Protocol.validate(Type.STATUS,json("paused" to "true"))}.isFailure)
        assertTrue(runCatching {Protocol.validate(Type.CLIPBOARD,json("transfer_id" to id,"text" to "x","created_at" to 0,"origin" to "other"))}.isFailure)
    }
    @Test fun fragmentedAndCoalescedReads() {
        val frames=vectors["frames"].asJsonArray.filter {it.asJsonObject["name"].asString !in setOf("file_begin","empty_begin")}.map {it.asJsonObject["frame_hex"].asString.unhex()}
        val bytes=frames.fold(ByteArrayOutputStream()) {o,b->o.apply {write(b)}}.toByteArray()
        val fragmented=object: FilterInputStream(ByteArrayInputStream(bytes)) {override fun read(b: ByteArray,off: Int,len: Int)=super.read(b,off,minOf(len,1))}
        repeat(frames.size) {Protocol.read(fragmented)};assertEquals(-1,fragmented.read())
    }
}
