package io.github.stantonmatt.pneu.protocol

import com.google.gson.*
import io.github.stantonmatt.pneu.features.*
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import java.io.*

class ExtensionProtocolTest {
    private val vectors=JsonParser.parseReader(InputStreamReader(javaClass.getResourceAsStream("/v1.json")!!)).asJsonObject
    private fun begin(bytes:ByteArray,extra:Long=0)=json("transfer_id" to "0".repeat(32),"file_name" to "Tree",
        "mime" to FOLDER_MIME,"size" to bytes.size+extra,"sha256" to "0".repeat(64),"kind" to "file",
        "created_at" to 0,"resume" to true,"folder" to json("manifest_len" to bytes.size))
    private fun accepts(bytes:ByteArray,extra:Long=0)=runCatching {FolderManifest.parse(bytes,begin(bytes,extra))}.isSuccess
    @Test fun typedModelsEncodeTheSharedWireFieldNamesAndActions() {
        val classes=mapOf("file_ready" to FileReady::class.java,"cancel" to Cancel::class.java,
            "battery_state" to BatteryState::class.java,"ring" to Ring::class.java,
            "ring_state" to RingState::class.java,"notification_event" to NotificationEvent::class.java)
        val gson=Gson()
        for(e in vectors["frames"].asJsonArray) {
            val v=e.asJsonObject;val decoded=v["decoded"].asJsonObject
            val model=classes[decoded["type"].asString] ?: continue
            val body=decoded["payload"].asJsonObject
            val encoded=gson.toJsonTree(gson.fromJson(body,model)).asJsonObject
            assertEquals(v["name"].asString,body,encoded)
            Protocol.validate(Type.entries.single {it.label==decoded["type"].asString},encoded)
        }
        for(e in vectors["folder_manifests"].asJsonArray) {
            val v=e.asJsonObject
            if(v["valid"].asBoolean) {
                val raw=JsonParser.parseString(v["manifest"].asString)
                val bytes=v["manifest"].asString.toByteArray(Charsets.UTF_8)
                assertEquals(FolderManifest.parse(bytes,begin(bytes,v["stream_size"].asLong-bytes.size)),gson.fromJson(raw,FolderManifest::class.java))
            }
        }
    }
    @Test fun sharedFolderManifests() {
        for(e in vectors["folder_manifests"].asJsonArray) {
            val v=e.asJsonObject;val bytes=v["manifest_hex"]?.asString?.unhex() ?: v["manifest"].asString.toByteArray(Charsets.UTF_8)
            val b=begin(bytes).apply {addProperty("size",v["stream_size"].asLong);add("folder",json("manifest_len" to v["manifest_len"].asLong))}
            assertEquals(v["name"].asString,v["valid"].asBoolean,runCatching {FolderManifest.parse(bytes,b)}.isSuccess)
        }
    }
    @Test fun pathStructureAndLimits() {
        for(good in listOf(".pneu-staging","CON","xx:ads","%2e%2e","A/empty","a/empty","é"))validateFolderPath(good)
        for(bad in listOf("","/a","a/","a//b",".","..","a/../b","a\\b","a\u0000b","C:","C:/x","x/D:file","//server/share","\\\\?\\C:\\x"))
            assertTrue(bad,runCatching {validateFolderPath(bad)}.isFailure)
        for(n in listOf(255,256))assertEquals(n==255,runCatching {validateFolderPath("a".repeat(n))}.isSuccess)
        assertTrue(runCatching {validateFolderPath("é".repeat(127))}.isSuccess)
        assertTrue(runCatching {validateFolderPath("é".repeat(128))}.isFailure)
        for(n in listOf(32,33))assertEquals(n==32,runCatching {validateFolderPath(List(n){"x"}.joinToString("/"))}.isSuccess)
        val path=List(16){"a".repeat(254)}.joinToString("/")+"/"+"b".repeat(16)
        assertEquals(4096,path.length);validateFolderPath(path)
        assertTrue(runCatching {validateFolderPath(path+"b")}.isFailure)
    }
    @Test fun manifestTypesIntegersCountsAndEncodedBytes() {
        for(raw in listOf("[]","{\"entries\":null}","{\"entries\":[[]]}",
            """{"entries":[{"path":"x","path":"y","type":"directory"}]}""",
            """{"entries":[{"path":"x","type":"directory","size":null}]}""",
            """{"entries":[{"path":"x","type":"directory","sha256":null}]}""",
            """{"entries":[{"path":"\ud800","type":"directory"}]}"""))assertFalse(raw,accepts(raw.toByteArray()))
        for(token in listOf("-0","0.0","0e0","null","68719476737")) {
            val raw="""{"entries":[{"path":"x","type":"file","size":$token,"sha256":"${sha256(byteArrayOf())}"}]}"""
            assertFalse(token,accepts(raw.toByteArray()))
        }
        val empty="""{"entries":[]}""".toByteArray()
        assertTrue(accepts(empty));assertFalse(accepts(empty,1));assertFalse(accepts(empty+0xff.toByte()))
        assertFalse(runCatching {FolderManifest.parse(empty,begin(empty).apply {add("folder",json("manifest_len" to empty.size-1))})}.isSuccess)
        for(n in listOf(10000,10001)) {
            val raw=json("entries" to (0 until n).map {json("path" to "$it","type" to "directory")}).toString().toByteArray()
            assertEquals(n==10000,accepts(raw))
        }
        val prefix="""{"entries":[],"future":"""" // final quote begins unknown string
        val raw=(prefix+"x".repeat(MAX_MANIFEST_BYTES-prefix.length-2)+"\"}").toByteArray()
        assertEquals(MAX_MANIFEST_BYTES,raw.size);assertTrue(accepts(raw));assertFalse(accepts(raw+32.toByte()))
    }
    @Test fun entryLimitRejectsBeforeDeserializingExcessEntry() {
        val entries=(0 until MAX_MANIFEST_ENTRIES).joinToString(",") {"{\"path\":\"$it\",\"type\":\"directory\"}"}
        val raw=("{\"entries\":["+entries+",false]}").toByteArray()
        val error=runCatching {FolderManifest.parse(raw,begin(raw))}.exceptionOrNull()
        assertEquals("manifest entry count",error?.message)
    }
    @Test(timeout=15_000) fun largeUnknownArrayIsConsumedWithinABoundedTime() {
        // Almost 8 MiB / four million primitive values must not become Gson nodes.
        val raw=("{\"entries\":[],\"future\":["+"0,".repeat((MAX_MANIFEST_BYTES-32)/2)+"0]}").toByteArray()
        assertTrue(raw.size <= MAX_MANIFEST_BYTES)
        assertEquals(emptyList<FolderEntry>(),FolderManifest.parse(raw,begin(raw)).entries)
    }
    @Test fun sharedContextualReadyRejections() {
        for(e in vectors["invalid_ready_offsets"].asJsonArray) {
            val v=e.asJsonObject
            val begin=vectors["frames"].asJsonArray.single {it.asJsonObject["name"].asString==v["begin"].asString}.asJsonObject["decoded"].asJsonObject["payload"].asJsonObject
            val ready=JsonParser.parseString(v["payload"].asString).asJsonObject
            assertTrue(runCatching {FileReady(ready["transfer_id"].asString,ready["offset"].asLong).validateOffset(begin["size"].asLong)}.isFailure)
        }
    }
    @Test fun checkedSumsAndContextualReadiness() {
        val file=FolderEntry("x",FolderEntryType.FILE,MAX_FILE-1,"0".repeat(64))
        FolderManifest(listOf(file)).validate(1,MAX_FILE)
        assertTrue(runCatching {FolderManifest(listOf(file.copy(size=MAX_FILE))).validate(1,MAX_FILE)}.isFailure)
        assertTrue(runCatching {FolderManifest(listOf(file.copy(size=Long.MAX_VALUE))).validate(1,MAX_FILE)}.isFailure)
        FileReady("0".repeat(32),4).validateOffset(4)
        assertTrue(runCatching {FileReady("0".repeat(32),4).validateOffset(3)}.isFailure)
        for((caps,valid) in listOf(listOf(CAP_FILE_RESUME) to false,listOf("files",CAP_FILE_RESUME) to true,
            listOf("files",CAP_FOLDER) to false,listOf("files",CAP_FILE_RESUME,CAP_FOLDER) to true)) {
            val h=json("version" to 1,"device_id" to "0".repeat(32),"name" to "X","capabilities" to caps)
            assertEquals(valid,runCatching {Protocol.validate(Type.HELLO,h)}.isSuccess)
        }
    }
    @Test fun unchangedHelloAndLegacyBeginWireAndUnnegotiatedGate()=runBlocking {
        assertEquals(listOf("files","clipboard_text","clipboard_image","mesh","setup"),LEGACY_PHONE_CAPABILITIES)
        val b=json("transfer_id" to "0".repeat(32),"file_name" to "x","mime" to "text/plain","size" to 3,
            "sha256" to sha256("abc".toByteArray()),"kind" to "file","created_at" to 0)
        val metadata=ReceivedMetadata.parse(b)
        for(resume in listOf(false,true)) {
            val changed=b.deepCopy().apply {addProperty("resume",resume);addProperty("future",true)}
            assertEquals(metadata,ReceivedMetadata.parse(changed));assertEquals(metadata.canonical,ReceivedMetadata.parse(changed).canonical)
        }
        val folder=begin("""{"entries":[]}""".toByteArray())
        assertNotEquals(ReceivedMetadata.parse(folder),ReceivedMetadata.parse(folder.deepCopy().apply {add("folder",json("manifest_len" to 13))}))
        var admitted=false
        val receiver=FileReceiveMachine(ReceiveBackend {admitted=true;ReceiveAdmission(reply=ReceiveReply())})
        try {receiver.begin(b.deepCopy().apply {addProperty("resume",true)});fail("Unnegotiated resume")}
        catch(e:ReceiveViolation) {assertEquals("unexpected_message",e.code)}
        assertFalse(admitted);assertFalse(receiver.active)
        receiver.begin(b);assertTrue(admitted)
        receiver.chunk(Frame.Chunk("0".repeat(32),0,"abc".toByteArray()))
        assertEquals(Type.ACK,receiver.end("0".repeat(32)).type)
    }
}
