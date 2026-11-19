package io.github.stantonmatt.pneu.data

import android.icu.lang.UCharacter
import java.text.Normalizer

/** Shared by both groups, retaining the full URL and the original row text. */
internal fun historySearchKey(value:String):String = UCharacter.foldCase(
    Normalizer.normalize(value,Normalizer.Form.NFD).replace(Regex("\\p{M}+"),"")
        .replace(Regex("[\\s\\p{Z}]+")," ").trim(),true)

internal fun historyMatches(item:SavedClip,query:String):Boolean {
    val key=historySearchKey(query)
    return key.isEmpty() || item.image.isEmpty() && historySearchKey(item.text).contains(key)
}
