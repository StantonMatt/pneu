package io.github.stantonmatt.pneu.features

import android.system.ErrnoException
import android.system.OsConstants
import java.io.IOException

internal object ReceiveErrors {
    fun code(error:Exception):String {
        val causes=generateSequence<Throwable>(error) {it.cause}.toList()
        val errno=causes.filterIsInstance<ErrnoException>().firstOrNull()?.errno
        return when {
            errno==OsConstants.ENOSPC || errno==OsConstants.EDQUOT || causes.any {it is android.database.sqlite.SQLiteFullException}->"disk_full"
            error is ReceiveStorageException->error.code
            errno in setOf(OsConstants.EACCES,OsConstants.EPERM,OsConstants.EROFS) || causes.any {it is SecurityException}->"not_writable"
            else->"storage_failed"
        }
    }
}
internal class ReceiveStorageException(val code:String):IOException(code)
internal object ReceivePolicy {
    private val mime=Regex("[A-Za-z0-9!#$&^_.+*-]+/[A-Za-z0-9!#$&^_.+*-]+")
    // Phone/computer v1 snapshots have an automatic-media switch only for
    // phone -> computer. Never apply that outbound switch to received photos.
    fun automaticEnabled(peer:io.github.stantonmatt.pneu.data.Peer,phoneId:String)=
        peer.settings["${peer.deviceId}/$phoneId/automatic_media"]?.enabled ?: true
    fun acceptsMime(value:String)=mime.matches(value) && '*' !in value
    fun newTransfer(kind:String,paused:Boolean,clipboard:Boolean,automatic:Boolean,mesh:Boolean):ReceiveReply? {
        // Pause stops automatic media. Explicit files still save; a paused
        // phone suppresses a clipboard image's optional OS effect separately.
        val disabled=kind=="clipboard_image" && !clipboard || kind in setOf("screenshot","photo","recording") && (paused || !automatic)
        return if(!disabled)null else if(mesh)ReceiveReply("unsupported_kind") else ReceiveReply()
    }
    fun storageName(value:String):String {
        val clean=value.map {if(it.isISOControl() || it in "/\\" || it in '\u202a'..'\u202e' || it in '\u2066'..'\u2069')'_' else it}.joinToString("").trim().trimStart('.')
        return clean.ifBlank {"file"}
    }
    fun folder(mime:String)=when {mime.startsWith("image/",ignoreCase=true)->"Pictures/Pneu/";mime.startsWith("video/",ignoreCase=true)->"Movies/Pneu/";else->"Download/Pneu/"}
}
