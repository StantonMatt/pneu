package io.github.stantonmatt.pneu.protocol

import com.google.gson.*
import org.junit.Assert.*
import org.junit.Test
import java.io.*

class WireAuditProtocolTest {
    private data class Control(val name: String, val type: Type, val body: JsonObject)
    private val controls = JsonParser.parseReader(InputStreamReader(javaClass.getResourceAsStream("/v1.json")!!))
        .asJsonObject["frames"].asJsonArray.map { it.asJsonObject }.filter { it["decoded"].asJsonObject["type"].asString != "file_chunk" }
        .map { entry ->
            val decoded = entry["decoded"].asJsonObject
            Control(entry["name"].asString, Type.entries.single { it.label == decoded["type"].asString }, decoded["payload"].asJsonObject)
        }
    private fun accepts(type: Type, payload: String): Boolean = runCatching {
        val bytes = payload.toByteArray(Charsets.UTF_8)
        val output = ByteArrayOutputStream()
        DataOutputStream(output).apply { writeInt(bytes.size + 1); writeByte(type.wire); write(bytes) }
        Protocol.read(ByteArrayInputStream(output.toByteArray()))
    }.isSuccess
    private fun sameType(a: JsonElement, b: JsonElement): Boolean = when {
        a is JsonArray -> b is JsonArray
        a is JsonObject -> b is JsonObject
        a is JsonPrimitive && b is JsonPrimitive -> a.isBoolean == b.isBoolean && a.isString == b.isString && a.isNumber == b.isNumber
        else -> false
    }
    @Test fun everyKnownFieldRejectsWrongTypesNullAndDuplicates() {
        val covered = mutableMapOf<Type, MutableSet<String>>()
        val nested = mutableSetOf<String>()
        for ((name, type, payload) in controls) {
            assertTrue(name, accepts(type, payload.toString()))
            val objects = mutableListOf(false to payload)
            if (type == Type.PEER_SETTINGS) objects.addAll(payload["settings"].asJsonArray.map { true to it.asJsonObject })
            for ((isNested, obj) in objects) {
                fun wrap(value: JsonObject): JsonObject = if (isNested) payload.deepCopy().apply {
                    add("settings", JsonArray().apply { add(value) })
                } else value
                for ((field, value) in obj.entrySet()) {
                    if (isNested) nested.add(field) else covered.getOrPut(type) { mutableSetOf() }.add(field)
                    val optional = !isNested && when (type) {
                        Type.HELLO -> field in setOf("paused", "role", "addresses")
                        Type.PAIR_RESULT, Type.NACK, Type.ERROR -> field == "message"
                        Type.FILE_BEGIN -> field in setOf("source_media_id","sensitive","folder") || (field=="resume" && !payload.has("folder"))
                        Type.NOTIFICATION_EVENT -> field in setOf("title","text","icon_png")
                        Type.CLIPBOARD -> field in setOf("origin","sensitive")
                        else -> false
                    }
                    val missing = obj.deepCopy().apply { remove(field) }
                    assertEquals("missing $name.$field", optional, accepts(type, wrap(missing).toString()))
                    for (token in listOf("null", "true", "0", "\"x\"", "[]", "{}")) {
                        val wrong = JsonParser.parseString(token)
                        if (sameType(value, wrong)) continue
                        val changed = obj.deepCopy().apply { add(field, wrong) }
                        assertFalse("$name.$field: $wrong", accepts(type, wrap(changed).toString()))
                    }
                    val escaped = "\"\\u%04x%s\"".format(field[0].code, field.drop(1))
                    for (key in listOf("\"$field\"", escaped)) {
                        val duplicate = obj.toString().dropLast(1) + ",$key:$value}"
                        val text = if (isNested) "{\"transfer_id\":${payload["transfer_id"]},\"settings\":[$duplicate]}" else duplicate
                        assertFalse("duplicate $name.$field", accepts(type, text))
                    }
                }
                val separator = if (obj.size() == 0) "" else ","
                val unknown = obj.toString().dropLast(1) + separator + "\"future\":{\"paused\":null,\"hlc\":[true]},\"future\":[null,1,\"x\"]}"
                val text = if (isNested) "{\"transfer_id\":${payload["transfer_id"]},\"settings\":[$unknown]}" else unknown
                assertTrue("unknown fields in $name", accepts(type, text))
            }
        }
        val expected = mapOf(
            Type.HELLO to "version device_id name capabilities paused role addresses",
            Type.PAIR_REQUEST to "token", Type.PAIR_RESULT to "accepted code message",
            Type.PING to "nonce", Type.PONG to "nonce", Type.STATUS to "paused",
            Type.FILE_BEGIN to "transfer_id file_name mime size sha256 kind created_at source_media_id hlc source_device sensitive resume folder",
            Type.FILE_END to "transfer_id", Type.ACK to "transfer_id deduplicated",
            Type.NACK to "transfer_id code retryable message",
            Type.CLIPBOARD to "transfer_id text created_at origin hlc source_device sensitive",
            Type.FILE_READY to "transfer_id offset", Type.CANCEL to "transfer_id",
            Type.BATTERY_STATE to "level charging", Type.RING to "action", Type.RING_STATE to "ringing",
            Type.NOTIFICATION_EVENT to "action key app_package app_label time title text icon_png",
            Type.PEER_SETTINGS to "transfer_id settings", Type.ERROR to "code message supported_version"
        )
        for ((type, fields) in expected) assertEquals(type.label, fields.split(' ').toSet(), covered[type])
        assertEquals(setOf("key", "enabled", "hlc", "writer_device"), nested)
    }
    @Test fun unknownFieldsIgnoreValuesWithoutValidatingThemAsKnownFields() {
        for (value in listOf("null", "1e9999", "18446744073709551616", "\"\\ud800\"", "[".repeat(140) + "0" + "]".repeat(140))) {
            for (key in listOf("future", "future\\ud83d\\ude00")) assertTrue("$key: $value", accepts(Type.UNPAIR, "{\"$key\":$value}"))
        }
    }
    @Test fun integerTokensAndBoundsForEveryNumericField() {
        data class Numeric(val fixture: String, val field: String, val min: String, val max: String)
        for ((fixture, field, min, max) in listOf(
            Numeric("hello_phone", "version", "1", "4294967295"),
            Numeric("empty_begin", "size", "0", "68719476736"),
            Numeric("empty_begin", "created_at", "0", "9007199254740991"),
            Numeric("clipboard_image_mesh", "hlc", "0", "18446744073709551615"),
            Numeric("clipboard_clear", "created_at", "0", "9007199254740991"),
            Numeric("clipboard_mesh", "hlc", "0", "18446744073709551615"),
            Numeric("peer_settings", "hlc", "0", "18446744073709551615"),
            Numeric("error_version_mismatch", "supported_version", "1", "4294967295")
        )) {
            val control = controls.single { it.name == fixture }
            fun raw(token: String): String {
                val body = control.body.deepCopy()
                val target = if (control.type == Type.PEER_SETTINGS) body["settings"].asJsonArray[0].asJsonObject else body
                target.addProperty(field, "NUMBER_TOKEN")
                return body.toString().replace("\"NUMBER_TOKEN\"", token)
            }
            for (token in listOf(min, "1", max)) assertTrue("$fixture.$field: $token", accepts(control.type, raw(token)))
            for (token in listOf("-0", "-1", "1.0", "1e0", "1E+0", "\"1\"", "01", "+1", (max.toBigInteger() + java.math.BigInteger.ONE).toString())) {
                assertFalse("$fixture.$field: $token", accepts(control.type, raw(token)))
            }
            if (min == "1") assertFalse(accepts(control.type, raw("0")))
        }
    }
    @Test fun stringConstraintsAndArrayElements() {
        for ((name, type, payload) in controls) {
            val objects = mutableListOf(false to payload)
            if (type == Type.PEER_SETTINGS) objects.addAll(payload["settings"].asJsonArray.map { true to it.asJsonObject })
            for ((nested, obj) in objects) for ((field, value) in obj.entrySet()) {
                fun wrap(changed: JsonObject) = if (nested) payload.deepCopy().apply { add("settings", JsonArray().apply { add(changed) }) } else changed
                fun check(replacement: JsonElement, valid: Boolean) {
                    val changed = obj.deepCopy().apply { add(field, replacement) }
                    assertEquals("$name.$field: $replacement", valid, accepts(type, wrap(changed).toString()))
                }
                fun text(replacement: String, valid: Boolean) = check(JsonPrimitive(replacement), valid)
                if (value is JsonPrimitive && value.isString) {
                    text("\u0000", false)
                    val changed = obj.deepCopy().apply { addProperty(field, "SURROGATE_TOKEN") }
                    for (token in listOf("\"\\ud800\"", "\"\\udc00\"", "\"\\udc00\\ud800\"")) {
                        assertFalse("$name.$field: $token", accepts(type, wrap(changed).toString().replace("\"SURROGATE_TOKEN\"", token)))
                    }
                    val digits = when (field) {
                        "device_id", "transfer_id", "source_device", "writer_device" -> 32
                        "sha256", "token" -> 64
                        "nonce" -> 16
                        else -> null
                    }
                    if (digits != null) for (bad in listOf("a".repeat(digits - 1), "a".repeat(digits + 1), "A".repeat(digits), "g".repeat(digits))) text(bad, false)
                    val bounds = when (field) {
                        "name" -> 1 to 128
                        "file_name" -> 1 to 255
                        "mime" -> if(!payload.has("folder")) 1 to 255 else null
                        "app_package" -> 1 to 255
                        "app_label" -> 1 to 128
                        "title" -> 0 to 1024
                        "source_media_id" -> 1 to 256
                        "message" -> 0 to 512
                        "text" -> 0 to (if(type==Type.NOTIFICATION_EVENT)8192 else 16384)
                        else -> null
                    }
                    if (bounds != null) {
                        val (min, max) = bounds
                        val prefix = if (field == "mime") "image/" else ""
                        text(if (min == 0) "" else prefix + "x", true)
                        val remaining = max - prefix.length
                        val atLimit = prefix + "é".repeat(remaining / 2) + "x".repeat(remaining % 2)
                        text(atLimit, true)
                        text(atLimit + "x", false)
                        if (min == 1) text("", false)
                    }
                    if (field == "file_name") for (bad in listOf(".", "..", "a/b", "a\\b")) text(bad, false)
                } else if (value is JsonArray) {
                    for (wrong in listOf("null", "true", "0", "[]", "{}")) check(JsonParser.parseString("[$wrong]"), false)
                    when (field) {
                        "capabilities" -> {
                            for (n in listOf(16, 17)) check(JsonArray().apply { repeat(n) { add("cap$it") } }, n == 16)
                            check(JsonArray().apply { add("é".repeat(32)) }, true)
                            check(JsonArray().apply { add("é".repeat(32) + "x") }, false)
                            for (bad in listOf("", "\u0000", "clipboard_image")) check(JsonArray().apply { add(bad) }, false)
                            check(JsonArray().apply { add("files"); add("files") }, false)
                        }
                        "addresses" -> {
                            check(JsonArray(), true)
                            for (n in listOf(8, 9)) check(JsonArray().apply { repeat(n) { add("pc$it.local:1") } }, n == 8)
                        }
                        else -> {
                            assertEquals("settings", field)
                            check(JsonArray(), false)
                            check(JsonArray().apply { repeat(4) { add(value[0].deepCopy()) } }, false)
                        }
                    }
                }
            }
        }
    }
    @Test fun helloAddressGrammar() {
        val control = controls.single { it.name == "hello_mesh_computer" }
        val maxHost = listOf("a".repeat(63), "b".repeat(63), "c".repeat(63), "d".repeat(61)).joinToString(".")
        val valid = listOf("host:1", "host:65535", "HOST.local:00001", "192.0.2.1:1", "[::1]:1", "[2001:db8::1]:1", "[::ffff:192.0.2.1]:1", "[::192.0.2.1]:1")
        val invalid = listOf("host:0", "host:65536", "host:+1", "host: 1", "host", "host.:1", "-host:1", "host_:1", "user@host:1", "host/path:1", "123:1", "999.1.1.1:1", "01.2.3.4:1", "[127.0.0.1]:1", "::1:1", "[fe80::1%eth0]:1", "[1::2::3]:1", "[00000::1]:1", "[::ffff:192.168.001.1]:1", "[::192.168.001.1]:1", "[::ffff:1.2.3]:1", "[::1]:1junk", "höst:1", " host:1")
        for ((address, expected) in valid.map { it to true } + invalid.map { it to false } + listOf("$maxHost:1" to true, "${maxHost}x:1" to false, "${"x".repeat(64)}.local:1" to false)) {
            val body = control.body.deepCopy().apply { add("addresses", JsonArray().apply { add(address) }) }
            assertEquals(address, expected, accepts(control.type, body.toString()))
        }
    }
}
