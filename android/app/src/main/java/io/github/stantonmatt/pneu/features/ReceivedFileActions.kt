package io.github.stantonmatt.pneu.features

import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.lifecycle.lifecycleScope
import io.github.stantonmatt.pneu.graph
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.ReceivedFile
import kotlinx.coroutines.*

/** Stored MediaStore IDs are references, not lasting grants. Every tap (including
 * a cached PendingIntent) resolves its exact journal identity before granting a
 * viewer access. This Activity is internal and never accepts a raw external URI. */
class ReceivedFileActionActivity:ComponentActivity() {
    override fun onCreate(savedInstanceState:Bundle?) {
        super.onCreate(savedInstanceState)
        lifecycleScope.launch {
            try {
                UiActions.feedback(this@ReceivedFileActionActivity).run(getString(R.string.toast_cant_read)) {
                    ReceivedFileActions.open(this@ReceivedFileActionActivity,ReceivedFileActions.expected(intent),intent.getBooleanExtra("share",false))
                }
            }finally {finish()}
        }
    }
}
internal object ReceivedFileActions {
    suspend fun open(context:Context,expected:ReceivedFile?,share:Boolean,launch:(Intent)->Unit={context.startActivity(it)}) {
        val resolved=checkNotNull(resolve(context,requireNotNull(expected),share)) {"Received file unavailable"}
        launch(Intent.createChooser(resolved,null))
    }
    fun intent(context:Context,row:ReceivedFile,share:Boolean=false)=Intent(context,ReceivedFileActionActivity::class.java)
        .setIdentifier("received:${if(share)"share" else "open"}:${row.peerId}:${row.transferId}:${row.generation}")
        .putExtra("peer",row.peerId).putExtra("id",row.transferId).putExtra("generation",row.generation)
        .putExtra("catalog",row.catalogVersion).putExtra("uri",row.uri).putExtra("metadata",row.metadata).putExtra("share",share)
    fun expected(intent:Intent):ReceivedFile?=runCatching {
        ReceivedFile().apply {
            peerId=requireNotNull(intent.getStringExtra("peer"));transferId=requireNotNull(intent.getStringExtra("id"))
            require(intent.hasExtra("generation"));generation=intent.getLongExtra("generation",-1)
            catalogVersion=requireNotNull(intent.getStringExtra("catalog"));uri=requireNotNull(intent.getStringExtra("uri"))
            metadata=requireNotNull(intent.getStringExtra("metadata"))
        }
    }.getOrNull()
    fun published(storage:ReceiveStorage,row:ReceivedFile):Boolean {
        val uri=Uri.parse(row.uri)
        if(row.state!="complete" || uri.scheme!="content" || uri.authority!="media")return false
        return row.catalogVersion==storage.version() && storage.pending(row.uri)==false && row.catalogVersion==storage.version()
    }
    suspend fun resolve(context:Context,expected:ReceivedFile,share:Boolean,storage:ReceiveStorage=ReceivedMediaStore(context)):Intent?=try {
        ProviderReads.call {
            val row=context.graph.queue.receivedFile(expected.peerId,expected.transferId)
            if(!io.github.stantonmatt.pneu.data.QueueDao.sameReceivedFile(row,expected) || !published(storage,row))return@call null
            publicIntent(row,share)
        }
    }catch(e:CancellationException) {throw e}catch(_:Exception) {null}
    internal fun publicIntent(row:ReceivedFile,share:Boolean)=
        (if(share)Intent(Intent.ACTION_SEND).setType(row.mime).putExtra(Intent.EXTRA_STREAM,Uri.parse(row.uri))
         else Intent(Intent.ACTION_VIEW).setDataAndType(Uri.parse(row.uri),row.mime))
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            .apply {clipData=ClipData.newRawUri(row.fileName,Uri.parse(row.uri))}
}
