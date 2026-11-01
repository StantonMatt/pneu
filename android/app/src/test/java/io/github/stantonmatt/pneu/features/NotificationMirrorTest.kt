package io.github.stantonmatt.pneu.features

import com.google.gson.Gson
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import org.junit.Test
import java.io.*

class NotificationMirrorTest {
    private val peer=Peer("PC","a".repeat(64),"b".repeat(32),emptyList(),generation=1)
    private val p=Prefs(deviceId="c".repeat(32),peers=listOf(peer))
    private val consent=NotificationConsent(true,true,false,setOf(NotificationConsent.computer(peer)),setOf("0/org.example.chat"))
    private val notice=MirrorNotice("raw-android-account-key",0,"org.example.chat","Chat",1800000000000,"Ana","¿Llegaste?",postSequence=1)
    private fun drain(session:NotificationMirrorSession,active:List<MirrorNotice>,settings:NotificationConsent=consent,allowed:Boolean=true):List<NotificationEvent> {
        val result=mutableListOf<NotificationEvent>()
        repeat(260) {
            val next=session.next(active,settings,allowed) ?: return result
            result+=next;session.committed(next)
        }
        error("Unbounded reconciliation")
    }
    @Test fun consentGatingMatrix() {
        for(master in listOf(false,true))for(computer in listOf(false,true))for(app in listOf(false,true))
            for(access in listOf(false,true))for(capable in listOf(false,true))for(paused in listOf(false,true)) {
                val c=consent.copy(enabled=master,computers=if(computer)consent.computers else emptySet(),apps=if(app)consent.apps else emptySet())
                val allowed=c.allows(p.copy(paused=paused),peer,access,capable)
                val events=drain(NotificationMirrorSession(),listOf(notice),c,allowed)
                assertEquals("$master/$computer/$app/$access/$capable/$paused",master && computer && app && access && capable && !paused,events.any {it.action==NotificationAction.UPSERT})
            }
        assertFalse(consent.allows(p,peer.copy(role="phone"),true,true))
        assertFalse(consent.allows(p,peer.copy(generation=2),true,true))
        assertFalse(consent.allows(p,peer.copy(pin="d".repeat(64)),true,true))
        assertFalse(consent.allows(p.copy(resetPeers=setOf(peer.deviceId)),peer,true,true))
        assertFalse(consent.allows(p,peer.copy(pairingLost=true),true,true))
        assertFalse(notice.copy(profile=10).eligible(consent))
    }
    @Test fun secretAndExcludedEventsAreRemovedButPrivateContentIsAllowed() {
        for(excluded in listOf(notice.copy(secret=true),notice.copy(ongoing=true),notice.copy(summary=true),notice.copy(alerting=false))) {
            val session=NotificationMirrorSession()
            val initial=drain(session,listOf(notice)).last()
            val removed=drain(session,listOf(excluded)).single()
            assertEquals(NotificationAction.REMOVE,removed.action);assertEquals(initial.key,removed.key)
        }
        assertEquals("¿Llegaste?",drain(NotificationMirrorSession(),listOf(notice)).last().text)
    }
    @Test fun appNamesOnlyOmitsBothFieldsAndRevokesQueuedContent() {
        val session=NotificationMirrorSession();session.committed(session.next(emptyList(),consent,true)!!)
        val queued=session.next(listOf(notice),consent,true)!!
        assertFalse(session.current(queued,emptyList(),consent,true))
        assertFalse(session.current(queued,listOf(notice),consent,false))
        assertFalse(session.current(queued,listOf(notice.copy(secret=true)),consent,true))
        assertFalse(session.current(queued,listOf(notice),consent.copy(namesOnly=true),true))
        val event=drain(session,listOf(notice.copy(postSequence=2)),consent.copy(namesOnly=true)).single()
        val json=Gson().toJsonTree(event).asJsonObject
        assertFalse(json.has("title"));assertFalse(json.has("text"))
    }
    @Test fun randomKeysStableForUpdatesAndRemovalFreshAcrossRecipientsAndReconnect() {
        val s=NotificationMirrorSession();val first=drain(s,listOf(notice)).last()
        val update=drain(s,listOf(notice.copy(time=1,text="changed"))).single()
        assertEquals(first.key,update.key)
        assertEquals(first.key,drain(s,emptyList()).single().key)
        val second=drain(NotificationMirrorSession(),listOf(notice)).last()
        assertNotEquals(first.key,second.key);assertTrue(first.key!!.matches(Regex("[a-f0-9]{32}")))
        assertFalse(Gson().toJson(first).contains(notice.localKey))
    }
    @Test fun reconnectOnlyClearsAndLivePostsUpdatesAndRemovalsStillFlow() {
        val original=drain(NotificationMirrorSession(),listOf(notice)).last()
        val session=NotificationMirrorSession(afterPost=notice.postSequence)
        assertEquals(listOf(NotificationAction.CLEAR),drain(session,listOf(notice)).map {it.action})
        assertTrue(drain(session,listOf(notice)).isEmpty())
        assertTrue(drain(session,listOf(notice,notice.copy(localKey="listener-snapshot",postSequence=0))).isEmpty())
        val live=notice.copy(localKey="new-live-post",postSequence=2)
        val posted=drain(session,listOf(notice,live)).single()
        assertEquals(NotificationAction.UPSERT,posted.action);assertNotEquals(original.key,posted.key)
        val updated=drain(session,listOf(notice,live.copy(text="Updated",postSequence=3))).single()
        assertEquals(posted.key,updated.key);assertEquals("Updated",updated.text)
        val removed=drain(session,listOf(notice)).single()
        assertEquals(NotificationAction.REMOVE,removed.action);assertEquals(posted.key,removed.key)
        assertTrue(drain(session,emptyList()).isEmpty()) // Old notice never had a key on this connection.
    }
    @Test fun preConnectionUpdateGetsFreshKeyEvenWhenContentAndTimeAreUnchanged() {
        val original=drain(NotificationMirrorSession(),listOf(notice)).last()
        val session=NotificationMirrorSession(afterPost=notice.postSequence)
        drain(session,listOf(notice))
        val updated=drain(session,listOf(notice.copy(postSequence=2))).single()
        assertEquals(NotificationAction.UPSERT,updated.action);assertNotEquals(original.key,updated.key)
        assertEquals(notice.text,updated.text);assertEquals(notice.time,updated.time)
        val removed=drain(session,emptyList()).single()
        assertEquals(NotificationAction.REMOVE,removed.action);assertEquals(updated.key,removed.key)
    }
    @Test fun newest128AndEvictionBeforeUpsert() {
        val active=(0..128).map {notice.copy(localKey="key$it",time=it.toLong())}
        val s=NotificationMirrorSession();val initial=drain(s,active)
        assertEquals(128,initial.count {it.action==NotificationAction.UPSERT});assertFalse(initial.any {it.time==0L})
        val changes=drain(s,active+notice.copy(localKey="new",time=1000))
        assertEquals(listOf(NotificationAction.REMOVE,NotificationAction.UPSERT),changes.map {it.action})
        assertEquals(1000L,changes.last().time)
        assertTrue(drain(s,active+notice.copy(localKey="new",time=1000)).isEmpty())
    }
    @Test fun clearOnConnectDisableAndReconnectAppOptOutRemoves() {
        val s=NotificationMirrorSession()
        assertEquals(NotificationAction.CLEAR,drain(s,listOf(notice)).first().action)
        assertEquals(NotificationAction.REMOVE,drain(s,listOf(notice),consent.copy(apps=emptySet())).single().action)
        drain(s,listOf(notice))
        assertEquals(listOf(NotificationAction.CLEAR),drain(s,listOf(notice),allowed=false).map {it.action})
        assertTrue(drain(s,listOf(notice),allowed=false).isEmpty())
        assertTrue(drain(s,listOf(notice)).isEmpty())
        assertEquals(NotificationAction.UPSERT,drain(s,listOf(notice.copy(postSequence=2))).single().action)
        assertEquals(NotificationAction.CLEAR,drain(NotificationMirrorSession(),listOf(notice)).first().action)
    }
    @Test fun disablingAfterEmptyAuthorizedSnapshotStillClears() {
        val session=NotificationMirrorSession();drain(session,emptyList())
        assertEquals(NotificationAction.CLEAR,drain(session,emptyList(),allowed=false).single().action)
        assertTrue(drain(session,emptyList(),allowed=false).isEmpty())
    }
    @Test fun iconOncePerAppPerConnectionClearKeepsCache() {
        val png="89504e470d0a1a0a0000000d4948445200000001000000010804000000b51c0c020000000b4944415478da6364f80f00010501012718e3660000000049454e44ae426082"
        val n=notice.copy(icon=png);val s=NotificationMirrorSession()
        assertEquals(png,drain(s,listOf(n)).last().iconPng)
        assertNull(drain(s,listOf(n.copy(time=2))).single().iconPng)
        drain(s,listOf(n),allowed=false)
        assertNull(drain(s,listOf(n.copy(postSequence=2))).single().iconPng)
        assertEquals(png,drain(NotificationMirrorSession(),listOf(n)).last().iconPng)
    }
    @Test fun paragraphsUnicodeAndEscapedFrameBounds() {
        val evil=notice.copy(title="😀".repeat(500)+"\nsecond",text="\u0001".repeat(9000)+"\nsecret")
        val event=drain(NotificationMirrorSession(),listOf(evil)).last()
        assertFalse(event.title!!.contains("second"));assertTrue(event.title.toByteArray().size<=1024)
        assertFalse(event.text!!.contains("secret"));assertTrue(event.text.toByteArray().size<=8192)
        val frame=Frame.Control(Type.NOTIFICATION_EVENT,Gson().toJsonTree(event).asJsonObject)
        val out=ByteArrayOutputStream();Protocol.write(out,frame)
        val parsed=Protocol.read(ByteArrayInputStream(out.toByteArray())) as Frame.Control
        assertEquals(frame.body,parsed.body)
    }
    @Test fun escapedPayloadDropsIconBeforeExceedingControlLimit() {
        val event=drain(NotificationMirrorSession(),listOf(notice.copy(title="\u0001".repeat(1024),text="\u0001".repeat(8192),icon="ab".repeat(8192)))).last()
        assertNull(event.iconPng)
        Protocol.write(ByteArrayOutputStream(),Frame.Control(Type.NOTIFICATION_EVENT,Gson().toJsonTree(event).asJsonObject))
    }
    @Test fun globalRateBurstFairnessAndCleanupPriority() {
        var now=0L;val rate=NotificationRateLimit {now}
        repeat(20) {assertTrue(rate.take("a",false))};assertFalse(rate.take("a",false))
        assertFalse(rate.take("b",false));now+=100
        assertTrue(rate.take("b",true));assertFalse(rate.take("a",false));now+=100
        assertTrue(rate.take("a",false))
        assertFalse(rate.take("b",false));assertFalse(rate.take("a",false));now+=100
        assertFalse(rate.take("a",false));assertTrue(rate.take("b",false))
        rate.forget("a");now+=100;assertTrue(rate.take("c",false))
    }
    @Test fun malformed83NotificationPayloadsAreRejected() {
        val id="112233445566778899aabbccddeeff0011"
        val upsert="""{"action":"upsert","key":"$id","app_package":"org.example.chat","app_label":"Chat","time":1800000000000}"""
        val bad=listOf("""{"action":"clear","key":"$id"}""","""{"action":"remove"}""",
            """{"action":"upsert","key":"raw-android-key"}""","""{"action":"clear","action":"clear"}""",
            upsert.dropLast(1)+",\"text\":null}",upsert.replace("Chat","\\u0000"),upsert.replace("Chat","x".repeat(129)),
            upsert.replace("org.example.chat","x".repeat(256)),upsert.replace("1800000000000","1.0"),
            upsert.dropLast(1)+",\"icon_png\":\"AB\"}",upsert.dropLast(1)+",\"text\":\""+"x".repeat(8193)+"\"}")
        for(raw in bad) {
            val bytes=ByteArrayOutputStream();DataOutputStream(bytes).apply {writeInt(raw.toByteArray().size+1);writeByte(0x33);write(raw.toByteArray())}
            assertTrue(raw.take(80),runCatching {Protocol.read(ByteArrayInputStream(bytes.toByteArray()))}.isFailure)
        }
    }
    @Test fun shared83NotificationVectorsAndLegacyHelloRemainUnchanged() {
        val vectors=com.google.gson.JsonParser.parseReader(InputStreamReader(javaClass.getResourceAsStream("/v1.json")!!)).asJsonObject
        var checked=0
        for(v in vectors["frames"].asJsonArray) {
            val decoded=v.asJsonObject["decoded"].asJsonObject
            if(decoded["type"].asString!="notification_event")continue
            val body=decoded["payload"].asJsonObject
            val event=Gson().fromJson(body,NotificationEvent::class.java)
            Protocol.validate(Type.NOTIFICATION_EVENT,Gson().toJsonTree(event).asJsonObject);checked++
        }
        assertTrue(checked>=4)
        assertEquals(listOf("files","clipboard_text","clipboard_image","mesh","setup"),LEGACY_PHONE_CAPABILITIES)
        assertFalse(CAP_NOTIFICATIONS in LEGACY_PHONE_CAPABILITIES)
    }
}
