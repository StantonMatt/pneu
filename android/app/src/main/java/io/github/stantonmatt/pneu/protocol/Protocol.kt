package io.github.stantonmatt.pneu.protocol

import com.google.gson.*
import com.google.gson.stream.JsonReader
import com.google.gson.stream.JsonToken
import java.io.*
import java.math.BigInteger
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.security.MessageDigest
import java.security.SecureRandom

const val MAX_FILE = 68_719_476_736L
const val MAX_TIMESTAMP = 9_007_199_254_740_991L
const val CHUNK_SIZE = 262_144

enum class Type(val wire: Int) {
    HELLO(1), PAIR_REQUEST(2), PAIR_RESULT(3), PING(4), PONG(5), STATUS(6), UNPAIR(7),
    FILE_BEGIN(16), FILE_CHUNK(17), FILE_END(18), ACK(19), NACK(20), CLIPBOARD(32), PEER_SETTINGS(33), MEMBERS(34), ERROR(127);
    val label get() = name.lowercase()
}
sealed interface Frame {
    data class Control(val type: Type, val body: JsonObject) : Frame
    data class Chunk(val id: String, val offset: Long, val data: ByteArray) : Frame
}
fun json(vararg fields: Pair<String, Any>): JsonObject = JsonObject().apply {
    fields.forEach { (k, v) -> add(k, Gson().toJsonTree(v)) }
}
fun randomId(bytes: Int = 16): String = ByteArray(bytes).also { SecureRandom().nextBytes(it) }.hex()
fun ByteArray.hex(): String = joinToString("") { "%02x".format(it.toInt() and 255) }
fun String.unhex(): ByteArray { require(length % 2 == 0); return chunked(2).map { it.toInt(16).toByte() }.toByteArray() }
fun sha256(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256").digest(bytes).hex()
fun strictUtf8(bytes: ByteArray): String = Charsets.UTF_8.newDecoder()
    .onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT)
    .decode(ByteBuffer.wrap(bytes)).toString()

