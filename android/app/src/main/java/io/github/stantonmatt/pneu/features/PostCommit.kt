package io.github.stantonmatt.pneu.features

/** Maintenance is outside the admission/receipt transaction. Its failure must
 * never roll back durable work or turn successful admission into a failed send. */
internal object PostCommit {
    fun maintain(body:()->Unit) {try {body()} catch(e:Exception) {failed(e)}}
    suspend fun maintainSuspending(body:suspend ()->Unit) {
        try {body()} catch(e:kotlinx.coroutines.CancellationException) {throw e}
        catch(e:Exception) {failed(e)}
    }
    private fun failed(e:Exception) {runCatching {android.util.Log.w("PneuMaintenance",e.javaClass.simpleName)}}
}
