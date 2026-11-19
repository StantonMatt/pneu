package io.github.stantonmatt.pneu.connection

import org.junit.Assert.*
import org.junit.Test

class FileFrameWriterTest {
    @Test fun cancelAfterFlushBeforeWriterReturnDoesNotClaimATornFrame() {
        val writer=FileFrameWriter()
        var flushed=false
        writer.write("transfer",{flushed=true},{
            // Held exactly between the IO flush and return to the sender dispatcher.
            assertTrue(flushed)
            assertFalse("A completed frame must keep the healthy connection",writer.cancel("transfer"))
        })
        assertFalse(writer.cancel("transfer"))
        writer.write("next",{
            assertFalse(writer.cancel("other"))
            assertTrue("An unfinished frame still requires teardown",writer.cancel("next"))
        })
    }
}
