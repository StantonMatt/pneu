package io.github.stantonmatt.pneu.features

import android.net.Uri
import android.content.Intent
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.flow.collect
import com.google.gson.Gson
import io.github.stantonmatt.pneu.AppGraph
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.CapturedAuthority
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.first
import java.io.File
import java.util.concurrent.ConcurrentHashMap

internal fun Outgoing.isFolder()=origin=="folder" && mime==FOLDER_MIME
internal fun Outgoing.folderSummary():FolderSummary?=if(isFolder())runCatching {Gson().fromJson(text,FolderSummary::class.java)}.getOrNull() else null
internal class FolderUnavailable(val device:String):java.io.IOException()
internal object FolderGate {
    fun needsUpdate(capabilities:Set<String>,connected:Boolean)=
        connected && !capabilities.containsAll(setOf("files",CAP_FILE_RESUME,CAP_FOLDER))
}
internal data class FolderPreparing(val row:Outgoing,val count:Int=0,val done:Long=0,val total:Long?=null,val name:String=row.fileName)
/** Persistable access lives through preparation and all pending attempts. */
internal class FolderSends(private val app:AppGraph) {
    private val mutable=MutableStateFlow<List<FolderPreparing>>(emptyList())
    val preparing=mutable.asStateFlow()
    private val grants=Mutex()
    private val preparingUris=mutableMapOf<String,String>()
    private val grantStore=app.context.getSharedPreferences("folder-grants",0)
    init {app.scope.launch(Dispatchers.IO) {
        app.initialized.await()
        app.queue.folderGrantChanges().collect {reconcileGrants()}
    }}
    private suspend fun acquireGrant(id:String,uri:Uri)=grants.withLock {
        val resolver=app.context.contentResolver
        val owned=grantStore.getStringSet("owned",emptySet()).orEmpty().toMutableSet()
        // Record intent first, so interrupted preparation is cleaned on restart.
        if(resolver.persistedUriPermissions.none {it.uri==uri && it.isReadPermission}) {
            owned.add(uri.toString());check(grantStore.edit().putStringSet("owned",owned).commit())
        }
        resolver.takePersistableUriPermission(uri,Intent.FLAG_GRANT_READ_URI_PERMISSION)
        preparingUris[id]=uri.toString()
    }
    private suspend fun reconcileGrants(finished:String?=null)=grants.withLock {
        if(finished!=null)preparingUris.remove(finished)
        val active=app.queue.activeFolderUris().toSet()+preparingUris.values
        val owned=grantStore.getStringSet("owned",emptySet()).orEmpty().toMutableSet()
        for(value in owned.toList())if(value !in active) {
            try {app.context.contentResolver.releasePersistableUriPermission(Uri.parse(value),Intent.FLAG_GRANT_READ_URI_PERMISSION)}catch(_:SecurityException) {}
            owned.remove(value)
        }
        check(grantStore.edit().putStringSet("owned",owned).commit())
    }
    private val jobs=ConcurrentHashMap<String,Job>()
    private fun update(body:(List<FolderPreparing>)->List<FolderPreparing>)=synchronized(mutable) {mutable.value=body(mutable.value)}
    fun cancel(id:String) {
        val job=jobs[id] ?: return
        val row=mutable.value.find {it.row.mediaId==id}?.row
        job.cancel()
        // Cancellation can race the atomic queue commit. Retire an admitted
        // delivery too, through the same CANCEL path as a sending row.
        if(row!=null)app.scope.launch {
            job.join()
            val current=app.queue.delivery(id,row.peerId) ?: return@launch
            app.discard(io.github.stantonmatt.pneu.data.ConfirmedQueue.from(app.preferences.read(),listOf(current)))
        }
    }
    suspend fun send(uri:Uri,authority:CapturedAuthority) {
        val binding=authority.binding
        val p=app.preferences.read()
        require(authority.stillValid(p,app.connection.generation(binding))) {"Pairing changed"}
        val peer=requireNotNull(p.devices.find {it.deviceId==binding.pcId})
        val state=app.connection.devices.value[binding.pcId]
        if(FolderGate.needsUpdate(state?.capabilities.orEmpty(),app.connection.connected(binding.pcId)))throw FolderUnavailable(peer.name)
        val row=Outgoing().apply {mediaId="folder:${randomId()}";transferId=randomId();kind="file";origin="folder";mime=FOLDER_MIME;automatic=false;createdAt=System.currentTimeMillis();fileName=app.context.getString(R.string.folder_name);recipients=listOf(binding);peerId=binding.pcId;bindingGeneration=binding.generation;this.state="preparing"}
        val job=currentCoroutineContext().job;jobs[row.mediaId]=job
        val authorityWatch=CoroutineScope(currentCoroutineContext()).launch {
            combine(app.prefs,app.connection.devices) {prefs,_->app.recipientCurrent(binding,prefs)}.first {!it}
            job.cancel(CancellationException("Folder recipient changed"))
        }
        try {
            acquireGrant(row.mediaId,uri)
            update {it+FolderPreparing(row)}
            val tree=SafFolderTree(app.context,uri)
            row.fileName=safeFilename(tree.stat(tree.rootId).name)
            update {rows->rows.map {if(it.row.mediaId==row.mediaId)it.copy(name=row.fileName) else it}}
            var last=0L
            val capture=FolderBuilder.prepare(tree,tree.rootId) {count,done,total->
                val now=android.os.SystemClock.elapsedRealtime()
                if(now-last>=250 || total!=null && done>=total) {
                    last=now;update {rows->rows.map {if(it.row.mediaId==row.mediaId)FolderPreparing(row,count,done,total) else it}}
                }
            }
            row.size=capture.size;row.hash=capture.hash;row.sourceHash=capture.hash;row.text=Gson().toJson(capture.summary);row.uri=uri.toString();row.state="waiting"
            currentCoroutineContext().ensureActive()
            check(app.staging.admitFolder(row,capture,binding)) {"Pairing changed"}
            app.connection.kick()
        }finally {authorityWatch.cancel();jobs.remove(row.mediaId);update {rows->rows.filterNot {it.row.mediaId==row.mediaId}};withContext(NonCancellable+Dispatchers.IO) {reconcileGrants(row.mediaId)}}
    }
    fun open(row:Outgoing,cancelled:()->Boolean={false}):FolderStream {
        val file=File(row.path)
        require(file.length() in 1..(32L*1024*1024))
        val capture=FolderCapture.decode(file.readText())
        require(capture.size==row.size && capture.hash==row.hash && capture.summary==row.folderSummary())
        return FolderStream(SafFolderTree(app.context,Uri.parse(row.uri)),capture,cancelled)
    }
}
