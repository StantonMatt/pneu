package io.github.stantonmatt.pneu.connection

/** One owner and one discovery generation. Stop invalidates callbacks before
 * releasing resources, including synchronous/asynchronous startup failures. */
internal class DiscoveryLifecycle(
    private val acquire:()->Unit,
    private val start:(Long)->Unit,
    private val stop:(Long)->Unit,
    private val clear:()->Unit,
    private val release:()->Unit
) {
    private var owner=false
    private var generation=0L
    private var current:Long?=null
    val active:Boolean @Synchronized get()=current!=null
    @Synchronized fun open() {owner=true}
    @Synchronized fun close() {owner=false;teardown()}
    @Synchronized fun update(eligible:Boolean) {
        if(!owner || !eligible) {teardown();return}
        if(current!=null)return
        val token=++generation;current=token
        try {acquire();start(token)} catch(_:Exception) {teardown()}
    }
    @Synchronized fun fail(token:Long) {if(current==token)teardown()}
    @Synchronized fun accept(token:Long,body:()->Unit) {if(owner && current==token)body()}
    private fun teardown() {
        val token=current;current=null
        // Cleanup steps are independent; one platform failure cannot leak others.
        if(token!=null)runCatching {stop(token)}
        runCatching {clear()}
        runCatching {release()}
    }
}
