package io.github.stantonmatt.pneu.data

import kotlinx.coroutines.*
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.sync.Mutex
import org.junit.Assert.*
import org.junit.Test

class AdmissionPolicyTest {
    @Test fun snapshotCalculationAndMutationCannotBeOvertaken()=runTest {
        val policy=AdmissionPolicy();var p=Prefs(paused=true,imageVersion="catalog")
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        val old=async {policy.locked {
            entered.complete(Unit);release.await()
            p=p.copy(paused=false,policyEpoch=AdmissionPolicy.next(p.policyEpoch),imageFloor=11)
        }}
        entered.await()
        val newer=async {policy.locked {p=p.copy(paused=true,policyEpoch=AdmissionPolicy.next(p.policyEpoch))}}
        yield();assertFalse(newer.isCompleted);assertTrue(p.paused);assertEquals(0L,p.policyEpoch)
        release.complete(Unit);old.await();newer.await()
        assertTrue(p.paused);assertEquals(2L,p.policyEpoch);assertEquals(11L,p.imageFloor)
    }
    @Test fun delayedMutationCalculatesFreshValuesAfterInterveningResumeAndPause()=runTest {
        val policy=AdmissionPolicy();var p=Prefs(paused=true,imageVersion="catalog");var generation=10L
        val release=CompletableDeferred<Unit>()
        val delayed=async {release.await();policy.locked {
            p=p.copy(paused=false,policyEpoch=AdmissionPolicy.next(p.policyEpoch),imageFloor=AdmissionPolicy.watermark(p.imageVersion,p.imageFloor,"catalog",generation+1))
        }}
        policy.locked {p=p.copy(paused=false,policyEpoch=AdmissionPolicy.next(p.policyEpoch),imageFloor=11)}
        policy.locked {p=p.copy(paused=true,policyEpoch=AdmissionPolicy.next(p.policyEpoch));generation=30}
        release.complete(Unit);delayed.await()
        assertFalse(p.paused);assertEquals(3L,p.policyEpoch);assertEquals(31L,p.imageFloor)
    }
    @Test fun waitingPublicationRejectsAnEpochChangedUnderTheSameLock()=runTest {
        val policy=AdmissionPolicy();var p=Prefs(imageVersion="catalog")
        val token=AdmissionToken.from(p,false);var published=false
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        val change=async {policy.locked {entered.complete(Unit);release.await();p=p.copy(policyEpoch=AdmissionPolicy.next(p.policyEpoch))}}
        entered.await()
        val admission=async {policy.locked {if(AdmissionPolicy.accepts(token,p,"catalog"))published=true}}
        yield();assertFalse(admission.isCompleted)
        release.complete(Unit);change.await();admission.await();assertFalse(published)
    }
    @Test fun publicationHoldsTheBoundaryUntilTheQueueCommitCompletes()=runTest {
        val policy=AdmissionPolicy();var p=Prefs(imageVersion="catalog")
        val token=AdmissionToken.from(p,false)
        val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        val publication=async {policy.locked {assertTrue(AdmissionPolicy.accepts(token,p,"catalog"));entered.complete(Unit);release.await();assertEquals(0L,p.policyEpoch)}}
        entered.await()
        val change=async {policy.locked {p=p.copy(policyEpoch=AdmissionPolicy.next(p.policyEpoch))}}
        yield();assertFalse(change.isCompleted)
        release.complete(Unit);publication.await();change.await();assertEquals(1L,p.policyEpoch)
    }
    @Test fun liveNamespaceAndEpochBothHaveToMatchEvenIfSavedMetadataHasNotChanged() {
        val p=Prefs(imageVersion="old",policyEpoch=4);val token=AdmissionToken.from(p,false)
        assertTrue(AdmissionPolicy.accepts(token,p,"old"))
        assertFalse(AdmissionPolicy.accepts(token,p,"rebuilt"))
        assertFalse(AdmissionPolicy.accepts(token,p.copy(policyEpoch=5),"old"))
        assertFalse(AdmissionPolicy.accepts(token,p.copy(paused=true),"old"))
        assertTrue(AdmissionPolicy.accepts(token,p.copy(imageGeneration=99),"old"))
    }
    @Test fun catalogValidationRunsAfterWaitingForThePublicationLock()=runTest {
        val policy=AdmissionPolicy();val queue=Mutex();val p=Prefs(imageVersion="old")
        val token=AdmissionToken.from(p,false);var liveVersion="old";var checked=false
        queue.lock()
        val publication=async {policy.publishing(queue) {checked=true;AdmissionPolicy.accepts(token,p,liveVersion)}}
        yield();assertFalse(checked);assertFalse(publication.isCompleted)
        liveVersion="rebuilt";queue.unlock()
        assertFalse(publication.await());assertTrue(checked)
    }
    @Test fun floorsNeverRegressWithinACatalogAndEpochCannotWrap() {
        assertEquals(100L,AdmissionPolicy.watermark("catalog",100,"catalog",5))
        assertEquals(101L,AdmissionPolicy.watermark("catalog",100,"catalog",101))
        assertEquals(5L,AdmissionPolicy.watermark("old",100,"rebuilt",5))
        assertEquals(5L,AdmissionPolicy.next(4))
        try {AdmissionPolicy.next(Long.MAX_VALUE);fail("Wrapped epoch")}catch(_:ArithmeticException) {}
    }
    @Test fun failedOrCancelledCalculationReleasesTheLockWithoutPublishingAnEpoch()=runTest {
        val policy=AdmissionPolicy();var epoch=0L
        try {policy.locked {throw java.io.IOException("Snapshot failed")};fail()}catch(_:java.io.IOException) {}
        val entered=CompletableDeferred<Unit>()
        val cancelled=launch {policy.locked {entered.complete(Unit);awaitCancellation()}}
        entered.await();cancelled.cancelAndJoin()
        policy.locked {epoch=AdmissionPolicy.next(epoch)}
        assertEquals(1L,epoch)
    }
}
