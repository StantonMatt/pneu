package io.github.stantonmatt.pneu.protocol

import com.google.gson.JsonParser
import org.junit.Assert.*
import org.junit.Test
import java.io.*

class KnownStringTest {
    private fun hello(capability:String)=json("version" to 1,"device_id" to "a".repeat(32),"name" to "PC","capabilities" to listOf(capability))
    private fun readHello(escaped:String):Frame {
        val raw="""{"version":1,"device_id":"${"a".repeat(32)}","name":"PC","capabilities":["$escaped"]}""".toByteArray()
        val wire=ByteArrayOutputStream().apply {DataOutputStream(this).apply {writeInt(raw.size+1);writeByte(Type.HELLO.wire);write(raw)}}
        return Protocol.read(ByteArrayInputStream(wire.toByteArray()))
    }
    @Test fun malformedCapabilitySurrogatesFailOnReadAndWrite() {
        for(escaped in listOf("\\ud800","\\udc00","\\udc00\\ud800","\\ud800x","\\ud800\\ud800")) {
            assertTrue(escaped,runCatching {readHello(escaped)}.isFailure)
            val value=JsonParser.parseString("\"$escaped\"").asString
            val output=ByteArrayOutputStream()
            assertTrue(escaped,runCatching {Protocol.write(output,Frame.Control(Type.HELLO,hello(value)))}.isFailure)
            assertEquals("Validation precedes writing the frame",0,output.size())
        }
    }
    @Test fun validPairsUseUtf8ByteBoundsAndUnknownCapabilitiesRemainAccepted() {
        assertEquals(listOf("😀"),(readHello("\\ud83d\\ude00") as Frame.Control).body["capabilities"].asJsonArray.map {it.asString})
        for(value in listOf("future_capability","😀".repeat(16))) {
            val frame=Frame.Control(Type.HELLO,hello(value));val output=ByteArrayOutputStream()
            Protocol.write(output,frame);assertEquals(frame,Protocol.read(ByteArrayInputStream(output.toByteArray())))
        }
        for(value in listOf("😀".repeat(17),"","nul\u0000value"))assertTrue(runCatching {Protocol.validate(Type.HELLO,hello(value))}.isFailure)
    }
    @Test fun namedKnownStringsShareTheSameUnicodeValidation() {
        val fields=listOf(
            Type.HELLO to (hello("files") to "name"),
            Type.CLIPBOARD to (json("transfer_id" to "a".repeat(32),"text" to "ok","created_at" to 0) to "text"),
            Type.ERROR to (json("code" to "closing","message" to "ok") to "message"),
            Type.FILE_BEGIN to (json("transfer_id" to "a".repeat(32),"sha256" to "b".repeat(64),"size" to 0,"created_at" to 0,"file_name" to "ok","mime" to "image/png","kind" to "file","source_media_id" to "ok") to "source_media_id")
        )
        for((type,payload) in fields) {
            val (body,field)=payload
            for(value in listOf("\ud800","\udc00","\udc00\ud800","\ud800x")) {
                body.addProperty(field,value);assertTrue(field,runCatching {Protocol.validate(type,body)}.isFailure)
            }
            body.addProperty(field,"😀");Protocol.validate(type,body)
        }
    }
}
