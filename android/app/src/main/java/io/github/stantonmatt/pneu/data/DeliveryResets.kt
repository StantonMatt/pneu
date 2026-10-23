package io.github.stantonmatt.pneu.data

/** Caller serializes binding changes. The original owner's quarantine and all
 * byte/receipt obligations are resolved before either durable journal clears.
 * Every step is idempotent so a process crash simply repeats unfinished work. */
internal class DeliveryResets(
    private val read:suspend ()->Prefs,
    private val resolve:suspend (Prefs)->Unit,
    private val clearLegacy:suspend ()->Unit,
    private val finishLegacy:suspend ()->Unit,
    private val clearPeer:suspend (String)->Unit,
    private val finishPeer:suspend (String)->Unit
) {
    private suspend fun legacy() {
        if(read().deliveryReset) {clearLegacy();finishLegacy()}
    }
    suspend fun peer(id:String) {
        legacy();resolve(read());clearPeer(id);finishPeer(id)
    }
    suspend fun reconcile() {
        legacy();val p=read();resolve(p)
        for(id in p.resetPeers)peer(id)
    }
}
