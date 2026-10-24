package io.github.stantonmatt.pneu

import android.content.Intent
import androidx.datastore.preferences.core.*
import androidx.test.platform.app.InstrumentationRegistry
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import org.junit.Assert.*
import org.junit.Test
import java.io.File

/** The canonical host runner force-stops both packages between these phases. */
class PairingProcessDeviceTest {
    private val c get()=InstrumentationRegistry.getInstrumentation().targetContext
    private val app get()=c.graph
    private val record get()=File(c.filesDir,"pairing-process.txt")
    private val id="d".repeat(32)
    private fun raw(pin:Char)="pneu://pair?v=1&name=Recovery&fp=${pin.toString().repeat(64)}&addr=127.0.0.1%3A47861&token=${"e".repeat(64)}&exp=${System.currentTimeMillis()+300000}"
    @Test fun prepareBeforeProcessDeath()=runBlocking<Unit> {
        DeviceFixture.reset();app.connection.stop();app.connection.awaitStopped()
        val uri=raw('a');assertTrue(app.requestPairing(uri,null))
        val afterInstall=InstrumentationRegistry.getArguments().getString("phase")=="after-install"
        if(afterInstall) {
            assertTrue(app.installBinding(Invitation.parse(uri),id))
            val row=DeviceFixture.text(app,"Old authority","share")
            app.preferences.store.edit {p->PeerRegistry.write(p,PeerRegistry.read(p).map {it.copy(pairingLost=true)})}
            val next=raw('c');assertTrue(app.requestPairing(next,app.preferences.read().peer!!.pin))
            app.database.openHelper.writableDatabase.execSQL("CREATE TRIGGER fail_pair_process BEFORE DELETE ON deliveries BEGIN SELECT RAISE(ABORT,'Interrupted installed cleanup'); END")
            assertTrue(runCatching {app.installBinding(Invitation.parse(next),id)}.isFailure)
            val p=app.preferences.read();assertNull(app.connection.attempt.value)
            assertEquals(setOf(id),p.resetPeers);assertNotNull(app.queue.find(row.mediaId))
            record.writeText("installed\n${p.devices.single().generation}\n${p.devices.single().pin}\n")
        } else {
            assertNotNull(app.pairingHello(app.connection.attempt.value!!,id))
            record.writeText("attempt\n${app.connection.attempt.value!!.id}\n")
            assertTrue(app.preferences.read().devices.isEmpty())
        }
    }
    @Test fun verifyAfterProcessDeath()=runBlocking<Unit> {
        app.initialized.await();assertNull(app.connection.attempt.value)
        val saved=record.readLines();val before=app.preferences.read()
        if(saved[0]=="installed") {
            assertEquals(saved[1].toLong(),before.devices.single().generation)
            assertEquals(saved[2],before.devices.single().pin)
            assertEquals(setOf(id),before.resetPeers)
            app.database.openHelper.writableDatabase.execSQL("DROP TRIGGER fail_pair_process")
            app.reconcileDelivery();app.reconcileDelivery()
            val after=app.preferences.read()
            assertEquals(before.devices,after.devices);assertTrue(after.resetPeers.isEmpty());assertTrue(app.queue.all().isEmpty())
        } else assertTrue(before.devices.isEmpty())
        record.delete();c.stopService(Intent(c,PneuService::class.java));app.connection.stop();app.connection.awaitStopped()
        DeviceFixture.reset()
    }
}
