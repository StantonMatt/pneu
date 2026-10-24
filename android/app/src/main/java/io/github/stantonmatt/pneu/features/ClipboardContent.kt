package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.net.Uri
import io.github.stantonmatt.pneu.protocol.sha256
import kotlinx.coroutines.*
import java.security.MessageDigest

/** Match the PC actor's exact UTF-8 / encoded-image byte fingerprints. A
 * received-content history or ownership marker alone is not current content. */
internal object ClipboardContent {
    fun textMatches(clip:ClipData?,text:String)=clip!=null && clip.itemCount>0 &&
        clip.getItemAt(0).text?.toString()==text

    internal data class ImageSnapshot(val uri:Uri,val hash:String) {
        fun matches(clip:ClipData?,hash:String)=this.hash==hash && clip!=null && clip.itemCount>0 &&
            clip.getItemAt(0).uri==uri
    }
    /** Provider work stays outside binding/policy locks. The effect boundary
     * must still check that this representation remains on the OS clipboard. */
    suspend fun imageSnapshot(context:Context):ImageSnapshot? {
        val uri=withContext(Dispatchers.Main) {
            runCatching {context.getSystemService(ClipboardManager::class.java).primaryClip}
                .getOrNull()?.takeIf {it.itemCount>0}?.getItemAt(0)?.uri
        } ?: return null
        return try {
            val hash=ProviderReads.call(timeout=2000) {signal->
                context.contentResolver.openInputStream(uri)?.use {input->
                    val digest=MessageDigest.getInstance("SHA-256");val buffer=ByteArray(65536)
                    while(true) {
                        signal.throwIfCanceled()
                        val count=input.read(buffer);if(count<0)break
                        digest.update(buffer,0,count)
                    }
                    digest.digest().joinToString("") {"%02x".format(it)}
                }
            } ?: return null
            ImageSnapshot(uri,hash)
        }catch(e:CancellationException) {throw e}
        catch(_:Exception) {null}
    }
    fun imageMatches(clip:ClipData?,hash:String,snapshot:ImageSnapshot?):Boolean {
        if(clip==null || clip.itemCount==0)return false
        val text=clip.getItemAt(0).text?.toString()
        return text!=null && sha256(text.toByteArray())==hash || snapshot?.matches(clip,hash)==true
    }
}
