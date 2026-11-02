package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.Outgoing
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.IOException

class FolderSourceTest {
    private class Tree:FolderTree {
        val nodes=linkedMapOf("root" to TreeNode("root","Trip",TreeKind.DIRECTORY,0,0))
        val parents=mutableMapOf<String,String>();val data=mutableMapOf<String,ByteArray>()
        var bytesRead=0L;var opens=0
        val unreadable=mutableSetOf<String>();var beforeRead:((String)->Unit)?=null
        fun add(id:String,kind:TreeKind=TreeKind.FILE,value:String="",parent:String="root") {
            nodes[id]=TreeNode(id,id.substringAfterLast('/'),kind,value.toByteArray().size.toLong(),0);parents[id]=parent;data[id]=value.toByteArray()
        }
        override suspend fun stat(id:String)=nodes.getValue(id)
        override suspend fun children(id:String):List<TreeNode> {
            if(id in unreadable)throw IOException("denied directory")
            return nodes.values.filter {parents[it.id]==id}
        }
        override suspend fun open(id:String):TreeRead {
            if(id in unreadable)throw IOException("denied")
            opens++;beforeRead?.invoke(id)
            val input=ByteArrayInputStream(data.getValue(id))
            return object:TreeRead {override suspend fun seek(offset:Long):Boolean {input.reset();return input.skip(offset)==offset};override suspend fun read(bytes:ByteArray)=input.read(bytes).also {if(it>0)bytesRead+=it};override fun close()=input.close()}
        }
    }
    private suspend fun bytes(tree:Tree,c:FolderCapture,q:Long=0):ByteArray=FolderStream(tree,c).use {s->s.verify();s.seek(q)
        val output=ByteArrayOutputStream();val b=ByteArray(2)
        while(true) {val n=s.read(b);if(n<0)break;output.write(b,0,n)}
        s.finish();output.toByteArray()
    }
    @Test fun nestedEmptyAndSkipped()=runBlocking {
        val tree=Tree().apply {
            add("nested",TreeKind.DIRECTORY);add("nested/empty",TreeKind.DIRECTORY,parent="nested")
            add("nested/a.txt",value="abc",parent="nested");add("zero")
            add("link",TreeKind.LINK);add("socket",TreeKind.SPECIAL);add("denied");unreadable.add("denied")
            add("denied-dir",TreeKind.DIRECTORY);add("denied-dir/unknown",parent="denied-dir");unreadable.add("denied-dir")
            add("._folder",TreeKind.DIRECTORY);add("._folder/inside",value="keep",parent="._folder")
            add(".DS_Store");add("._metadata")
        }
        val c=FolderBuilder.prepare(tree,"root")
        assertEquals(listOf("nested","nested/empty","nested/a.txt","zero","._folder","._folder/inside"),FolderManifest.parse(c.bytes,c.begin()).entries.map {it.path})
        assertEquals(3,c.summary.count);assertEquals(1,c.summary.skipped.links);assertEquals(1,c.summary.skipped.special)
        assertEquals(1,c.summary.skipped.unreadable);assertEquals(listOf("denied"),c.summary.skipped.names)
        assertEquals(1,c.summary.skipped.directories);assertEquals(listOf("denied-dir"),c.summary.skipped.directoryNames)
        assertEquals(c.hash,sha256(bytes(tree,c)))
        assertEquals(c,FolderCapture.decode(c.encode()))
    }
    @Test fun exactSection82VectorAndAllOffsets()=runBlocking {
        val tree=Tree().apply {add("empty",TreeKind.DIRECTORY);add("a.txt",value="abc")}
        val c=FolderBuilder.prepare(tree,"root")
        assertEquals(165L,c.summary.manifestLen);assertEquals(168L,c.size)
        assertEquals("06b77a27b341ed4486baab8d2126e98a9ba3f818aa61c717fdfb2b23de8a44e0",c.hash)
        val all=c.bytes+"abc".toByteArray();assertArrayEquals(all,bytes(tree,c))
        for(q in 0..c.size)assertArrayEquals("offset $q",all.copyOfRange(q.toInt(),all.size),bytes(tree,c,q))
        assertEquals(FolderPosition(164,0,0),folderPosition(c,164))
        assertEquals(FolderPosition(null,0,2),folderPosition(c,167))
        assertEquals(FolderPosition(null,1,0),folderPosition(c,168))
    }
    @Test fun emptyRootWireVectorStillValidThoughPickerUxDeclinesIt()=runBlocking {
        val tree=Tree()
        val c=FolderCapture(tree.nodes.getValue("root"),"{\"entries\":[]}",emptyList(),FolderSummary(14,0,FolderSkipped()),14,
            "d801aa1fb7ddcc330a5e3173372ea6af4a3d08ec58074478e85aa5603e926658")
        c.validate();assertEquals(c.hash,sha256(bytes(tree,c)))
        for(q in 0L..14L)assertArrayEquals(c.bytes.copyOfRange(q.toInt(),14),bytes(tree,c,q))
    }
    @Test fun offsetsAcrossTwoFilesAndZeroFiles()=runBlocking {
        val tree=Tree().apply {add("a",value="abcd");add("zero");add("b",value="efgh")}
        val c=FolderBuilder.prepare(tree,"root");val all=bytes(tree,c)
        for(q in c.summary.manifestLen-1..c.size)assertArrayEquals(all.copyOfRange(q.toInt(),all.size),bytes(tree,c,q))
    }
    @Test fun sameSizeSameMetadataChangeFailsEvenAtEofResume()=runBlocking {
        val tree=Tree().apply {add("a",value="abc")};val c=FolderBuilder.prepare(tree,"root")
        tree.data["a"]="xyz".toByteArray()
        for(q in listOf(0L,c.summary.manifestLen+2,c.size)) {
            try {bytes(tree,c,q);fail("changed source accepted at $q")}catch(_:FolderChanged) {}
        }
    }
    @Test fun changeDuringSendFailsBeforeEnd()=runBlocking {
        val tree=Tree().apply {add("a",value="abc")};val c=FolderBuilder.prepare(tree,"root")
        FolderStream(tree,c).use {stream->
            stream.verify();stream.seek(0);stream.read(ByteArray(c.bytes.size))
            tree.data["a"]="xyz".toByteArray()
            assertEquals(3,stream.read(ByteArray(3)))
            try {stream.finish();fail("must not send END")}catch(_:FolderChanged) {}
        }
    }
    @Test fun changedAtCaptureIsSkippedAndReported()=runBlocking {
        val tree=Tree().apply {add("a",value="abc");add("b",value="kept")}
        var reads=0
        tree.beforeRead={id->if(id=="a" && ++reads==2)tree.data[id]="xyz".toByteArray()}
        val c=FolderBuilder.prepare(tree,"root")
        assertEquals(listOf("b"),c.files.map {it.path});assertEquals(1,c.summary.skipped.unreadable)
    }
    @Test fun providerDisplayNameCannotInventAnAncestorPath()=runBlocking {
        val tree=Tree().apply {add("dir",TreeKind.DIRECTORY);add("file",value="abc");nodes["file"]=nodes.getValue("file").copy(name="dir/child")}
        try {FolderBuilder.prepare(tree,"root");fail("provider name introduced a false subtree")}catch(_:FolderInvalid) {}
    }
    @Test fun metadataCancellationStopsCaptureRatherThanSkippingTheDirectory()=runBlocking {
        val tree=Tree().apply {add("dir",TreeKind.DIRECTORY);add("a",value="abc")}
        val cancelled=object:FolderTree by tree {
            override suspend fun stat(id:String):TreeNode {
                if(id=="dir")throw kotlinx.coroutines.CancellationException("metadata cancelled")
                return tree.stat(id)
            }
        }
        try {FolderBuilder.prepare(cancelled,"root");fail("cancellation was swallowed")}catch(_:kotlinx.coroutines.CancellationException) {}
    }
    @Test fun legacyGateDoesNotAuthorizePicker() {
        assertTrue(FolderGate.needsUpdate(setOf("files"),true))
        assertTrue(FolderGate.needsUpdate(setOf("files",CAP_FOLDER),true))
        assertTrue(FolderGate.needsUpdate(setOf("files",CAP_FILE_RESUME),true))
        assertFalse(FolderGate.needsUpdate(setOf("files",CAP_FILE_RESUME,CAP_FOLDER),true))
        assertFalse(FolderGate.needsUpdate(emptySet(),false))
        assertFalse(LEGACY_PHONE_CAPABILITIES.contains(CAP_FOLDER))
    }
    @Test fun legacyMetadataRemainsUnchanged() {
        val row=Outgoing().apply {transferId="00112233445566778899aabbccddeeff";mediaId="manual:x";kind="file";fileName="a";mime="text/plain";size=3;hash=sha256("abc".toByteArray());createdAt=1}
        assertEquals(json("transfer_id" to row.transferId,"file_name" to "a","mime" to "text/plain","size" to 3,"sha256" to row.hash,"kind" to "file","created_at" to 1,"source_media_id" to "manual:x").toString(),Staging.metadata(row).toString())
    }
    @Test fun emptyAndOverLimit()=runBlocking {
        val empty=Tree().apply {add("empty",TreeKind.DIRECTORY);add("link",TreeKind.LINK)}
        try {FolderBuilder.prepare(empty,"root");fail()}catch(_:FolderEmpty) {}
        val huge=Tree().apply {add("huge");nodes["huge"]=nodes.getValue("huge").copy(size=MAX_FILE+1)}
        try {FolderBuilder.prepare(huge,"root");fail()}catch(_:FolderTooLarge) {}
    }
    @Test fun resumeReadsNoPrefixBeforeFirstChunk()=runBlocking {
        val tree=Tree().apply {add("big",value="x".repeat(4*1024*1024))}
        val c=FolderBuilder.prepare(tree,"root")
        FolderStream(tree,c).use {stream->
            stream.verify() // before BEGIN, on an attempted row only
            val before=tree.bytesRead
            stream.seek(c.summary.manifestLen+3*1024*1024)
            assertEquals(before,tree.bytesRead)
            assertEquals(CHUNK_SIZE,stream.read(ByteArray(CHUNK_SIZE)))
            assertEquals(CHUNK_SIZE.toLong(),tree.bytesRead-before)
        }
    }
    @Test fun preparationOnlyReadsContentTwice()=runBlocking {
        val tree=Tree().apply {add("a",value="abc")}
        FolderBuilder.prepare(tree,"root")
        assertEquals(6L,tree.bytesRead)
    }
    @Test fun providerStallIsRetryableNotChanged()=runBlocking {
        val tree=Tree().apply {add("a",value="abc")};val c=FolderBuilder.prepare(tree,"root")
        tree.beforeRead={throw IOException("Provider stalled")}
        try {io.github.stantonmatt.pneu.connection.SendFailures.local("folder_read") {FolderStream(tree,c).verify()};fail()}
        catch(e:io.github.stantonmatt.pneu.connection.LocalSendFailure) {
            assertEquals("local_read",e.code)
            assertEquals(60000L,io.github.stantonmatt.pneu.connection.SendFailures.retryAfter(e.code))
        }
    }
    @Test fun lostGrantIsPermanentAndSpecific()=runBlocking {
        try {io.github.stantonmatt.pneu.connection.SendFailures.local("folder_read") {throw SecurityException("revoked")};fail()}
        catch(e:io.github.stantonmatt.pneu.connection.LocalSendFailure) {
            assertEquals("folder_access_lost",e.code)
            assertEquals(Long.MAX_VALUE,io.github.stantonmatt.pneu.connection.SendFailures.retryAfter(e.code))
        }
    }
    @Test fun addedRootFileAndDirectoryMetadataDoNotInvalidateResume()=runBlocking {
        val tree=Tree().apply {add("dir",TreeKind.DIRECTORY);add("a",value="abc",parent="dir")}
        val c=FolderBuilder.prepare(tree,"root")
        tree.add("new",value="not in this capture")
        tree.nodes["root"]=tree.nodes.getValue("root").copy(size=42,modified=100)
        tree.nodes["dir"]=tree.nodes.getValue("dir").copy(size=23,modified=90)
        assertArrayEquals(c.bytes+"abc".toByteArray(),bytes(tree,c))
    }
    @Test fun declaredLimitFailsBeforeOpeningAnyContent()=runBlocking {
        val tree=Tree().apply {add("huge");nodes["huge"]=nodes.getValue("huge").copy(size=MAX_FILE+1)}
        try {FolderBuilder.prepare(tree,"root");fail()}catch(_:FolderTooLarge) {}
        assertEquals(0,tree.opens)
    }
    @Test fun invalidDuplicateAndDeepPathsHaveSpecificReasons()=runBlocking {
        for(name in listOf("bad/name","bad\\name","X:","..")) {
            val tree=Tree().apply {add("a");nodes["a"]=nodes.getValue("a").copy(name=name)}
            try {FolderBuilder.prepare(tree,"root");fail(name)}catch(e:FolderInvalid) {assertEquals(FolderInvalid.Reason.PATH,e.reason)}
        }
        val duplicate=Tree().apply {add("a");add("b");nodes["b"]=nodes.getValue("b").copy(name="a")}
        try {FolderBuilder.prepare(duplicate,"root");fail()}catch(e:FolderInvalid) {assertEquals(FolderInvalid.Reason.PATH,e.reason)}
        val deep=Tree();var parent="root"
        repeat(33) {val id="d$it";deep.add(id,TreeKind.DIRECTORY,parent=parent);parent=id}
        try {FolderBuilder.prepare(deep,"root");fail()}catch(e:FolderInvalid) {assertEquals(FolderInvalid.Reason.DEPTH,e.reason)}
    }
    @Test fun totalInspectedCapRefusesWholeCapture()=runBlocking {
        val tree=Tree().apply {repeat(MAX_MANIFEST_ENTRIES+1) {add("f$it")}}
        try {FolderBuilder.prepare(tree,"root");fail()}catch(e:FolderInvalid) {assertEquals(FolderInvalid.Reason.COUNT,e.reason)}
        assertEquals(0,tree.opens)
    }
    @Test fun allUnreadableIsNotEmpty()=runBlocking {
        val tree=Tree().apply {add("locked");unreadable.add("locked")}
        try {FolderBuilder.prepare(tree,"root");fail()}catch(e:FolderInvalid) {assertEquals(FolderInvalid.Reason.UNREADABLE,e.reason)}
    }
    @Test fun repeatedCaptureChangesStopAfterOneRestart()=runBlocking {
        val tree=Tree().apply {add("a",value="abc");add("b",value="abc");add("c",value="abc")}
        val opened=mutableMapOf<String,Int>()
        tree.beforeRead={id->val n=(opened[id]?:0)+1;opened[id]=n;if(n==2)tree.data[id]="xyz".toByteArray()}
        try {FolderBuilder.prepare(tree,"root");fail()}catch(e:FolderInvalid) {assertEquals(FolderInvalid.Reason.CHANGING,e.reason)}
        assertTrue(tree.opens<=8)
    }

