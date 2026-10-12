package io.github.stantonmatt.pneu.data

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

/** A placeholder is not policy. Cold admissions retain the first authoritative
 * load; later replacement cannot change a recipient waiting to be captured. */
internal class PreferenceSnapshots(source:Flow<Prefs>,private val scope:CoroutineScope) {
    private val first=CompletableDeferred<Prefs>()
    val state=source.onEach {check(it.loaded);first.complete(it)}
        .stateIn(scope,SharingStarted.Eagerly,Prefs(loaded=false))
    suspend fun awaitLoaded():Prefs {
        val current=state.value
        return if(current.loaded)current else storageDeadline {first.await()}
    }
    fun captureBinding(select:(Prefs)->DeliveryBinding?={DeliveryBinding.from(it)}):Deferred<DeliveryBinding?> {
        val current=state.value
        return if(current.loaded)CompletableDeferred(select(current))
        else scope.async(start=CoroutineStart.UNDISPATCHED) {select(storageDeadline {first.await()})}
    }
}
