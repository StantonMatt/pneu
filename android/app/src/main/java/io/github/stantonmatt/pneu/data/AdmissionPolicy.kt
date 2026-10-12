package io.github.stantonmatt.pneu.data

import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/** One serialization boundary for policy calculations, mutations and publication.
 * Preferences persists the epoch in the same edit as the policy it describes. */
class AdmissionPolicy {
    private val mutex=Mutex()
    suspend fun <T> locked(body:suspend ()->T):T=mutex.withLock {body()}
    suspend fun <T> publishing(publicationMutex:Mutex,body:suspend ()->T):T=locked {
        publicationMutex.withLock {body()}
    }
    companion object {
        fun next(epoch:Long)=Math.addExact(epoch,1)
        fun accepts(token:AdmissionToken,p:Prefs,liveVersion:String)=
            token.epoch==p.policyEpoch && !p.paused && token.version==liveVersion
        fun watermark(oldVersion:String,old:Long,version:String,value:Long)=
            if(oldVersion==version)maxOf(old,value) else value
    }
}

/** A scan's policy and catalog, independent of routine checkpoint advancement. */
data class AdmissionToken(val video:Boolean,val epoch:Long,val version:String) {
    companion object {
        fun from(p:Prefs,video:Boolean)=AdmissionToken(video,p.policyEpoch,if(video)p.videoVersion else p.imageVersion)
    }
}