object Protocol {
    private val known = mapOf(
        Type.HELLO to setOf("version", "device_id", "name", "capabilities", "paused", "role", "addresses", "setup"),
        Type.PAIR_REQUEST to setOf("token"), Type.PAIR_RESULT to setOf("accepted", "code", "message"),
        Type.PING to setOf("nonce"), Type.PONG to setOf("nonce"), Type.STATUS to setOf("paused"), Type.UNPAIR to emptySet(),
        Type.FILE_BEGIN to setOf("transfer_id", "file_name", "mime", "size", "sha256", "kind", "created_at", "source_media_id", "hlc", "source_device"),
        Type.FILE_END to setOf("transfer_id"), Type.ACK to setOf("transfer_id", "deduplicated"),
        Type.NACK to setOf("transfer_id", "code", "retryable", "message"),
        Type.CLIPBOARD to setOf("transfer_id", "text", "created_at", "origin", "hlc", "source_device"),
        Type.PEER_SETTINGS to setOf("transfer_id", "settings"),
        Type.MEMBERS to setOf("transfer_id", "setup_id", "members"),
        Type.ERROR to setOf("code", "message", "supported_version")
    )
    val nackCodes = setOf("hash_mismatch", "size_mismatch", "storage_failed", "disk_full", "not_writable", "unsupported_kind", "id_conflict", "clipboard_unavailable")
    private val errors = setOf("version_mismatch", "unauthorized", "unexpected_message", "malformed_message", "frame_too_large", "unknown_message", "timeout", "busy", "connection_replaced", "closing")
    private val pairCodes = setOf("invalid_token", "expired_token", "pairing_closed", "already_paired", "storage_failed")
    private fun validateUnicode(value: String) {
        // Gson permits escaped unpaired surrogates; these are not Unicode strings.
        var i = 0
        while (i < value.length) {
            val c = value[i++]
            if (c.isHighSurrogate()) { require(i < value.length && value[i++].isLowSurrogate()) }
            else require(!c.isLowSurrogate())
        }
    }
    internal fun validateAddress(value: String) {
        val endpoint = Endpoint.parse(value)
        if (':' in endpoint.host) {
            // Java also permits overlong hex groups padded with leading zeroes.
            require(endpoint.host.split(':').all { '.' in it || it.length <= 4 })
        }
        if (':' in endpoint.host && '.' in endpoint.host) {
            // InetAddress accepts leading zeroes in an IPv6 embedded IPv4 tail,
            // unlike the protocol's IPv4 literal grammar.
            require(endpoint.host.substringAfterLast(':').split('.').all { it.length == 1 || !it.startsWith('0') })
        }
    }
    fun read(input: InputStream): Frame {
        val stream = DataInputStream(input)
        val length = stream.readInt().toLong() and 0xffffffffL
        require(length in 1..262169) { "Invalid frame length" }
        val wire = stream.readUnsignedByte()
        val type = Type.entries.firstOrNull { it.wire == wire }
        return readBody(stream, length.toInt(), type)
    }
    private fun readBody(stream: DataInputStream, length: Int, type: Type?): Frame {
        requireNotNull(type) { "Unknown message" }
        if (type == Type.FILE_CHUNK) {
            require(length in 26..262169)
            val id = ByteArray(16).also { stream.readFully(it) }.hex()
            val offset = stream.readLong(); require(offset in 0..MAX_FILE)
            require((length - 25).toLong() <= MAX_FILE - offset)
            return Frame.Chunk(id, offset, ByteArray(length - 25).also { stream.readFully(it) })
        }
        require(length <= 65537)
        val text = strictUtf8(ByteArray(length - 1).also { stream.readFully(it) })
        require(!text.startsWith('\uFEFF'))
        val reader = JsonReader(StringReader(text)).apply { strictness = Strictness.STRICT }
        require(reader.peek() == JsonToken.BEGIN_OBJECT)
        val result = readValue(reader, known.getValue(type)).asJsonObject
        require(reader.peek() == JsonToken.END_DOCUMENT)
        validate(type, result)
        return Frame.Control(type, result)
    }
    // Stream every object before Gson can discard duplicate fields. Unknown
    // values still get Unicode member-name validation recursively.
    private fun readValue(reader: JsonReader, fields: Set<String>? = null, context: String = ""): JsonElement = when(reader.peek()) {
        JsonToken.BEGIN_OBJECT -> JsonObject().apply {
            val seen=mutableSetOf<String>();reader.beginObject()
            while(reader.hasNext()) {
                val name=reader.nextName().also(::validateUnicode)
                require(fields==null || name !in fields || seen.add(name)) { "Duplicate field" }
                val nested=when(name.takeIf {fields!=null && name in fields}) {
                    "setup" -> SetupWire.helloFields
                    "reach" -> SetupWire.reachFields
                    "readmits" -> SetupWire.readmitsFields
                    else -> null
                }
                val value=readValue(reader,nested,if(fields!=null && name in fields)name else "")
                if(fields==null || name in fields)add(name,value)
            }
            reader.endObject()
        }
        JsonToken.BEGIN_ARRAY -> JsonArray().apply {
            reader.beginArray()
            while(reader.hasNext()) {
                val itemFields=when(context) {
                    "members" -> SetupWire.memberFields
                    "settings" -> setOf("key","enabled","hlc","writer_device")
                    else -> null
                }
                add(readValue(reader,itemFields))
            }
            reader.endArray()
        }
        else -> JsonParser.parseReader(reader)
    }
    fun write(output: OutputStream, frame: Frame) {
        val stream = DataOutputStream(output)
        when (frame) {
            is Frame.Chunk -> {
                require(frame.id.matches(Regex("[0-9a-f]{32}")) && frame.offset in 0..MAX_FILE && frame.data.size in 1..CHUNK_SIZE)
                require(frame.data.size.toLong() <= MAX_FILE - frame.offset)
                stream.writeInt(25 + frame.data.size); stream.writeByte(17); stream.write(frame.id.unhex()); stream.writeLong(frame.offset); stream.write(frame.data)
            }
            is Frame.Control -> {
                validate(frame.type, frame.body)
                val bytes = frame.body.toString().toByteArray(Charsets.UTF_8); require(bytes.size <= 65536)
                stream.writeInt(bytes.size + 1); stream.writeByte(frame.type.wire); stream.write(bytes)
            }
        }
        stream.flush()
    }
    fun validate(type: Type, o: JsonObject) {
        fun string(e: JsonElement?, max: Int, min: Int = 1): String {
            require(e is JsonPrimitive && e.isString) { "Missing/string value" }
            return e.asString.also { value ->
                require('\u0000' !in value && value.toByteArray(Charsets.UTF_8).size in min..max)
                validateUnicode(value)
            }
        }
        fun str(k: String, max: Int, min: Int = 1)=string(o.get(k),max,min)
        fun unsigned(e: JsonElement?, max: BigInteger): BigInteger {
            require(e is JsonPrimitive && e.isNumber && e.asString.matches(Regex("0|[1-9][0-9]*"))) { "Unsigned integer token required" }
            return e.asString.toBigInteger().also { require(it <= max) }
        }
        fun num(k: String, max: Long, min: Long = 0): Long =
            unsigned(o.get(k), BigInteger.valueOf(max)).longValueExact().also { require(it >= min) }
        fun bool(k: String): Boolean { val e = o.get(k); require(e is JsonPrimitive && e.isBoolean); return e.asBoolean }
        fun hex(k: String, n: Int) { require(str(k, n).matches(Regex("[0-9a-f]{$n}"))) }
        fun optional(k: String, f: () -> Unit) { if (o.has(k)) f() }
        fun hlc(e: JsonElement?) {
            // Do not narrow a wire u64 to signed Long or Double.
            unsigned(e, BigInteger("18446744073709551615"))
        }
        fun clipboardClock() {
            require(o.has("hlc") == o.has("source_device"))
            optional("hlc") { hlc(o.get("hlc"));hex("source_device",32) }
        }
        if (type in setOf(Type.PAIR_RESULT, Type.NACK, Type.ERROR)) optional("message") { str("message", 512, 0) }
        when (type) {
            Type.HELLO -> {
                num("version", 4294967295, 1); hex("device_id", 32); str("name", 128)
                val caps = o.get("capabilities"); require(caps is JsonArray && caps.size() <= 16)
                val names = caps.map { string(it,64) }
                require(names.distinct().size == names.size && ("clipboard_image" !in names || "files" in names))
                SetupWire.validateHello(o,names)
                optional("paused") { bool("paused") }
                optional("role") { require(str("role",8) in setOf("phone","computer")) }
                optional("addresses") {
                    val addresses=o.get("addresses");require(addresses is JsonArray && addresses.size() <= 8)
                    val names=addresses.map { string(it, 65536).also(::validateAddress) }
                    require(names.distinct().size == names.size)
                }
            }
            Type.PAIR_REQUEST -> hex("token", 64)
            Type.PAIR_RESULT -> if (bool("accepted")) require(!o.has("code") && !o.has("message")) else require(str("code", 64) in pairCodes)
            Type.PING, Type.PONG -> hex("nonce", 16)
            Type.STATUS -> bool("paused")
            Type.UNPAIR -> Unit
            Type.FILE_BEGIN -> {
                hex("transfer_id", 32); hex("sha256", 64); num("size", MAX_FILE); num("created_at", MAX_TIMESTAMP)
                val name = str("file_name", 255); require(name !in setOf(".", "..") && '/' !in name && '\\' !in name)
                val mime = str("mime", 255); val kind = str("kind", 32)
                require(kind in setOf("screenshot", "photo", "recording", "file", "clipboard_image"))
                if (kind == "clipboard_image") require(mime.startsWith("image/") && mime.length > 6)
                optional("source_media_id") { str("source_media_id", 256) }
                clipboardClock();require(!o.has("hlc") || (mime.startsWith("image/") && mime.length > 6))
            }
            Type.FILE_END -> hex("transfer_id", 32)
            Type.ACK -> { hex("transfer_id", 32); bool("deduplicated") }
            Type.NACK -> { hex("transfer_id", 32); val code = str("code", 64); require(code in nackCodes); require(bool("retryable") == (code !in setOf("unsupported_kind", "id_conflict"))) }
            Type.CLIPBOARD -> { hex("transfer_id", 32); str("text", 16384, 0); num("created_at", MAX_TIMESTAMP); optional("origin") { require(str("origin", 16) in setOf("clipboard", "share")) };clipboardClock() }
            Type.PEER_SETTINGS -> {
                hex("transfer_id",32)
                val settings=o.get("settings");require(settings is JsonArray && settings.size() in 1..3)
                val keys=mutableSetOf<String>()
                for(entry in settings) {
                    require(entry is JsonObject)
                    val key=string(entry.get("key"),81)
                    val parts=key.split('/')
                    require(parts.size == 3 && parts.take(2).all { it.matches(Regex("[0-9a-f]{32}")) } && parts[0] != parts[1])
                    require(parts[2] in setOf("clipboard","automatic_media") && keys.add(key))
                    val enabled=entry.get("enabled");require(enabled is JsonPrimitive && enabled.isBoolean)
                    hlc(entry.get("hlc"))
                    require(string(entry.get("writer_device"),32) in parts.take(2))
                }
            }
            Type.MEMBERS -> { SetupWire.decode(o) }
            Type.ERROR -> { val code = str("code", 64); require(code in errors); if (code == "version_mismatch") num("supported_version", 4294967295, 1) else require(!o.has("supported_version")) }
            Type.FILE_CHUNK -> error("Binary only")
        }
    }
}
