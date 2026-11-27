package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.*
import org.junit.Assert.*
import org.junit.Test

class SmsQueueTest {
    private val phone="1".repeat(32)
    private val pc="2".repeat(32)
    private val peer=PairSettings.initialize(Peer("Computer","pin",pc,emptyList(),generation=1,smsSupported=true),phone)
    private val p=Prefs(deviceId=phone,peer=peer,peers=listOf(peer),sms=true)
    private val binding=DeliveryBinding(phone,"pin",pc,1)
    @Test fun permissionPauseMasterPairAndIdentityGateAdmission() {
        assertTrue(SmsQueue.allowed(p,true,binding))
        assertFalse(SmsQueue.allowed(p,false,binding))
        assertFalse(SmsQueue.allowed(p.copy(paused=true),true,binding))
        assertFalse(SmsQueue.allowed(p.copy(sms=false),true,binding))
        for(other in listOf(peer.copy(smsSupported=false),peer.copy(pairingLost=true),peer.copy(pin="changed"),peer.copy(generation=2),peer.copy(settings=peer.settings+("$phone/$pc/sms" to PairField(false,"1",phone))))) {
            assertFalse(SmsQueue.allowed(p.copy(peer=other,peers=listOf(other)),true,binding))
        }
        // SMS deliberately ignores queue-offline and mobile-data preferences.
        assertTrue(SmsQueue.allowed(p.copy(queue=false,mobile=false),true,binding))
    }
    @Test fun memoryQueueExpiresAtArrivalPlusTwoMinutesAndIsPerComputer() {
        var elapsed=100L
        val queue=SmsQueue {elapsed}
        val other=binding.copy(pcId="3".repeat(32))
        queue.add("Bank","code",123,listOf(binding,other))
        val id=queue.next(binding)!!.id
        queue.delivered(id,binding)
        assertNull(queue.next(binding));assertEquals(id,queue.next(other)!!.id)
        elapsed=120_099;assertNotNull(queue.next(other))
        elapsed=120_100;assertNull(queue.next(other))
        queue.add("Bank","delayed receiver",123,listOf(binding),arrivedElapsed=100)
        assertNull(queue.next(binding))
    }
    @Test fun pauseOrOffRevokesQueuedSmsAcrossResumeAndSamePairReenable() {
        val queue=SmsQueue {0}
        queue.add("Bank","not persisted",123,listOf(binding))
        queue.prune(p.copy(paused=true),true)
        queue.prune(p,true);assertNull(queue.next(binding))
        queue.add("Bank","not revived",123,listOf(binding))
        queue.prune(p.copy(smsEpoch=2),true)
        assertNull(queue.next(binding))
        queue.add("Bank","permission revoked",123,listOf(binding))
        queue.prune(p,false);assertNull(queue.next(binding))
    }
    @OptIn(ExperimentalCoroutinesApi::class)
    @Test fun stalePreferenceWakeDoesNotDiscardSmsAdmittedAfterSwitchEnabled()=runTest {
        val key="$phone/$pc/sms"
        val disabledPeer=peer.copy(settings=peer.settings+(key to PairField(false,"1",phone)))
        val disabled=p.copy(peer=disabledPeer,peers=listOf(disabledPeer))
        val enabledPeer=peer.copy(settings=peer.settings+(key to PairField(true,"2",phone)))
        val enabled=p.copy(peer=enabledPeer,peers=listOf(enabledPeer))
        val snapshots=MutableStateFlow(disabled)
        val policy=AdmissionPolicy()
        val queue=SmsQueue {0}
        var current=disabled
        var reads=0
        val read:suspend ()->Prefs={reads++;current}
        val cleanup=launch {snapshots.collect {queue.pruneCurrent(policy,read) {true}}}
        try {
            // The committed switch and SMS admission overtake the flow's disabled snapshot.
            policy.locked {
                runCurrent()
                assertEquals(0,reads)
                current=enabled
                assertTrue(SmsQueue.allowed(current,true,binding))
                queue.add("Bank","code",123,listOf(binding),current.smsEpoch,
                    mapOf(pc to current.devices.single().settings.getValue(key)))
            }
            runCurrent()
            assertEquals(1,reads)
            assertFalse(snapshots.value.devices.single().sendSms)
            assertNotNull(queue.next(binding))
            // A later stale emission also cannot revoke the newly authorized entry.
            snapshots.value=disabled.copy(queue=false)
            runCurrent()
            assertNotNull(queue.next(binding))
            // Committed revocation still wins even before the flow catches up.
            policy.locked {current=disabled}
            queue.pruneCurrent(policy,read) {true}
            assertNull(queue.next(binding))
        } finally {cleanup.cancelAndJoin()}
    }
    @Test fun oldComputersKeepTheirThreeKeysAndSmsComputersGetFour() {
        val old=PairSettings.initialize(peer.copy(smsSupported=false),phone)
        assertEquals(3,PairSettings.snapshot(old).size())
        assertFalse(PairSettings.snapshot(old).any {it.asJsonObject["key"].asString.endsWith("/sms")})
        assertEquals(4,PairSettings.snapshot(peer).size())
        assertTrue(peer.sendSms)
    }
    @Test fun firstSmsOptInRefreshesHelloAndLaterDisablingKeepsTheDesktopSettingAvailable() {
        val old=p.copy(smsActivated=false)
        val activated=p.copy(smsActivated=true)
        assertTrue(io.github.stantonmatt.pneu.connection.BindingTransitions.restart(old,activated,binding))
        assertTrue(io.github.stantonmatt.pneu.connection.BindingTransitions.restart(activated,activated.copy(sms=false),binding))
    }

    @Test fun smsOnlyMobileConnectionDoesNotAdvertiseOrdinaryReceiveKinds() {
        assertEquals(listOf("mesh","sms"),SmsReceiveCapabilities.forRoute(true,false))
        for(caps in listOf(SmsReceiveCapabilities.forRoute(false,false),SmsReceiveCapabilities.forRoute(true,true))) {
            assertTrue(caps.containsAll(listOf("files","clipboard_text","clipboard_image","mesh","sms")))
        }
    }

}
