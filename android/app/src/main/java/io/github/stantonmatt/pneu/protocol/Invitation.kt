package io.github.stantonmatt.pneu.protocol

import java.io.ByteArrayOutputStream
import java.net.InetAddress
import java.net.URI

data class Endpoint(val host: String, val port: Int) {
    val tailscale: Boolean get() {
        val address=literalAddress(host)?.address ?: return false
        return if(address.size==4) address[0].toInt() and 255 == 100 && (address[1].toInt() and 255) in 64..127
        else address.size==16 && address.take(6).map {it.toInt() and 255} == listOf(0xfd,0x7a,0x11,0x5c,0xa1,0xe0)
    }
    override fun toString(): String = if (':' in host) "[$host]:$port" else "$host:$port"
    companion object {
        /** Numeric literals only: never ask DNS to classify a hostname. Java normalizes mapped IPv6. */
        fun literalAddress(host:String):InetAddress? {
            val ipv4=host.split('.')
            val valid4=ipv4.size==4 && ipv4.all {it.isNotEmpty() && it.length<=3 && it.all {c->c in '0'..'9'} && it.toInt() in 0..255 && (it.length==1 || !it.startsWith('0'))}
            val valid6=':' in host && host.all {it in "0123456789abcdefABCDEF:."}
            return if(valid4 || valid6)runCatching {InetAddress.getByName(host)}.getOrNull() else null
        }
        fun parse(value: String): Endpoint {
            require(value.isNotEmpty() && value.all { it.code in 33..126 } && '%' !in value)
            val host: String; val p: String
            if (value.startsWith('[')) {
                val close = value.indexOf(']'); require(close > 1 && value.getOrNull(close + 1) == ':')
                host = value.substring(1, close); p = value.substring(close + 2)
                require(':' in host && host.all { it in "0123456789abcdefABCDEF:." }); InetAddress.getByName(host)
            } else {
                require(value.count { it == ':' } == 1)
                host = value.substringBefore(':'); p = value.substringAfter(':')
                if (host.all { it.isDigit() || it == '.' }) {
                    val parts = host.split('.'); require(parts.size == 4 && parts.all { it.isNotEmpty() && it.length <= 3 && it.all(Char::isDigit) && it.toInt() in 0..255 && (it.length == 1 || !it.startsWith('0')) })
                } else {
                    require(host.length in 1..253 && host.split('.').all { it.matches(Regex("[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?")) })
                }
            }
            require(p.matches(Regex("[0-9]+")))
            return Endpoint(host, p.toInt().also { require(it in 1..65535) })
        }
    }
}
data class Invitation(val version: Long, val name: String, val fingerprint: String, val addresses: List<Endpoint>, val token: String, val expires: Long) {
    companion object {
        fun parse(raw: String): Invitation {
            require(raw.toByteArray().size <= 8192)
            val uri = URI(raw)
            require(uri.scheme == "pneu" && uri.rawAuthority == "pair" && uri.rawPath.isNullOrEmpty() && uri.rawFragment == null)
            val fields = linkedMapOf<String, MutableList<String>>()
            for (pair in requireNotNull(uri.rawQuery).split('&')) {
                require('=' in pair)
                val key = decode(pair.substringBefore('=')); val value = decode(pair.substringAfter('='))
                require(key in setOf("v", "name", "fp", "addr", "token", "exp"))
                fields.getOrPut(key) { mutableListOf() }.add(value)
            }
            fun one(key: String): String = requireNotNull(fields[key]).also { require(it.size == 1) }.single()
            fun number(key: String, max: Long, min: Long): Long = one(key).also { require(it.matches(Regex("[0-9]+"))) }.toLong().also { require(it in min..max) }
            fun hash(key: String): String = one(key).also { require(it.matches(Regex("[0-9a-f]{64}"))) }
            val name = one("name").also { require(it.toByteArray().size in 1..128 && '\u0000' !in it) }
            val addresses = requireNotNull(fields["addr"]).also { require(it.size in 1..8 && it.distinct().size == it.size) }.map(Endpoint::parse)
            return Invitation(number("v", 4294967295, 1), name, hash("fp"), addresses, hash("token"), number("exp", MAX_TIMESTAMP, 0))
        }
        private fun decode(value: String): String {
            val bytes = ByteArrayOutputStream(); var i = 0
            while (i < value.length) {
                val c = value[i++]
                when (c) {
                    '%' -> { require(i + 1 < value.length); bytes.write(value.substring(i, i + 2).toInt(16)); i += 2 }
                    '+' -> bytes.write(32)
                    else -> { require(c.code < 128); bytes.write(c.code) }
                }
            }
            return strictUtf8(bytes.toByteArray())
        }
    }
}
