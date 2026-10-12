package io.github.stantonmatt.pneu.features

import android.os.CancellationSignal
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import java.util.concurrent.*
import kotlin.coroutines.resumeWithException

/** Untrusted Binder providers get bounded workers and deadlines, never binding locks. */
object ProviderReads {
    private fun pool(n:Int,queued:Int=0)=ThreadPoolExecutor(n,n,30,TimeUnit.SECONDS,if(queued==0)SynchronousQueue<Runnable>() else ArrayBlockingQueue<Runnable>(queued),ThreadFactory {r->Thread(r,"Pneu-provider").apply {isDaemon=true}},ThreadPoolExecutor.AbortPolicy())
    private val workers=pool(4)
    private val copies=pool(4)
    private val previews=pool(2,2)
    private val previewSlots=Semaphore(2)
    private val cancels=pool(2)
    private val timer=ScheduledThreadPoolExecutor(1) {r->Thread(r,"Pneu-provider-deadline").apply {isDaemon=true}}
    suspend fun <T> call(timeout:Long=10000,discard:(T)->Unit={},body:(CancellationSignal)->T):T = execute(workers,timeout,discard,body)
    /** A blocked proxy/FUSE read can outlive cancellation. It consumes only a
     * bounded copy worker, never a metadata worker or an admission lock. */
    suspend fun <T> copy(timeout:Long=10000,body:(CancellationSignal,()->Unit)->T):T = execute(copies,timeout,{},body)
    fun closeLater(body:()->Unit) {runCatching {cancels.execute {runCatching(body)}}}
    /** Private preview work cannot consume slots reserved for untrusted providers.
     * Queue ordinary decodes as coroutines, so a busy Recent list loses no images. */
    suspend fun <T> preview(timeout:Long=2000,discard:(T)->Unit={},body:()->T):T = previewSlots.withPermit {
        execute(previews,timeout,discard) {_->body()}
    }
    private suspend fun <T> execute(executor:ThreadPoolExecutor,timeout:Long,discard:(T)->Unit,body:(CancellationSignal)->T):T = suspendCancellableCoroutine {continuation->
        val signal=CancellationSignal()
        val deadline=timer.schedule({continuation.cancel(java.io.IOException("Provider timeout"))},timeout,TimeUnit.MILLISECONDS)
        continuation.invokeOnCancellation {runCatching {cancels.execute {signal.cancel()}}}
        try {executor.execute worker@{
            try {
                if(!continuation.isActive)return@worker
                val value=body(signal)
                continuation.resume(value,onCancellation={_,v,_->discard(v)})
            } catch(e:Exception) {if(continuation.isActive)continuation.resumeWithException(e)}
            finally {deadline.cancel(false)}
        }} catch(e:RejectedExecutionException) {deadline.cancel(false);continuation.resumeWithException(java.io.IOException("Provider busy",e))}
    }
    private suspend fun <T> execute(executor:ThreadPoolExecutor,timeout:Long,discard:(T)->Unit,body:(CancellationSignal,()->Unit)->T):T = suspendCancellableCoroutine {continuation->
        val signal=CancellationSignal()
        val progress=CopyProgress(timeout)
        val deadline=timer.scheduleWithFixedDelay({
            if(progress.expired())continuation.cancel(java.io.IOException("Provider stalled"))
        },timeout,timeout.coerceAtMost(1000),TimeUnit.MILLISECONDS)
        continuation.invokeOnCancellation {deadline.cancel(false);closeLater {signal.cancel()}}
        try {executor.execute worker@{
            try {
                if(!continuation.isActive)return@worker
                val value=body(signal) {
                    if(!continuation.isActive)throw java.io.IOException("Copy abandoned")
                    progress.advance()
                }
                continuation.resume(value,onCancellation={_,v,_->discard(v)})
            } catch(e:Exception) {if(continuation.isActive)continuation.resumeWithException(e)}
            finally {deadline.cancel(false)}
        }} catch(e:RejectedExecutionException) {deadline.cancel(false);continuation.resumeWithException(java.io.IOException("Provider busy",e))}
    }
}
