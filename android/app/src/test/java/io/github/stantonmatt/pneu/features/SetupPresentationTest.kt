package io.github.stantonmatt.pneu.features

import android.app.Application
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.SetupMember
import io.github.stantonmatt.pneu.protocol.SetupScope
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/** Every part C state the phone shows, mapped from the core's setup state. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk=[34],application=Application::class)
class SetupPresentationTest {
    private val r get()=RuntimeEnvironment.getApplication().resources
    private val self="a".repeat(32)
    private fun peer(c:Char,name:String,scope:SetupScope?=SetupScope.ASK,supported:Boolean=true,via:Peer?=null,lost:Boolean=false)=
        Peer(name,c.toString().repeat(64),c.toString().repeat(32),emptyList(),generation=c.code.toLong(),pairingLost=lost,
            setupDecision=scope,addedViaId=via?.deviceId,addedViaName=via?.name,setupSupported=supported)
    private fun member(c:Char,name:String,role:String="computer",status:String="member")=
        SetupMember(c.toString().repeat(32),c.toString().repeat(64),role,name,status,"1",self)
    private val me=member('a',"Galaxy S25+","phone")
    private val pc=peer('b',"MJStanton-Desktop");private val mac=peer('c',"MacBook Air");private val office=peer('d',"Office-PC")
    private fun prefs(vararg peers:Peer,state:SetupState=SetupState())=Prefs(deviceId=self,peers=peers.toList(),membership=state)
    private fun linked(vararg members:SetupMember,generation:Long=3)=SetupState("f".repeat(32),generation,listOf(me)+members)
    private fun prompt(p:Peer,generation:Long=3,allowed:Boolean=false)=SetupPrompt(p.deviceId,p.pin,p.generation,generation,p.name,allowed)
    private fun notice(p:Peer,via:Peer)=SetupNotice("3/${p.deviceId}/${p.generation}",p.deviceId,p.name,via.deviceId,via.name)

    @Test fun deviceRowCoversEveryStateAboveRemoveOrUnpair() {
        val enabled=pc.copy(setupDecision=SetupScope.ENABLED)
        fun row(peer:Peer,state:SetupState=linked())=SetupPresentation.page(prefs(peer,state=state),peer)
        assertEquals(SetupPage(null,null,LinkRow.LINK),row(pc))
        assertEquals(LinkRow.LINK,row(pc.copy(setupDecision=SetupScope.DISABLED)).row)
        assertEquals(LinkRow.PENDING,row(enabled).row)
        assertEquals(LinkRow.ASKED,row(pc,linked().copy(prompts=listOf(prompt(pc)))).row)
        assertEquals(LinkRow.UPDATE,row(pc.copy(setupSupported=false)).row)
        assertEquals(LinkRow.SEPARATE,row(pc,linked().copy(separateLinks=setOf(pc.deviceId))).row)
        // Allowed here: it links when that device reconnects, so nothing to tap.
        assertEquals(LinkRow.NONE,row(pc,linked().copy(prompts=listOf(prompt(pc,allowed=true)))).row)
        val linkedPage=row(enabled,linked(member('b',pc.name)))
        assertTrue(linkedPage.member);assertEquals(LinkRow.NONE,linkedPage.row);assertNull(linkedPage.addedVia)
        // In your devices through another device, this one pairing not linked: Link and Remove from all devices.
        val viaPhone=row(pc.copy(setupDecision=SetupScope.DISABLED),linked(member('b',pc.name)))
        assertTrue(viaPhone.member);assertEquals(LinkRow.LINK,viaPhone.row)
        assertEquals("MJStanton-Desktop",row(mac.copy(setupDecision=SetupScope.ENABLED,addedViaId=pc.deviceId,addedViaName=pc.name),linked(member('c',mac.name))).addedVia)
        assertEquals(LinkRow.NONE,row(pc.copy(pairingLost=true)).row)
        assertEquals(SetupPage(null,null,LinkRow.NONE),row(pc,SetupState(withdrawn=true)))
        // A removed member row or a different certificate is not one of your devices.
        assertFalse(row(enabled,linked(member('b',pc.name,status="removed"))).member)
        assertFalse(row(enabled.copy(pin="9".repeat(64)),linked(member('b',pc.name))).member)
    }
    @Test fun otherPhonesAreListedWithWhoAddedThem() {
        val pixel=member('e',"Pixel 9","phone")
        val state=linked(member('b',pc.name),pixel).copy(provenance=mapOf(pixel.key to SetupProvenance(pc.deviceId,pc.name)))
        assertEquals(listOf(PhoneMember(pixel.key,"Pixel 9","MJStanton-Desktop")),SetupPresentation.phones(prefs(pc,state=state)))
        assertTrue(SetupPresentation.phones(prefs(pc,state=state.copy(withdrawn=true))).isEmpty())
        assertTrue(SetupPresentation.phones(prefs(pc,state=state.copy(roster=state.roster.map {if(it==pixel)it.copy(status="removed") else it}))).isEmpty())
    }
    @Test fun remoteScopeIsAnObservationNotLocalConsent() {
        for(remote in listOf(null,SetupScope.ENABLED,SetupScope.ASK,SetupScope.DISABLED)) {
            val enabled=pc.copy(setupDecision=SetupScope.ENABLED,remoteSetupScope=remote)
            val p=prefs(enabled)
            val declined=remote==SetupScope.DISABLED
            assertEquals(if(declined)LinkRow.THEIRS else LinkRow.PENDING,SetupPresentation.page(p,enabled).row)
            assertEquals(if(declined)emptyList<Peer>() else listOf(enabled),SetupPresentation.waiting(p))
            assertEquals(if(declined)null else LinkBanner.Waiting(listOf(pc.name)),SetupPresentation.banner(p,SetupUiState(linkStarted=true)))
            // A retained member is still removable, even if this direct link no longer participates.
            val retained=p.copy(membership=linked(member('b',pc.name)))
            assertTrue(SetupPresentation.page(retained,enabled).member)
            assertEquals(when(remote) {SetupScope.DISABLED->LinkRow.THEIRS;SetupScope.ASK->LinkRow.PENDING;else->LinkRow.NONE},
                SetupPresentation.page(retained,enabled).row)
            for(local in listOf(SetupScope.ASK,SetupScope.DISABLED)) {
                val plain=enabled.copy(setupDecision=local)
                assertEquals(LinkRow.LINK,SetupPresentation.page(prefs(plain),plain).row)
            }
            val legacy=enabled.copy(setupSupported=false)
            assertEquals(LinkRow.UPDATE,SetupPresentation.page(prefs(legacy),legacy).row)
            assertTrue(SetupPresentation.waiting(prefs(legacy)).isEmpty())
        }
        val declined=pc.copy(setupDecision=SetupScope.ENABLED,remoteSetupScope=SetupScope.DISABLED)
        val asking=mac.copy(setupDecision=SetupScope.ENABLED,remoteSetupScope=SetupScope.ASK)
        assertEquals(LinkBanner.Waiting(listOf(mac.name)),SetupPresentation.banner(prefs(declined,asking),SetupUiState(linkStarted=true)))
        assertEquals("Link on MJStanton-Desktop too",r.getString(io.github.stantonmatt.pneu.R.string.setup_row_theirs,pc.name))
    }
    @Test fun bannerOffersLinkOnceThenSaysWhereToAllow() {
        val before=prefs(pc,mac)
        val offer=SetupPresentation.banner(before,SetupUiState())
        assertEquals(LinkBanner.Offer(pc.deviceId),offer)
        assertEquals("Link your devices, so new ones pair just once",SetupPresentation.bannerText(r,offer!!))
        assertNull("✕ hides it for good",SetupPresentation.banner(before,SetupUiState(bannerDismissed=true)))
        assertNull("A device that asked gets the request instead",SetupPresentation.banner(prefs(pc,mac,state=SetupState(prompts=listOf(prompt(pc,0)))),SetupUiState()))
        assertNull("Older Pneu can't link yet",SetupPresentation.banner(prefs(pc.copy(setupSupported=false)),SetupUiState()))
        val after=prefs(pc.copy(setupDecision=SetupScope.ENABLED),mac.copy(setupDecision=SetupScope.ENABLED),state=linked(generation=4))
        val waiting=SetupPresentation.banner(after,SetupUiState(linkStarted=true))
        assertEquals(LinkBanner.Waiting(listOf(pc.name,mac.name)),waiting)
        assertEquals("Allow linking on MJStanton-Desktop and MacBook Air",SetupPresentation.bannerText(r,waiting!!))
        assertEquals("Allow linking on 3 devices",SetupPresentation.bannerText(r,LinkBanner.Waiting(listOf("A","B","C"))))
        assertNull(SetupPresentation.banner(after,SetupUiState()))
        // The PC linked; the Mac hasn't answered yet.
        val oneLeft=after.copy(membership=linked(member('b',pc.name),generation=4))
        assertEquals(LinkBanner.Waiting(listOf(mac.name)),SetupPresentation.banner(oneLeft,SetupUiState(linkStarted=true)))
        val done=after.copy(membership=linked(member('b',pc.name),member('c',mac.name),generation=4))
        assertNull(SetupPresentation.banner(done,SetupUiState(linkStarted=true)))
        assertEquals(SetupNews.Linked,SetupPresentation.news(done,SetupUiState(linkStarted=true),0))
        assertNull(SetupPresentation.news(done,SetupUiState(),0))
        assertNull("Still waiting",SetupPresentation.news(oneLeft,SetupUiState(linkStarted=true),0))
    }
    @Test fun requestsNameTheDevicesThatAsked() {
        assertEquals("MJStanton-Desktop wants to link your devices",SetupPresentation.requestTitle(r,listOf(pc.name)))
        assertEquals("Galaxy S25+ and MJStanton-Desktop want to link your devices",SetupPresentation.requestTitle(r,listOf("Galaxy S25+",pc.name)))
        assertEquals("A and 2 other devices want to link your devices",SetupPresentation.requestTitle(r,listOf("A","B","C")))
        val p=prefs(pc,mac,state=linked().copy(prompts=listOf(prompt(pc),prompt(mac,allowed=true),prompt(office))))
        // Answered Allow, or a pairing that's gone, is no longer asking.
        assertEquals(listOf(prompt(pc)),SetupPresentation.prompts(p))
        assertEquals(listOf(pc.name),SetupPresentation.promptNames(p,SetupPresentation.prompts(p)))
        assertTrue(SetupPresentation.prompts(p.copy(membership=p.membership.copy(generation=5))).isEmpty())
    }
    @Test fun thePhoneThatJoinedSaysAlsoLinkedWhileTheOthersSayJoined() {
        val viaPc=mac.copy(setupDecision=SetupScope.ENABLED,addedViaId=pc.deviceId,addedViaName=pc.name)
        val state=linked(member('b',pc.name),member('c',mac.name)).copy(
            roster=listOf(me.copy(hlc="100"),member('b',pc.name),member('c',mac.name)),notices=listOf(notice(viaPc,pc).copy(alsoLinked=true)))
        val p=prefs(pc,viaPc,state=state)
        val joining=SetupUiState(joinedPin=pc.pin,joinedAt=1000)
        val also=SetupPresentation.news(p,joining,2000)
        assertEquals(SetupNews.AlsoLinked(listOf(mac.name),listOf(notice(viaPc,pc).id)),also)
        assertEquals("Also linked: MacBook Air",SetupPresentation.newsText(r,also!!))
        assertTrue(SetupPresentation.joined(p).isEmpty())
        // Restart, delayed delivery and missing pairing UI memory cannot turn old members into news.
        assertEquals(also,SetupPresentation.news(p,SetupUiState(),Long.MAX_VALUE))
        assertEquals(also,SetupPresentation.news(p,joining,1000+SetupPresentation.PAIR_OUTCOME_WINDOW_MS))
        // On a phone whose own membership predates the new member, it is news.
        val older=p.copy(membership=state.copy(notices=state.notices.map {it.copy(alsoLinked=false)},roster=state.roster.map {if(it.deviceId==self)it.copy(hlc="0") else it}))
        val joined=SetupPresentation.news(older,joining,2000)
        assertEquals(SetupNews.Joined(notice(viaPc,pc),"c".repeat(32)+"/"+"c".repeat(64)),joined)
        assertEquals("MacBook Air joined · Added via MJStanton-Desktop",SetupPresentation.newsText(r,joined!!))
        // Still being installed: not yet.
        assertNull(SetupPresentation.news(p.copy(resetPeers=setOf(mac.deviceId)),SetupUiState(),0))
        assertEquals("Also linked: A and B",SetupPresentation.newsText(r,SetupNews.AlsoLinked(listOf("A","B"),emptyList())))
        assertEquals("Also linked: A, B and 2 more",SetupPresentation.newsText(r,SetupNews.AlsoLinked(listOf("A","B","C","D"),emptyList())))
    }
    @Test fun noticeClassificationSurvivesRenamesAndPresentationSuppressesDelivery() {
        val notices=listOf(notice(mac,pc).copy(alsoLinked=true),notice(office,pc))
        val state=linked(member('c',mac.name),member('d',office.name)).copy(notices=notices)
        for(clock in listOf("0","18446744073709551615")) {
            val p=prefs(mac,office,state=state.copy(roster=state.roster.map {it.copy(hlc=clock,name="Renamed")}))
            assertEquals(listOf(notices[0]),SetupPresentation.alsoLinked(p))
            assertEquals(listOf(notices[1]),SetupPresentation.joined(p))
            assertNull(SetupPresentation.news(p,SetupUiState(),0,notices.map {it.id}.toSet()))
        }
    }
    @Test fun scannerInstallationRequiresItsSelectedPinEvenWhenAnotherComputerIsNewer() {
        val introduced=mac.copy(generation=3)
        val p=prefs(introduced)
        assertNull(p.installedAfter(1,pc.pin))
        val paired=p.copy(peers=listOf(introduced,pc.copy(generation=2)))
        assertEquals(pc.deviceId,paired.installedAfter(1,pc.pin)?.pcId)
        assertNull(paired.installedAfter(2,pc.pin))
    }
    @Test fun phoneOnlyRosterKeepsHomeAvailable() {
        val phone=member('e',"Other phone","phone")
        assertTrue(SetupPresentation.hasDevices(prefs(state=linked(phone))))
        assertFalse(SetupPresentation.hasDevices(prefs(state=linked(phone.copy(status="removed")))))
    }
    @Test fun devicesWithTheirOwnLinkedDevicesAreAnnouncedOnce() {
        val p=prefs(pc,office,state=linked(member('b',pc.name)).copy(separateLinks=setOf(office.deviceId)))
        val afterPairing=SetupPresentation.news(p,SetupUiState(joinedPin=office.pin,joinedAt=0),1)
        assertEquals(SetupNews.Separate(office.deviceId,"Office-PC",true),afterPairing)
        assertEquals("Paired with Office-PC only. It already has its own linked devices.",SetupPresentation.newsText(r,afterPairing!!))
        val afterLink=SetupPresentation.news(p,SetupUiState(),1)
        assertEquals("Office-PC wasn't linked. It already has its own linked devices.",SetupPresentation.newsText(r,afterLink!!))
        assertNull(SetupPresentation.news(p,SetupUiState(announcedSeparate=setOf(office.deviceId)),1))
        // The banner leaves it out.
        assertNull(SetupPresentation.banner(prefs(office,state=SetupState(separateLinks=setOf(office.deviceId))),SetupUiState()))
    }
    @Test fun theSwitchStopsAtTheLimitOf17() {
        val sixteen=(1..16).map {SetupMember("%032x".format(it),"%064x".format(it),"computer","PC $it","member","1",self)}
        assertFalse(SetupPresentation.atLimit(prefs(state=SetupState("f".repeat(32),1,sixteen))))
        assertTrue(SetupPresentation.atLimit(prefs(state=SetupState("f".repeat(32),1,sixteen+me))))
        assertFalse(SetupPresentation.atLimit(prefs(state=SetupState("f".repeat(32),1,sixteen.map {it.copy(status="removed")}+me))))
    }
}
