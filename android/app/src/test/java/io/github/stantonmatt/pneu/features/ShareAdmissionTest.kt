package io.github.stantonmatt.pneu.features

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import java.io.IOException

class ShareAdmissionTest {
    private val app="io.github.stantonmatt.pneu"
    private fun rejection(scheme:String?="content",authority:String?="media",path:String="/external/images/media/1",provider:String?="com.android.providers.media")=
        ShareAdmission.rejection(scheme,authority,path,path.split('/'),app,provider)

    @Test fun externalContentUsesAvailableAccessWithoutCallerOrApiProof() {
        assertNull(rejection())
        assertNull(rejection(authority="gallery.provider",provider="com.sec.android.gallery3d"))
        // Provider visibility may be absent; the real bounded open still decides access.
        assertNull(rejection(authority="unlisted.provider",provider=null))
    }
    @Test fun nonContentAndMissingAuthoritiesAreRejected() {
        for(scheme in listOf(null,"file","https"))assertEquals("not_content",rejection(scheme=scheme))
        for(authority in listOf(null,""))assertEquals("not_content",rejection(authority=authority))
    }
    @Test fun ownProviderAliasesAndPrivatePathsRemainRejected() {
        for(authority in listOf(app,"$app.files"))assertEquals("own_provider",rejection(authority=authority))
        assertEquals("own_provider",rejection(authority="another.alias",provider=app))
        for(path in listOf("/data/user/0/private","/proc/self/fd/1","/sys/private","/images/../private"))
            assertEquals("unsafe_content",rejection(path=path))
    }
    @Test fun successfulReadAndSpecificFailuresSurviveTheAdmissionBoundary()=runTest {
        assertEquals("bytes",shareStep("read_failed") {"bytes"})
        val open=ShareFailure("open_failed",IOException("Provider content must never be logged"))
        try {shareStep("read_failed") {throw open};fail("Open failure was accepted")}
        catch(e:ShareFailure) {assertSame(open,e);assertEquals("open_failed",e.reason)}
        val read=IOException("Provider content must never be logged")
        try {shareStep("read_failed") {throw read};fail("Read failure was accepted")}
        catch(e:ShareFailure) {assertEquals("read_failed",e.reason);assertSame(read,e.cause)}
        try {shareStep("read_failed") {throw BindingChangedException()};fail("Stale binding was accepted")}
        catch(e:ShareFailure) {assertEquals("binding_mismatch",e.reason)}
    }
    @Test fun genuineCancellationIsNotReportedAsAProviderFailure()=runTest {
        val cancelled=CancellationException()
        try {shareStep("read_failed") {throw cancelled};fail("Cancellation swallowed")}
        catch(e:CancellationException) {assertSame(cancelled,e)}
    }
}
