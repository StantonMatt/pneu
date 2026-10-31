package io.github.stantonmatt.pneu.protocol

import com.google.gson.*
import org.junit.Test
import org.junit.Assert.*
import java.io.*

class SetupProtocolTest {
    private val vectors=JsonParser.parseReader(InputStreamReader(javaClass.getResourceAsStream("/setup-membership-vectors.json")!!)).asJsonObject
    private fun frame(payload:String,type:Type=Type.MEMBERS):ByteArray=ByteArrayOutputStream().also {out->DataOutputStream(out).apply {
        val bytes=payload.toByteArray();writeInt(bytes.size+1);writeByte(type.wire);write(bytes)
    }}.toByteArray()
    private fun read(payload:String,type:Type=Type.MEMBERS)=Protocol.read(ByteArrayInputStream(frame(payload,type))) as Frame.Control
    private val join get()=vectors["vectors"].asJsonArray[0].asJsonObject["payload"].asString
    private fun rejects(payload:String,type:Type=Type.MEMBERS) {assertThrows(Exception::class.java) {read(payload,type)}}
    private fun hello(setup:String,caps:String="\"mesh\",\"setup\"",role:String=",\"role\":\"computer\"")=
        """{"version":1,"device_id":"${"2".repeat(32)}","name":"PC","capabilities":[$caps]$role,"setup":$setup}"""
    @Test fun literalJsonVectorsAndExactFramePrefixes() {
        for(raw in vectors["vectors"].asJsonArray) {
            val v=raw.asJsonObject;val payload=v["payload"].asString;val bytes=frame(payload)
            assertEquals(v["name"].asString,v["prefix"].asString,bytes.take(5).toByteArray().hex())
            val decoded=read(payload);assertEquals(Type.MEMBERS,decoded.type)
            val snapshot=SetupWire.decode(decoded.body)
            val round=ByteArrayOutputStream();Protocol.write(round,decoded)
            assertEquals(snapshot,SetupWire.decode((Protocol.read(round.toByteArray().inputStream()) as Frame.Control).body))
        }
    }
    @Test fun sharedReadmissionPayloadsUseTheExistingWireCodec() {
        val shared=JsonParser.parseReader(javaClass.getResourceAsStream("/readmit-vectors.json")!!.reader()).asJsonObject
        for((name,fixture) in shared.getAsJsonObject("fixtures").entrySet()) {
            val payload=fixture.asJsonObject["payload"].toString()
            assertEquals(name,SetupWire.decode(read(payload).body),SetupWire.decode(JsonParser.parseString(payload).asJsonObject))
        }
    }
    @Test fun literalInvalidNestedVectorsAreRejectedBeforeMapParsing() {
        val invalid=vectors["invalid"].asJsonArray.map {it.asString}
        for(entry in invalid.take(2))rejects("""{"transfer_id":"${"4".repeat(32)}","setup_id":"${"0".repeat(32)}","members":[$entry]}""")
        rejects(hello(invalid[2]),Type.HELLO)
    }
    @Test fun helloSetupCompatibilityVectorsAndCapabilityDependencies() {
        for(raw in vectors["compatibility"].asJsonArray)for(side in listOf("left","right")) {
            val value=raw.asJsonObject[side].asJsonObject
            assertEquals(SetupWire.hello(value),SetupWire.hello(read(hello(value.toString()),Type.HELLO).body["setup"]))
        }
        rejects(hello("{\"scope\":\"enabled\"}","\"setup\""),Type.HELLO)
        rejects(hello("{\"scope\":\"enabled\"}",role=""),Type.HELLO)
        rejects(hello("null"),Type.HELLO)
        rejects(hello("{\"scope\":\"disabled\",\"setup_id\":\"${"0".repeat(32)}\"}"),Type.HELLO)
        rejects(hello("{\"scope\":\"ask\",\"scope\":\"enabled\"}"),Type.HELLO)
        rejects(hello("{\"scope\":\"unknown\"}"),Type.HELLO)
        read(hello("{\"scope\":\"enabled\",\"future\":true}","\"mesh\",\"setup\",\"unknown_capability\""),Type.HELLO)
    }
    @Test fun unsignedHlcUsesAll64BitsAndRejectsNonIntegerTokens() {
        for(value in listOf("0","9223372036854775808","18446744073709551615")) {
            val body=read(join.replace("1800000000001",value)).body
            assertEquals(value,SetupWire.decode(body).members.first().hlc)
        }
        for(value in listOf("18446744073709551616","-0","-1","1.0","1e0","null","\"1\""))rejects(join.replace("1800000000001",value))
    }
    @Test fun nestedDuplicatesUnknownFieldsUnicodeAndUtf8Limits() {
        rejects(join.replace("\"relay\":[]","\"relay\":[],\"relay\":[]"))
        rejects(join.replace("\"name\":\"Laptop\"","\"name\":\"Laptop\",\"name\":\"Other\""))
        val body=JsonParser.parseString(join).asJsonObject
        fun named(name:String)=body.deepCopy().apply {getAsJsonArray("members")[2].asJsonObject.addProperty("name",name)}.toString()
        read(named("é".repeat(64)));rejects(named("é".repeat(65)));rejects(named("x\u0000"));rejects(join.replace("Laptop","\\ud800"))
        val unknown=join.replace("\"name\":\"Laptop\"","\"name\":\"Laptop\",\"future\":{\"ignored\":1},\"future\":2")
        assertEquals(SetupWire.decode(read(join).body),SetupWire.decode(read(unknown).body))
        rejects(join.replace("\"name\":\"Laptop\"","\"name\":\"Laptop\",\"future\":{\"\\ud800\":1}"))
        assertThrows(Exception::class.java) {Protocol.read((frame(join).dropLast(1).toByteArray()+byteArrayOf(0xc0.toByte())).inputStream())}
    }
    @Test fun membershipStructuralLimitsAndReachGrammar() {
        val base=SetupWire.decode(read(join).body)
        val self=base.members[0]
        fun snapshot(entries:List<SetupMember>)=base.copy(members=entries).json().toString()
        val many=(1..17).map {n->self.copy(deviceId=n.toString(16).padStart(32,'0'),fingerprint=n.toString(16).padStart(64,'0'))}
        read(snapshot(many));rejects(snapshot(many+many[0].copy(deviceId="f".repeat(32),fingerprint="f".repeat(64))))
        val total=(1..64).map {n->self.copy(deviceId=n.toString(16).padStart(32,'0'),fingerprint=n.toString(16).padStart(64,'0'),status="removed")}
        read(snapshot(total));rejects(snapshot(total+total[0].copy(deviceId="e".repeat(32))))
        rejects(snapshot(listOf(self,self)))
        rejects(snapshot(listOf(self,self.copy(fingerprint="d".repeat(64)))))
        val removed=(1..4).map {self.copy(fingerprint=it.toString(16).padStart(64,'0'),status="removed")}
        read(snapshot(removed));rejects(snapshot(removed+removed[0].copy(fingerprint="f".repeat(64))))
        rejects(join.replace("\"relay\":[]","\"relay\":[{}]"))
        rejects(join.replace("\"writer_device\":\"${"3".repeat(32)}\"","\"writer_device\":\"${"1".repeat(32)}\""))
        rejects(join.replace("192.168.1.12:47821","https://host:47821"))
        rejects(join.replace("192.168.1.12:47821","[00000::1]:47821"))
        rejects(join.replace("\"role\":\"computer\",\"name\":\"Laptop\"","\"role\":\"phone\",\"name\":\"Laptop\""))
        rejects(join.replace("\"status\":\"member\"","\"status\":null"))
        rejects(join.replace("\"setup_id\":\"${"0".repeat(32)}\"","\"setup_id\":null"))
    }
    @Test fun encodedPayloadBoundaryIsCheckedBeforeAllocation() {
        val padded=join.dropLast(1)+",\"x\":\""+"x".repeat(65536-join.toByteArray().size-7)+"\"}"
        assertEquals(65536,padded.toByteArray().size);read(padded)
        rejects(padded.replace("\"x\":\"","\"x\":\"x"))
        val header=ByteArrayOutputStream().also {DataOutputStream(it).apply {writeInt(65538);writeByte(Type.MEMBERS.wire)}}
        assertThrows(IllegalArgumentException::class.java) {Protocol.read(header.toByteArray().inputStream())}
    }
}
