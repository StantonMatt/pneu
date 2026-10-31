package io.github.stantonmatt.pneu.data

import androidx.datastore.preferences.core.*
import com.google.gson.*
import io.github.stantonmatt.pneu.protocol.*
import org.junit.Assert.*
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
        Peer("Other PC","d".repeat(64),d,emptyList(),generation=6,setupDecision=SetupScope.ENABLED))
    private fun initial()=mutablePreferencesOf(stringPreferencesKey("deviceId") to b,longPreferencesKey("pairingRevision") to 6L).also {
        PeerRegistry.write(it,senders);SetupStorage.write(it,SetupState(setupId,7,snapshot("join").members.filter {m->m.deviceId!=c}))
    }
    @Test fun allSharedSequences() {
        for(raw in shared.getAsJsonArray("scenarios")) {
            val scenario=raw.asJsonObject;var data=initial();var captured:ReadmissionPrompt?=null;var attempt:SetupAttemptTarget?=null
            fun state()=SetupStorage.read(data)
            fun peers()=PeerRegistry.read(data)
            fun prompt()=SetupReconcile.readmissions(state()).firstOrNull {it.key.startsWith("$c/")}
            fun subjectBarrier()=state().barriers["$c/$pe"] ?: state().barriers["$c/$pc"]
            fun answer(allow:Boolean,p:ReadmissionPrompt?=prompt()) {p?.let {SetupReconcile.answer(state(),peers(),b,it,allow,attempt)?.let {plan->SetupTransactions.commit(data,plan)}}}
            fun rejoin() {
                val retired=if(state().setupId==null)state() else SetupReconcile.retire(state(),true)
                SetupStorage.write(data,retired.copy(setupId=setupId,withdrawn=false,roster=snapshot("join").members.filter {it.deviceId!=c},
                    barriers=retired.barriers.mapValues {(key,barrier)->if(key=="$b/$pb")barrier.copy(cleared=true,readmission=null) else barrier}))
                PeerRegistry.write(data,senders)
            }
            fun pair(pin:String) {
                val prior=peers().find {it.deviceId==c};val peer=Peer("Laptop",pin,c,emptyList(),generation=20,setupDecision=SetupScope.ENABLED)
                SetupTransactions.paired(data,state(),peer,prior,true,true,now=now)
                PeerRegistry.write(data,peers().filterNot {it.deviceId==c}+peer)
                MeshClock.observe(data,(now-1).toBigInteger(),now)
                val stamp=MeshClock.next(data,b,false,now)
                val m=SetupMember(c,pin,"computer","Laptop","member",stamp.hlc.toString(),stamp.source)
                SetupStorage.write(data,state().copy(roster=state().roster.filterNot {it.deviceId==c && it.active || it.key==m.key}+m))
            }
            for((index,element) in scenario.getAsJsonArray("steps").withIndex()) {
                val step=element.asJsonObject;val before=state();val beforePeers=peers();val beforeRevision=data[longPreferencesKey("pairingRevision")] ?: 0
                var duplicate=false;var failure:Exception?=null
                try {
                    if(step.has("receive")) {
                        val name=step["receive"].asString;val snap=snapshot(name)
                        val sender=senders.single {it.deviceId==fixtures.getAsJsonObject(name)["authenticated_sender"].asString}
                        if(before.setupId==null || before.withdrawn)throw SetupUnavailable()
                        duplicate=before.receipts.any {it.sender==sender.deviceId && it.transferId==snap.transferId}
                        if(!duplicate) {
                            val plan=SetupReconcile.plan(before,peers(),self,sender,snap,attempt,now)
                            val receipt=SetupReceipt(sender.deviceId,snap.transferId,before.generation,name)
                            SetupTransactions.commit(data,plan.copy(state=plan.state.copy(receipts=plan.state.receipts+receipt)),sender)
                        }
                    } else when(step["local_action"].asString) {
                        "dismiss"->Unit
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
                    expected.has("nack_code")->assertTrue(label,failure is SetupUnavailable)
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
                str("record_decision",record?.decision?.name?.lowercase());str("introducer",record?.introducerId)
                str("value_hlc",record?.member?.hlc ?: subject?.hlc);str("value_writer",record?.member?.writer ?: subject?.writer)
                str("member_hlc",subject?.hlc)
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
                if(expected.has("readiness")) {assertFalse(label,peers().single {it.deviceId==c}.established);assertFalse(label,peers().single {it.deviceId==c}.readyObserved)}
                bool("hello_seen",peers().find {it.deviceId==c}?.helloSeen==true)
                if(expected.has("settings_bootstrap"))assertTrue(label,peers().single {it.deviceId==c}.settings.values.all {it.enabled && it.hlc=="0"})
                if(expected.has("diagnostic"))assertTrue(label,st.conflicts.any {it.deviceId==c && it.reason.startsWith("Different certificate")})
                bool("dial_C",peers().any {it.deviceId==c});bool("old_pin_barrier_retained",st.barriers.containsKey("$c/$pc"))
                bool("pending_record_discarded",barrier?.readmission==null);bool("setup_generation_advanced",st.generation>before.generation)
                bool("retired_roster_exported",st.roster.any {!it.active});bool("local_identity_unchanged",data[stringPreferencesKey("deviceId")]==b)
                if(expected.has("new_member_tuple_greater_than"))assertTrue(label,SetupReconcile.stamp(subject!!)>SetupReconcile.stamp(barrier!!.removed))
                if(expected.has("authored_values_delta"))assertEquals(label,before.roster,st.roster)
                if(expected.has("settings_reset"))assertEquals(label,beforePeers,peers())
                if(expected.has("new_question"))assertEquals(label,before.barriers.values.count {it.readmission?.decision==ReadmissionDecision.PENDING},st.barriers.values.count {it.readmission?.decision==ReadmissionDecision.PENDING})
            }
        }
    }
}
