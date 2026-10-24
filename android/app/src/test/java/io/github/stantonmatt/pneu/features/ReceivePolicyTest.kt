package io.github.stantonmatt.pneu.features

import org.junit.Assert.*
import org.junit.Test

class ReceivePolicyTest {
    @Test fun policyMatrix() {
        for(kind in listOf("file","photo","screenshot","recording","clipboard_image"))
            for(paused in listOf(false,true))for(clipboard in listOf(false,true))for(automatic in listOf(false,true))for(mesh in listOf(false,true)) {
                val disabled=kind=="clipboard_image" && !clipboard || kind in listOf("photo","screenshot","recording") && (paused || !automatic)
                val result=ReceivePolicy.newTransfer(kind,paused,clipboard,automatic,mesh)
                if(!disabled)assertNull(result)
                else assertEquals(if(mesh)ReceiveReply("unsupported_kind") else ReceiveReply(),result)
            }
    }
    @Test fun foldersAndNamesCannotFeedCameraOrScreenshotPaths() {
        assertEquals("Pictures/Pneu/",ReceivePolicy.folder("image/jpeg"))
        assertEquals("Movies/Pneu/",ReceivePolicy.folder("video/mp4"))
        assertEquals("Download/Pneu/",ReceivePolicy.folder("application/pdf"))
        assertEquals("photo__name.png",ReceivePolicy.storageName("..photo\n\u202ename.png"))
        assertEquals("file",ReceivePolicy.storageName(" ... "))
        assertFalse(ReceivePolicy.acceptsMime("image/"));assertFalse(ReceivePolicy.acceptsMime("text/plain\n"));assertTrue(ReceivePolicy.acceptsMime("application/vnd.example+json"))
    }
    @Test fun automaticSettingsAreDirectional() {
        val phone="a".repeat(32);val computer="b".repeat(32)
        val outbound="$phone/$computer/automatic_media";val inbound="$computer/$phone/automatic_media"
        val peer=io.github.stantonmatt.pneu.data.Peer("Computer","pin",computer,emptyList(),automaticMedia=false,
            settings=mapOf(outbound to io.github.stantonmatt.pneu.data.PairField(false,"0",phone)))
        assertTrue(ReceivePolicy.automaticEnabled(peer,phone))
        assertFalse(ReceivePolicy.automaticEnabled(peer.copy(settings=peer.settings+(inbound to io.github.stantonmatt.pneu.data.PairField(false,"0",computer))),phone))
    }
    @Test fun retryabilityMatchesTheWireContract() {
        io.github.stantonmatt.pneu.protocol.Protocol.nackCodes.forEach {code->
            val result=ReceiveReply(code).frame("a".repeat(32))
            io.github.stantonmatt.pneu.protocol.Protocol.validate(result.type,result.body)
        }
    }
}
