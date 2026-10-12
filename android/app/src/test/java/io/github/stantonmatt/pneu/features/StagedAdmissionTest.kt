package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.test.runTest
import org.junit.*
import org.junit.Assert.*
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.IOException

class StagedAdmissionTest {
    @Rule @JvmField val files=TemporaryFolder()
    @Test fun togglingPolicyMidCaptureDropsWithoutExceptionAndRescanAdmitsFreshBytes()=runTest {
        val policy=AdmissionPolicy();val queue=Mutex();var p=Prefs(imageVersion="catalog")
        val old=AdmissionToken.from(p,false);val entered=CompletableDeferred<Unit>();val release=CompletableDeferred<Unit>()
        val part=files.newFile("snapshot.part");val target=File(files.root,"snapshot.bin")
        var rescans=0;var published=0;var result:Boolean?=null
        // launch deliberately has no exception wrapper: an escaped rejection
        // fails the parent test just as it would fail a dispatcher-owned observer.
        val capture=launch {
            result=StagedAdmission.publish(true,discard={part.delete();target.delete()},rescan={
                assertFalse(part.exists());assertFalse(target.exists());rescans++
            }) {
                part.outputStream().use {output->
                    output.write(byteArrayOf(1,2));entered.complete(Unit);release.await();output.write(byteArrayOf(3,4))
                }
                check(part.renameTo(target))
                policy.publishing(queue) {
                    if(!AdmissionPolicy.accepts(old,p,"catalog"))false else {published++;true}
                }
            }
        }
        entered.await()
        policy.locked {p=p.copy(originals=true,policyEpoch=AdmissionPolicy.next(p.policyEpoch))}
        release.complete(Unit);capture.join()
        assertEquals(false,result);assertEquals(0,published);assertEquals(1,rescans)
        val fresh=AdmissionToken.from(p,false)
        assertTrue(StagedAdmission.publish(true,discard={part.delete();target.delete()},rescan={rescans++}) {
            part.writeBytes(byteArrayOf(1,2,3,4));check(part.renameTo(target))
            policy.publishing(queue) {
                if(!AdmissionPolicy.accepts(fresh,p,"catalog"))false else {published++;true}
            }
        })
        assertEquals(1,published);assertEquals(1,rescans);assertFalse(part.exists())
        assertArrayEquals(byteArrayOf(1,2,3,4),target.readBytes())
    }
    @Test fun manualRejectionRemainsVisibleAndRealStorageFailureIsNotARescan()=runTest {
        var deleted=0;var rescans=0
        try {StagedAdmission.publish(false,{deleted++},{rescans++}) {false};fail("Manual rejection disappeared")}
        catch(e:IllegalStateException) {assertEquals("Binding changed",e.message)}
        val storage=IOException("Storage unavailable")
        try {StagedAdmission.publish(true,{deleted++},{rescans++}) {throw storage};fail("Storage failure disappeared")}
        catch(e:IOException) {assertSame(storage,e)}
        assertEquals(2,deleted);assertEquals(0,rescans)
    }
    @Test fun genuineCancellationDiscardsBytesAndSuccessfulCommitRetainsThem()=runTest {
        val part=files.newFile("cancel.part");val entered=CompletableDeferred<Unit>();var rescans=0
        val capture=launch {StagedAdmission.publish(true,{part.delete()},{rescans++}) {entered.complete(Unit);awaitCancellation()}}
        entered.await();capture.cancelAndJoin()
        assertTrue(capture.isCancelled);assertFalse(part.exists());assertEquals(0,rescans)
        val admitted=files.newFile("admitted.bin")
        assertTrue(StagedAdmission.publish(true,{admitted.delete()},{rescans++}) {true})
        assertTrue(admitted.exists());assertEquals(0,rescans)
    }
}
