package io.github.stantonmatt.pneu.connection

import org.junit.Assert.*
import org.junit.Test
import java.io.IOException

class DiscoveryLifecycleTest {
    private class Fixture {
        var held=false;var watches=0;var cache=0;val starts=mutableListOf<Long>();val stops=mutableListOf<Long>()
        var acquireError=false;var startError=false;var stopError=false;var clearError=false
        val lease=DiscoveryLifecycle(
            {held=true;if(acquireError)throw IOException()},
            {starts+=it;if(startError)throw IOException()},
            {stops+=it;if(stopError)throw IOException()},
            {watches=0;cache=0;if(clearError)throw IOException()},
            {held=false}
        )
    }
    @Test fun policyAndOwnerStopClearResourcesAndRejectLateCallbacks() {
        val f=Fixture();f.lease.update(true);assertTrue(f.starts.isEmpty())
        f.lease.open();f.lease.update(true);val old=f.starts.single()
        f.lease.accept(old) {f.watches=2;f.cache=3};f.lease.update(true);assertEquals(1,f.starts.size)
        f.lease.update(false);assertFalse(f.held);assertFalse(f.lease.active);assertEquals(0,f.watches);assertEquals(0,f.cache)
        f.lease.update(true);val new=f.starts.last();assertNotEquals(old,new)
        f.lease.accept(old) {fail("Old callback repopulated resources")};f.lease.fail(old);assertTrue(f.lease.active)
        f.lease.close();f.lease.update(true);assertFalse(f.held);assertFalse(f.lease.active);assertEquals(2,f.starts.size)
        f.lease.accept(new) {fail("Stopped owner accepted callback")}
    }
    @Test fun synchronousAndAsynchronousStartupFailuresReleaseLockAndAllowRetry() {
        for(acquire in listOf(true,false)) {
            val f=Fixture();f.acquireError=acquire;f.startError=!acquire
            f.lease.open();f.lease.update(true);assertFalse(f.held);assertFalse(f.lease.active)
            f.acquireError=false;f.startError=false;f.lease.update(true);assertTrue(f.held)
            val token=f.starts.last();f.lease.fail(token);assertFalse(f.held);assertFalse(f.lease.active)
            f.lease.update(true);assertTrue(f.held);f.lease.close()
        }
    }
    @Test fun independentTeardownFailuresDoNotSkipOtherCleanup() {
        val f=Fixture();f.lease.open();f.lease.update(true);f.watches=1;f.cache=1;f.stopError=true;f.clearError=true
        f.lease.close();assertFalse(f.held);assertFalse(f.lease.active);assertEquals(0,f.watches);assertEquals(0,f.cache)
    }
}
