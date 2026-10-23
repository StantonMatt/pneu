package io.github.stantonmatt.pneu.features

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.*
import java.util.concurrent.atomic.AtomicInteger

class ImageWorkBudgetTest {
    @Test fun optimizationAndAbandonedPreviewWorkersShareABoundedBudgetUntilTheirActualExit() {
        val pool=Executors.newFixedThreadPool(8)
        val entered=CountDownLatch(2);val release=CountDownLatch(1);val count=AtomicInteger();val peak=AtomicInteger()
        try {
            val tasks=(1..8).map {pool.submit {ImageWorkBudget.run {
                val n=count.incrementAndGet();peak.updateAndGet {maxOf(it,n)};entered.countDown()
                try {assertTrue(release.await(5,TimeUnit.SECONDS))}finally {count.decrementAndGet()}
            }}}
            assertTrue(entered.await(5,TimeUnit.SECONDS));assertEquals(2,count.get())
            // Caller abandonment does not release the actual worker's permit.
            assertEquals(2,peak.get());release.countDown();tasks.forEach {it.get(5,TimeUnit.SECONDS)}
            assertEquals(2,peak.get());assertEquals(0,count.get())
            try {ImageWorkBudget.run<Unit> {throw IllegalStateException("decode")}}catch(_:IllegalStateException) {}
            ImageWorkBudget.run {assertEquals(0,count.get())}
        }finally {release.countDown();pool.shutdownNow();assertTrue(pool.awaitTermination(5,TimeUnit.SECONDS))}
    }
}