    @Test fun nonseekableResumeDoesNotReadAndCanRestartAtZero()=runBlocking {
        val tree=Tree().apply {add("a",value="abc")};val c=FolderBuilder.prepare(tree,"root")
        val pipe=object:FolderTree by tree {
            override suspend fun open(id:String):TreeRead {
                val input=tree.open(id)
                return object:TreeRead by input {override suspend fun seek(offset:Long)=false}
            }
        }
        FolderStream(pipe,c).use {stream->
            val before=tree.bytesRead
            try {stream.seek(c.summary.manifestLen+2);fail()}catch(_:IOException) {}
            assertEquals(before,tree.bytesRead)
            stream.seek(0)
            val bytes=ByteArrayOutputStream();val buffer=ByteArray(CHUNK_SIZE)
            while(true) {val n=stream.read(buffer);if(n<0)break;bytes.write(buffer,0,n)}
            assertEquals(c.hash,sha256(bytes.toByteArray()))
        }
    }

    @Test fun fileBecomingUnreadableInSecondPassIsSkipped()=runBlocking {
        val tree=Tree().apply {add("a",value="abc");add("b",value="kept")};var opens=0
        tree.beforeRead={id->if(id=="a" && ++opens==2)throw IOException("Provider stalled")}
        val capture=FolderBuilder.prepare(tree,"root")
        assertEquals(listOf("b"),capture.files.map {it.path})
        assertEquals(1,capture.summary.skipped.unreadable)
    }
    @Test fun streamingProviderFailureRemainsRetryable()=runBlocking {
        val tree=Tree().apply {add("a",value="abc")};val c=FolderBuilder.prepare(tree,"root")
        tree.beforeRead={throw IOException("Provider stalled")}
        FolderStream(tree,c).use {stream->
            stream.seek(c.summary.manifestLen)
            try {io.github.stantonmatt.pneu.connection.SendFailures.local("folder_read") {stream.read(ByteArray(3))};fail()}
            catch(e:io.github.stantonmatt.pneu.connection.LocalSendFailure) {assertEquals("local_read",e.code)}
        }
    }

}
