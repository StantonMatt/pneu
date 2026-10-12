package io.github.stantonmatt.pneu.media

import android.Manifest
import android.content.ContentUris
import android.content.ContentResolver
import android.os.Bundle
import android.database.ContentObserver
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.provider.MediaStore
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.connection.Connectivity
import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.data.AdmissionToken
import io.github.stantonmatt.pneu.data.DeliveryBinding
import io.github.stantonmatt.pneu.data.MediaCatalog
import io.github.stantonmatt.pneu.protocol.*
import io.github.stantonmatt.pneu.features.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.combine
import java.io.File

/** Samsung image/video observation; stable media-ID mappings survive skip, ACK and discard. */
class ScreenshotObserver(private val app: AppGraph,private val cleanup:suspend ()->Unit={app.staging.cleanup()},
                         private val latest:suspend (Boolean)->Long={MediaCatalog.latest(app.context,it)},
                         private val connectivity:()->Connectivity={app.connection.connectivity()},
                         private val canRead:(Boolean)->Boolean={video->app.context.granted(if(video)Manifest.permission.READ_MEDIA_VIDEO else Manifest.permission.READ_MEDIA_IMAGES)}) {
    private val resolver=app.context.contentResolver
    private val changes=Channel<Unit>(Channel.CONFLATED)
    private var job: Job?=null
    private var pendingRows=false
    private val needsBaseline=mutableSetOf(false,true)
    private val observer=object:ContentObserver(Handler(Looper.getMainLooper())) {override fun onChange(selfChange: Boolean,uri: Uri?) {changes.trySend(Unit)}}
    fun start(scope: CoroutineScope) {
        if(job?.isActive==true)return
        val previous=job
        job=scope.launch(Dispatchers.IO) {
            previous?.join()
            val settings=launch {combine(app.prefs,app.work,app.mediaRefresh) {_,_,_->Unit}.collect {changes.trySend(Unit)}}
            var registered=false;var cleaned=false
            try {
                app.initialized.await()
                while(isActive) {
                    var failed=false
                    try {
                        if(!registered) {
                            for(collection in listOf(MediaStore.Images.Media.EXTERNAL_CONTENT_URI,MediaStore.Video.Media.EXTERNAL_CONTENT_URI,
                                MediaStore.Images.Media.getContentUri("external_primary"),MediaStore.Video.Media.getContentUri("external_primary")))resolver.registerContentObserver(collection,true,observer)
                            registered=true
                        }
                        if(!cleaned) {cleanup();cleaned=true}
                        scanOnce()
                    } catch(e:CancellationException) {throw e}
                    catch(e:Exception) {failed=true;android.util.Log.w("PneuMedia",e.javaClass.simpleName)
                        if(!registered)runCatching {resolver.unregisterContentObserver(observer)}
                    }
                    withTimeoutOrNull(if(failed)2000 else if(pendingRows)500 else 60000) {changes.receive();delay(50)}
                }
            } finally {settings.cancel();runCatching {resolver.unregisterContentObserver(observer)}}
        }
    }
    internal suspend fun scanOnce() {
        pendingRows=false
        val p=app.preferences.read()
        if(p.paused)return
        val binding=DeliveryBinding.from(p) ?: return
        for(video in listOf(false,true)) {
            if(!canRead(video)) {needsBaseline.add(video);continue}
            val collection=if(video)MediaStore.Video.Media.getContentUri("external_primary") else MediaStore.Images.Media.getContentUri("external_primary")
            val catalog=MediaCatalog.snapshot(app.context)
            val version=catalog.version;val generation=catalog.generation
            val token=AdmissionToken.from(p,video)
            val savedVersion=if(video)p.videoVersion else p.imageVersion
            val savedGeneration=if(video)p.videoGeneration else p.imageGeneration
            val pairId=if(video)p.videoPairId else p.imagePairId
            // A rebuilt catalog is a new ID namespace. Baseline its existing contents once.
            if(savedVersion!=version || video in needsBaseline) {
                if(app.checkpoint(binding,video,version,generation+1,latest(video),generation+1,token))needsBaseline.remove(video)
                return
            }
            var watermark=maxOf(generation,savedGeneration)
            val projection=arrayOf("_id","relative_path","is_pending","_display_name","mime_type","date_added","datetaken","generation_added","generation_modified")
            val query=Bundle().apply {
                putString(ContentResolver.QUERY_ARG_SQL_SELECTION,"(generation_added >= ? OR generation_modified >= ?) AND _id > ?")
                putStringArray(ContentResolver.QUERY_ARG_SQL_SELECTION_ARGS,arrayOf(savedGeneration.toString(),savedGeneration.toString(),pairId.toString()))
                putString(ContentResolver.QUERY_ARG_SQL_SORT_ORDER,"generation_added ASC, _id ASC")
                putInt(MediaStore.QUERY_ARG_MATCH_PENDING,MediaStore.MATCH_INCLUDE)
            }
            val cursorResult=ProviderReads.call(discard={it?.close()}) {signal->resolver.query(collection,projection,query,signal)} ?: continue
            cursorResult.use {cursor ->while(cursor.moveToNext()) {
                currentCoroutineContext().ensureActive()
                val relative=cursor.getString(1).orEmpty().lowercase()
                val kind=when {
                    !video && relative in listOf("dcim/screenshots/","pictures/screenshots/")->"screenshot"
                    !video && relative=="dcim/camera/"->"photo"
                    video && relative in listOf("dcim/screen recordings/","movies/screen recordings/")->"recording"
                    else->continue
                }
                val id="external_primary:$version:${if(video)"video" else "images"}:${cursor.getLong(0)}"
                if(app.queue.seen(id)!=0 || app.queue.find(id)!=null)continue
                if(cursor.getLong(7)<(if(video)p.videoFloor else p.imageFloor))continue
                val row=Outgoing().apply {mediaId=id;transferId=randomId();uri=ContentUris.withAppendedId(collection,cursor.getLong(0)).toString();this.kind=kind
                    fileName=cursor.getString(3) ?: if(video)"Screen recording.mp4" else "Image.png";mime=cursor.getString(4) ?: if(video)"video/mp4" else "image/png"
                    createdAt=cursor.getLong(6).takeIf {it>0} ?: cursor.getLong(5)*1000}
                val enabled=when(kind) {"photo"->p.photos && cursor.getLong(0)>p.photosAfterId;"recording"->p.recordings && cursor.getLong(0)>p.recordingsAfterId;else->p.screenshots && cursor.getLong(0)>p.screenshotsAfterId}
                if(DeliveryPolicy.skipAutomatic(p.paused,enabled,p.queue,app.connection.connected,connectivity(),p.mobile)) {
                    row.state="skipped";if(!app.enqueue(row,binding,token))return;continue
                }
                if(cursor.getInt(2)!=0) {pendingRows=true;watermark=minOf(watermark,maxOf(cursor.getLong(7),cursor.getLong(8)));continue}
                try {
                    if(!app.staging.capture(row,binding,token))return

                } catch(e: Exception) {if(e is CancellationException)throw e;watermark=minOf(watermark,maxOf(cursor.getLong(7),cursor.getLong(8)));android.util.Log.w("PneuMedia",e.javaClass.simpleName)}
            }}
            if(!app.checkpoint(binding,video,version,watermark,expected=token))return
        }
        // Connected is independent of observation readiness. Signal only after
        // both startup baselines and a current-policy scan have committed.
        if(needsBaseline.isEmpty())app.onMediaObserverReady()
    }
    fun stop() {job?.cancel();runCatching {resolver.unregisterContentObserver(observer)}}
}
