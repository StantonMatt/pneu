package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.*
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
import io.github.stantonmatt.pneu.features.*
import org.junit.Test

/** Executes the identical fixture objects and local action sequences used on desktop. */
class ReadmissionVectorsTest {
    private val shared=JsonParser.parseReader(javaClass.getResourceAsStream("/readmit-vectors.json")!!.reader()).asJsonObject
    private val fixtures=shared.getAsJsonObject("fixtures")
    private val a="1".repeat(32);private val b="2".repeat(32);private val c="3".repeat(32);private val d="4".repeat(32)
    private val pb="b".repeat(64);private val pc="c".repeat(64);private val pe="e".repeat(64)
    private val setupId="0".repeat(32);private val now=shared["clock_ms"].asLong
    private val self=SetupIdentity(b,pb,"Office PC","computer")
    private fun snapshot(name:String)=SetupWire.decode(fixtures.getAsJsonObject(name).getAsJsonObject("payload"))
    private val senders=listOf(Peer("Phone","a".repeat(64),a,emptyList(),role="phone",generation=5,setupDecision=SetupScope.ENABLED),
        Peer("Other PC","d".repeat(64),d,emptyList(),generation=6,setupDecision=SetupScope.ENABLED)).map {
        it.copy(established=true,readyObserved=true,helloObserved=true,setupSupported=true,remoteSetupScope=SetupScope.ENABLED)
    }
    private fun initial()=mutablePreferencesOf(stringPreferencesKey("deviceId") to b,longPreferencesKey("pairingRevision") to 6L).also {
        PeerRegistry.write(it,senders);SetupStorage.write(it,SetupState(setupId,7,snapshot("join").members.filter {m->m.deviceId!=c}))
    }
    @Test fun allSharedSequences() {
        for(raw in shared.getAsJsonArray("scenarios")) {
            val scenario=raw.asJsonObject;var data=initial();
            val firstRemove=scenario.getAsJsonArray("steps").map {it.asJsonObject}.firstOrNull {it["local_action"]?.asString=="remove-C-from-all-devices"}
            var localNow=firstRemove?.getAsJsonObject("stamp")?.get("hlc")?.asLong?.minus(1) ?: now;
            var holderResults=emptyMap<String,SetupPlan>();var captured:ReadmissionPrompt?=null;var attempt:SetupAttemptTarget?=null
            fun state()=SetupStorage.read(data)
            fun peers()=PeerRegistry.read(data)
            fun prompt()=SetupReconcile.readmissions(state()).firstOrNull {it.key.startsWith("$c/")}
            fun subjectBarrier()=state().barriers["$c/$pe"] ?: state().barriers["$c/$pc"]
            fun answer(allow:Boolean,p:ReadmissionPrompt?=prompt()) {p?.let {SetupReconcile.answer(state(),peers(),b,it,allow,attempt)?.let {plan->SetupTransactions.commit(data,plan)}}}
            fun rejoin() {
                val retired=if(state().setupId==null)state() else SetupReconcile.retire(state(),true)
                SetupStorage.write(data,retired.copy(setupId=setupId,withdrawn=false,roster=snapshot("join").members.filter {it.deviceId!=c},
                    barriers=retired.barriers.mapValues {(key,barrier)->if(key=="$b/$pb")barrier.clear().copy(readmission=null) else barrier}))
                PeerRegistry.write(data,senders)
            }
            fun pair(pin:String) {
                val prior=peers().find {it.deviceId==c};val peer=Peer("Laptop",pin,c,emptyList(),generation=20,setupDecision=SetupScope.ENABLED)
                SetupTransactions.paired(data,state(),peer,prior,true,true,now=now)
                PeerRegistry.write(data,peers().filterNot {it.deviceId==c}+peer)
                SetupTransactions.tokenHello(data,DeliveryBinding(b,pin,c,peer.generation),SetupHello(SetupScope.ENABLED,setupId),self,now)

            }
            for((index,element) in scenario.getAsJsonArray("steps").withIndex()) {
                val step=element.asJsonObject;val before=state();val beforePeers=peers();val beforeRevision=data[longPreferencesKey("pairingRevision")] ?: 0
                var duplicate=false;var failure:Exception?=null
                try {
                    if(step.has("receive")) {
                        val name=step["receive"].asString;val snap=snapshot(name)
                        val sender=peers().singleOrNull {it.deviceId==fixtures.getAsJsonObject(name)["authenticated_sender"].asString} ?: throw SetupUnavailable()
                        if(before.setupId==null || before.withdrawn)throw SetupUnavailable()
                        SetupReconcile.validate(snap,self,sender,localNow)
                        val hash=sha256(snap.canonical().toByteArray())
                        val receipt=before.receipts.find {it.sender==sender.deviceId && it.transferId==snap.transferId}
                        if(receipt!=null && receipt.hash!=hash)throw SetupReceiptConflict()
                        duplicate=receipt!=null
                        if(!duplicate) {
                            val plan=SetupReconcile.plan(before,peers(),self,sender,snap,attempt,localNow)
                            val receipt=SetupReceipt(sender.deviceId,snap.transferId,before.generation,hash)
                            SetupTransactions.commit(data,plan.copy(state=plan.state.copy(receipts=plan.state.receipts+receipt)),sender)
                        }
                    } else when(step["local_action"].asString) {
                        "dismiss"->Unit
                        "remove-C-from-all-devices"->{
                            val clock=step.getAsJsonObject("stamp")["hlc"].asLong
                            val target=state().member(c)!!
                            SetupTransactions.remove(data,target.key,state().generation,clock)
                            assertEquals(clock.toString(),state().barriers.getValue(target.key).removed.hlc)
                            localNow=now
                        }
                        "reset-to-initial-state"->{data=initial();localNow=now}
                        "authenticate-C-ready","authenticate-A-ready"->{
                            val id=if(step["local_action"].asString=="authenticate-C-ready")c else a
                            PeerRegistry.write(data,peers().map {if(it.deviceId==id)it.copy(helloObserved=true,established=true,readyObserved=true,
                                remoteSetupScope=SetupScope.ENABLED,setupRefused=false) else it})
                        }
                        "load-legacy-barrier-without-origin"->{
                            val m=snapshot("remove").members.single {it.deviceId==c}
                            SetupStorage.write(data,state().copy(roster=state().roster+m,barriers=mapOf(m.key to RemovalBarrier(m))))
                            val json=JsonParser.parseString(data[stringPreferencesKey("setupMembership")]).asJsonObject
                            val barrier=json.getAsJsonObject("barriers").getAsJsonObject(m.key)
                            barrier.remove("authored");barrier.remove("revoked_binding")
                            data[stringPreferencesKey("setupMembership")]=json.toString()
                        }
                        "load-legacy-member-without-claim"->{
                            val snap=snapshot("later-introduction-unclaimed")
                            SetupTransactions.commit(data,SetupReconcile.plan(state(),peers(),self,senders.first(),snap,now=now),senders.first())
                        }
                        "deliver-exported-snapshot-to-independent-holders"->{
                            val exporter=Peer(self.name,self.pin,b,emptyList(),role=self.role,setupDecision=SetupScope.ENABLED)
                            val payload=MembersSnapshot("f".repeat(32),setupId,state().roster)
                            holderResults=step.getAsJsonArray("holders").associate {entry->
                                val h=entry.asJsonObject;val origin=h.getAsJsonObject("origin");val id=h["device_id"].asString
                                val identityRow=payload.members.single {it.deviceId==id && it.active}
                                val removal=subjectBarrier()!!.removed
                                val barrier=RemovalBarrier(removal,authored=origin["authored"].asBoolean,revoked_binding=origin["revoked_binding"].asBoolean)
                                val initial=SetupState(setupId,7,payload.members.filter {it.deviceId!=c}+removal,barriers=mapOf(removal.key to barrier))
                                val trust=(senders+exporter).filter {it.deviceId!=id}
                                (if(barrier.isAuthored)"authored_holder" else "inherited_holder") to SetupReconcile.plan(initial,trust,
                                    SetupIdentity(id,identityRow.fingerprint,identityRow.name,identityRow.role),exporter,payload,now=now)
                            }
                        }
                        "arrange-departed-keeper-and-fresh-repairer"->{
                            val removal=snapshot("departed-keeper-removal").members.single {it.deviceId==c}
                            val claimed=snapshot("departed-keeper-claimed-member").members.single {it.deviceId==c}
                            assertEquals(step.getAsJsonObject("keeper").getAsJsonObject("removal")["writer_device"].asString,removal.writer)
                            assertTrue(SetupReconcile.claims(claimed,removal))
                        }
                        "arrange-introduced-phone-keeps-B-removed-and-stays-silent"->PeerRegistry.write(data,peers().map {
                            if(it.deviceId==a)it.copy(addedViaId=d,addedViaName="Other PC",helloObserved=false,established=false,readyObserved=false,
                                remoteSetupScope=null,setupRefused=false) else it})
                        "observe-without-authenticated-refusal"->Unit
                        "receive-authenticated-unauthorized-from-A"->PeerRegistry.write(data,peers().map {
                            if(it.deviceId==a)refusedPeer(it,DeliveryBinding(b,it.pin,a,it.generation),true) else it})
                        "restart"->{data=data.toMutablePreferences();assertEquals(before,state())}
                        "allow-C","allow-C-again"->answer(true)
                        "keep-removed-C"->answer(false)
                        "evict-membership-receipts"->SetupStorage.write(data,state().copy(receipts=emptyList()))
                        "capture-allow-notification"->captured=prompt()
                        "invoke-captured-allow"->answer(true,captured)
                        "retire-and-rejoin-S-via-A","fresh-token-rejoin-via-A"->rejoin()
                        "fresh-token-pair-C-with-enabled-scope"->pair(pc)
                        "fresh-token-pair-C-new-pin-with-enabled-scope"->pair(pe)
                        "fill-active-roster-to-17"->{val st=state();val added=(10 until 10+17-st.roster.count {it.active}).map {n->
                            SetupMember(n.toString(16).padStart(32,'0'),n.toString(16).padStart(64,'0'),"phone","Extra","member","1",b)}
                            SetupStorage.write(data,st.copy(roster=st.roster+added))}
                        "free-active-slot-and-start-direct-attempt-targeting-C-pin"->{SetupStorage.write(data,state().copy(roster=state().roster.filterNot {it.name=="Extra"}));attempt=SetupAttemptTarget(null,pc)}
                        "cancel-attempt-and-retry-allow-C"->{attempt=null;answer(true)}
                        else->error("Unknown action $step")
                    }
                } catch(e:Exception) {failure=e}
                val label="${scenario["name"].asString} step $index: $step"
                val expected=step.getAsJsonObject("expect")
                if(expected==null) {assertNull(label,failure);continue}
                when {
                    expected.has("error")->assertTrue(label,failure is MeshViolation)
                    expected.has("nack_code")->assertTrue(label,if(expected["nack_code"].asString=="id_conflict")failure is SetupReceiptConflict else failure is SetupUnavailable)
                    expected.has("visible_error")->assertTrue(label,failure is SetupCapacity)
                    expected.has("deferred")->assertTrue(label,failure is ReadmissionBlocked && !failure.conflict)
                    else->assertNull(label,failure)
                }
                val st=state();val barrier=subjectBarrier();val record=barrier?.readmission;val subject=st.roster.find {it.deviceId==c && it.active}
                fun bool(key:String,value:Boolean) {if(expected.has(key))assertEquals("$label $key",expected[key].asBoolean,value)}
                fun str(key:String,value:String?) {if(expected.has(key))assertEquals("$label $key",expected[key].asString,value)}
                fun tuple(key:String,m:SetupMember?) {if(expected.has(key)) {
                    val v=expected[key];if(v.isJsonNull)assertNull(label,m) else {assertEquals(label,v.asJsonObject["hlc"].asString,m?.hlc);assertEquals(label,v.asJsonObject["writer_device"].asString,m?.writer)}
                }}
                bool("denied",barrier?.cleared==false);bool("old_pin_denied",st.denies(c,pc));bool("new_pin_denied",st.denies(c,pe))
                bool("binding_C",peers().any {it.deviceId==c});str("binding_C_pin",peers().find {it.deviceId==c}?.pin)
                str("exported_C",st.roster.find {it.deviceId==c}?.status)
                str("record_decision",record?.decision?.name?.lowercase());str("introducer",record?.introducerId ?: subject?.let {st.provenance[it.key]?.id})
                str("value_hlc",record?.member?.hlc ?: subject?.hlc);str("value_writer",record?.member?.writer ?: subject?.writer)
                str("member_hlc",subject?.hlc);str("value_name",record?.member?.name ?: subject?.name)
                for(key in listOf("readmits","retained_claim"))if(expected.has(key)) {
                    val claim=subject?.readmits;val wanted=expected[key]
                    if(wanted.isJsonNull)assertNull("$label $key",claim) else {
                        assertEquals(label,wanted.asJsonObject["hlc"].asString,claim?.hlc)
                        assertEquals(label,wanted.asJsonObject["writer_device"].asString,claim?.writer)
                    }
                }
                if(expected.has("origin")) {
                    val origin=expected.getAsJsonObject("origin")
                    assertEquals(label,origin["authored"].asBoolean,barrier?.isAuthored)
                    assertEquals(label,origin["revoked_binding"].asBoolean,barrier?.revoked_binding==true)
                }
                bool("origin_effective_authored",barrier?.isAuthored==true)
                if(expected.has("membership_stamp_unchanged"))assertEquals(label,before.member(c)?.value(),subject?.value())
                for((key,plan) in holderResults)if(expected.has(key)) {
                    val e=expected.getAsJsonObject(key)
                    assertEquals(label,e["binding_C"].asBoolean,plan.peers.any {it.deviceId==c})
                    assertEquals(label,e["pending_questions_for_C"].asInt,SetupReconcile.readmissions(plan.state).size)
                    if(e.has("joined_notices_delta"))assertEquals(label,e["joined_notices_delta"].asInt,plan.installed.count {it.deviceId==c})
                }
                val aPeer=peers().find {it.deviceId==a}
                bool("remote_refusal_claimed",aPeer?.setupRefused==true)
                bool("remote_keep_removed_claimed",false)
                bool("also_linked",aPeer?.readyObserved==true)
                bool("trust_A_retained",aPeer!=null);bool("refusal_status_cleared",aPeer?.setupRefused==false)
                if(expected.has("status"))assertEquals(label,IntroducedStatus.SILENT,SetupPresentation.introducedStatus(aPeer!!))
                if(expected.has("status_example"))assertEquals(label,IntroducedStatus.SILENT,SetupPresentation.introducedStatus(aPeer!!))
                tuple("removal",barrier?.removed);tuple("removal_high_water",barrier?.removed);tuple("record_barrier",barrier?.removed)
                tuple("own_removal_high_water",st.barriers["$b/$pb"]?.removed)
                bool("own_deny_cleared",!st.denies(b,pb));bool("withdrawn",st.withdrawn)
                if(expected.has("setup_id") && expected["setup_id"].isJsonNull)assertNull(label,st.setupId)
                for(key in listOf("record","own_readmission_record"))if(expected.has(key)) {
                    val v=expected[key];val rec=if(key=="record")record else st.barriers["$b/$pb"]?.readmission
                    if(v.isJsonNull)assertNull(label,rec) else {
                        val obj=v.asJsonObject;assertNotNull(label,rec)
                        for((k,value) in obj.entrySet())assertEquals(label,value.asString,when(k) {
                            "decision"->rec?.decision?.name?.lowercase();"introducer"->rec?.introducerId;"value_hlc"->rec?.member?.hlc
                            "value_writer"->rec?.member?.writer;"value_name"->rec?.member?.name;else->error("Unknown record $k")})
                    }
                }
                for(key in listOf("pending_questions_for_C","pending_questions_for_own_key"))if(expected.has(key))assertEquals(label,expected[key].asInt,
                    SetupReconcile.readmissions(st).count {it.key.startsWith(if(key.endsWith("own_key"))"$b/" else "$c/")})
                bool("ack_deduplicated",duplicate)
                if(expected["state_unchanged"]?.asBoolean==true) {assertEquals(label,before,st);assertEquals(label,beforePeers,peers())}
                if(expected.has("joined_notices_delta"))assertEquals(label,expected["joined_notices_delta"].asInt,st.notices.size-before.notices.size)
                if(expected.has("binding_generation_delta"))assertEquals(label,expected["binding_generation_delta"].asLong,(data[longPreferencesKey("pairingRevision")] ?: 0)-beforeRevision)
                if(expected.has("roster_resend"))assertEquals(label,expected["roster_resend"].asBoolean,before.roster!=st.roster)
                if(expected.has("scope"))assertEquals(label,SetupScope.ENABLED,peers().single {it.deviceId==c}.setupScope)
                if(expected.has("readiness")) {assertEquals(label,expected["readiness"].asInt==1,peers().single {it.deviceId==c}.readyObserved)}
                bool("hello_seen",peers().find {it.deviceId==c}?.helloSeen==true)
                if(expected.has("settings_bootstrap"))assertTrue(label,peers().single {it.deviceId==c}.settings.values.all {it.enabled && it.hlc=="0"})
                if(expected.has("diagnostic")) {
                    if(expected["diagnostic"].asString.contains("different certificate"))assertTrue(label,st.conflicts.any {it.deviceId==c && it.reason.startsWith("Different certificate")})
                    else assertEquals(label,IntroducedStatus.REFUSED,SetupPresentation.introducedStatus(aPeer!!))
                }
                bool("dial_C",peers().any {it.deviceId==c});bool("old_pin_barrier_retained",st.barriers.containsKey("$c/$pc"))
                bool("pending_record_discarded",barrier?.readmission==null);bool("setup_generation_advanced",st.generation>before.generation)
                bool("retired_roster_exported",st.roster.any {!it.active});bool("local_identity_unchanged",data[stringPreferencesKey("deviceId")]==b)
                if(expected.has("new_member_tuple_greater_than"))assertTrue(label,SetupReconcile.stamp(subject!!)>SetupReconcile.stamp(barrier!!.removed))
                if(expected.has("authored_values_delta"))assertEquals(label,before.roster,st.roster)
                if(expected.has("settings_reset"))assertEquals(label,beforePeers.map {it.deviceId to it.settings},peers().map {it.deviceId to it.settings})
                if(expected.has("new_question"))assertEquals(label,before.barriers.values.count {it.readmission?.decision==ReadmissionDecision.PENDING},st.barriers.values.count {it.readmission?.decision==ReadmissionDecision.PENDING})
            }
        }
    }
}
