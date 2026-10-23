package io.github.stantonmatt.pneu.features

import android.app.Application
import android.database.sqlite.SQLiteFullException
import android.system.ErrnoException
import android.system.OsConstants
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.IOException

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ReceiveErrorMappingTest {
    @Test fun diskFullPermissionAndOtherIoFailuresProduceTheRequiredNacks() {
        for(errno in listOf(OsConstants.ENOSPC,OsConstants.EDQUOT))assertEquals("disk_full",ReceiveErrors.code(IOException("write failed",ErrnoException("write",errno))))
        assertEquals("disk_full",ReceiveErrors.code(SQLiteFullException()))
        for(errno in listOf(OsConstants.EROFS,OsConstants.EACCES,OsConstants.EPERM))assertEquals("not_writable",ReceiveErrors.code(IOException("write failed",ErrnoException("open",errno))))
        assertEquals("not_writable",ReceiveErrors.code(SecurityException()))
        assertEquals("storage_failed",ReceiveErrors.code(IOException()))
        assertEquals("disk_full",ReceiveErrors.code(ReceiveStorageException("disk_full")))
    }
}
