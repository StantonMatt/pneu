package io.github.stantonmatt.pneu.features

import android.app.Application
import androidx.datastore.preferences.core.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.data.Preferences
import io.github.stantonmatt.pneu.protocol.randomId
import kotlinx.coroutines.*
import org.junit.*
import org.junit.Assert.*
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import java.io.File
import java.math.BigInteger

@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class,manifest=Config.NONE)
class ReceiveImageOrderingTest {
    private val phone="0".repeat(32);private val a="a".repeat(32);private val b="b".repeat(32)
    private fun preferences(body:suspend (Preferences)->Unit)=runBlocking {
        val scope=CoroutineScope(SupervisorJob()+Dispatchers.IO)
        val file=File(RuntimeEnvironment.getApplication().cacheDir,"image-order-${randomId()}.preferences_pb")
        val store=PreferenceDataStoreFactory.create(scope=scope) {file}
        try {store.edit {it[stringPreferencesKey("deviceId")]=phone};body(Preferences(RuntimeEnvironment.getApplication(),store))}
        finally {scope.cancel();scope.coroutineContext[Job]?.join();file.delete()}
    }
    @Test fun deferredImageUsesBeginStampAndCannotOverwriteLaterLocalCopy()=preferences {p->
        val image=ClipboardStamp(BigInteger.valueOf(System.currentTimeMillis()-100),a)
        p.observeClipboard(image);assertTrue(p.clipboardWins(image))
        val local=p.originateClipboard();assertTrue(local>image);assertFalse(p.clipboardWins(image))
    }
    @Test fun unfinishedTextOrImageOsEffectFencesQueuedImageEvenWhenPersistenceFailed()=preferences {p->
        val older=ClipboardStamp(BigInteger.valueOf(System.currentTimeMillis()-200),a)
        val effect=ClipboardStamp(older.hlc+BigInteger.ONE,b)
        p.recordClipboardEffect(effect,"hash",b,1,"unfinished")
        p.beforeMutation={if(it=="clipboardApplied")throw java.io.IOException("injected persistence failure")}
        try {p.appliedClipboard(effect,"hash",b,1,"unfinished");fail("Expected failure")}catch(_:java.io.IOException) {}
        assertFalse(p.clipboardWins(older));assertFalse(p.clipboardWins(effect))
        assertTrue(p.clipboardWins(ClipboardStamp(effect.hlc+BigInteger.ONE,a)))
    }
    @Test fun unfinishedLegacyImageFencesOlderLegacyTextInTheSameBinding()=preferences {p->
        val stamp=ClipboardStamp(BigInteger.valueOf(System.currentTimeMillis()),a)
        p.recordClipboardEffect(stamp,"hash",a,7,"image",legacyTime=1234)
        assertEquals(1234L,p.legacyClipboardTime(a,7,-1))
        assertEquals(-1L,p.legacyClipboardTime(a,8,-1))
        assertEquals(2000L,p.legacyClipboardTime(a,7,2000))
    }
    @Test fun equalHlcUsesAuthenticatedSourceAcrossComputersAndLegacyEffectsFenceMeshImages()=preferences {p->
        val stamp=ClipboardStamp(BigInteger.valueOf(System.currentTimeMillis()-100),a)
        p.recordClipboardEffect(stamp,"hash",a,1,"image")
        assertTrue(p.clipboardWins(ClipboardStamp(stamp.hlc,b)))
        val legacy=p.legacyClipboardStamp(b)
        p.recordClipboardEffect(legacy,"legacy",b,1,"legacy-image")
        assertFalse(p.clipboardWins(ClipboardStamp(stamp.hlc,b)))
    }
}
