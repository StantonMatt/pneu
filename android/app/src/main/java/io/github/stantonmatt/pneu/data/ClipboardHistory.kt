package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.JsonObject
import com.google.gson.JsonParser

/** Ownership survives clipboard-history restores that strip ClipDescription extras.
 * Hashes and sources share one eviction order and one atomic preferences edit. */
internal object ClipboardHistory {
    private val hashes=stringPreferencesKey("recentClipboardHashes")
    private val sources=stringPreferencesKey("recentClipboardSources")
    private val lastHash=stringPreferencesKey("lastReceivedHash")
    private val lastSource=stringPreferencesKey("lastReceivedSource")
    const val LIMIT=64
    fun read(p:androidx.datastore.preferences.core.Preferences):Map<String,String> {
        val saved=p[sources]?.let {JsonParser.parseString(it).asJsonObject}
        val history=linkedMapOf<String,String>()
        p[hashes].orEmpty().split('|').filter(String::isNotEmpty).forEach {hash->history[hash]=saved?.get(hash)?.asString.orEmpty()}
        // Preserve the latest known legacy source on upgrade. Older hash-only
        // entries have unknown ownership until that content is received again.
        p[lastHash]?.takeIf(String::isNotEmpty)?.let {hash->history[hash]=history[hash].orEmpty().ifEmpty {p[lastSource].orEmpty()}}
        return history.entries.toList().takeLast(LIMIT).associate {it.toPair()}
    }
    fun remember(p:MutablePreferences,hash:String,source:String) {
        val history=read(p).toMutableMap().apply {remove(hash);put(hash,source)}.entries.toList().takeLast(LIMIT)
        p[hashes]=history.joinToString("|") {it.key}
        p[sources]=JsonObject().apply {history.forEach {addProperty(it.key,it.value)}}.toString()
        p[lastHash]=hash;p[lastSource]=source
    }
}
