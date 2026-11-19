package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.ReceivedFile
import org.junit.Assert.*
import org.junit.Test
import java.nio.file.Files
import java.nio.ByteBuffer

class ReceiveSpoolTest {
    @Test fun checkpointReopenTruncatesUncommittedTailAndPreservesExactPrefix() {
        val root=Files.createTempDirectory("pneu-spool-test").toFile()
        try {
            var now=1000L;val spool=ReceiveSpool(root,wall={now});val row=ReceivedFile().apply {size=10}
            spool.create(row)
            spool.open(row).use {ReceiveSpool.write(it,"ABCDEtail".toByteArray());it.force(true)}
            row.checkpoint=5
            now+=100;assertTrue(spool.reconcile(row));assertEquals(1000L,row.progressWall)
            spool.open(row).use {it.position(row.checkpoint);ReceiveSpool.write(it,"FGHIJ".toByteArray());it.force(true);it.position(0);val bytes=ByteBuffer.allocate(10);it.read(bytes);assertEquals("ABCDEFGHIJ",String(bytes.array()))}
            spool.delete(row);assertFalse(spool.reconcile(row))
        }finally {root.deleteRecursively()}
    }
    @Test fun expiryRollbackMissingShortIdentityAndSymlinkAreRejected() {
        val root=Files.createTempDirectory("pneu-spool-test").toFile()
        try {
            var now=1000L;val spool=ReceiveSpool(root,wall={now})
            val row=ReceivedFile().apply {size=10};spool.create(row)
            now=999;assertFalse(spool.reconcile(row))
            now=1000+ReceiveSpool.TTL;assertFalse(spool.reconcile(row))
            now=1000;row.checkpoint=1;assertFalse(spool.reconcile(row));row.checkpoint=0
            val original=row.stageIdentity;row.stageIdentity="wrong";assertFalse(spool.reconcile(row));row.stageIdentity=original
            val target=Files.createTempFile("pneu-spool-foreign",".bin")
            try {Files.writeString(target,"user data");spool.delete(row);Files.createSymbolicLink(java.io.File(row.stagePath).toPath(),target);assertFalse(spool.reconcile(row));spool.delete(row);assertEquals("user data",Files.readString(target))}finally {Files.delete(target)}
        }finally {root.deleteRecursively()}
    }
    @Test fun monotonicExpiryCannotBeExtendedByReconnectOrAForwardWallClockCorrection() {
        val root=Files.createTempDirectory("pneu-spool-test").toFile()
        try {
            var wall=1000L;var mono=100L
            val spool=ReceiveSpool(root,wall={wall},monotonic={mono});val row=ReceivedFile().apply {size=1};spool.create(row)
            wall+=100;mono+=100;assertTrue(spool.reconcile(row));assertEquals(1000L,row.progressWall)
            mono+=ReceiveSpool.TTL;assertFalse(spool.reconcile(row))
        }finally {root.deleteRecursively()}
    }

}
