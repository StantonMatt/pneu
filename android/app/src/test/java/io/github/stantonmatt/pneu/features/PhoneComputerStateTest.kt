package io.github.stantonmatt.pneu.features

import com.google.gson.JsonParser
import io.github.stantonmatt.pneu.data.Prefs
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.*
import org.junit.Assert.*
import org.junit.Test
import java.io.*

@OptIn(ExperimentalCoroutinesApi::class)
class PhoneComputerStateTest {
    @Test fun section83VectorsAndUnknownFields() {
        val examples=mapOf(
            Type.BATTERY_STATE to listOf("""{"level":82,"charging":true}""","""{"level":0,"charging":false}""","{}"),
            Type.RING to listOf("""{"action":"start"}""","""{"action":"stop"}"""),
            Type.RING_STATE to listOf("""{"ringing":true}""","""{"ringing":false}"""))
        for((type,values) in examples)for(raw in values) {
            val body=JsonParser.parseString(raw).asJsonObject
            val bytes=ByteArrayOutputStream().also {Protocol.write(it,Frame.Control(type,body))}.toByteArray()
            assertEquals(body,(Protocol.read(ByteArrayInputStream(bytes)) as Frame.Control).body)
            body.addProperty("duration",9000);body.addProperty("permission",true)
            Protocol.validate(type,body)
        }
        for((type,raw) in listOf(Type.BATTERY_STATE to """{"level":101,"charging":false}""",
            Type.BATTERY_STATE to """{"level":82}""",Type.BATTERY_STATE to """{"charging":null}""",
            Type.RING to """{"action":"pause"}""",Type.RING_STATE to """{"ringing":1}"""))
            assertTrue(raw,runCatching {Protocol.validate(type,JsonParser.parseString(raw).asJsonObject)}.isFailure)
    }
    @Test fun defaultSettingsAndCapabilityTogglePreserveLegacyOrder() {
        assertTrue(Prefs().showBatteryLevel);assertTrue(Prefs().allowRinging)
        assertEquals(LEGACY_PHONE_CAPABILITIES+CAP_FILE_RESUME,phoneComputerCapabilities(false))
        assertEquals(LEGACY_PHONE_CAPABILITIES+CAP_FILE_RESUME+CAP_RING,phoneComputerCapabilities(true))
        assertFalse(CAP_BATTERY in phoneComputerCapabilities(true))
        assertFalse(CAP_NOTIFICATIONS in phoneComputerCapabilities(true))
        // Resume stays advertised when ring willingness is off.
        fun hello(caps:List<String>)=ByteArrayOutputStream().also {Protocol.write(it,Frame.Control(Type.HELLO,
            json("version" to 1,"device_id" to "0".repeat(32),"name" to "Phone","capabilities" to caps,"role" to "phone","paused" to false).apply {add("setup",io.github.stantonmatt.pneu.data.SetupState().hello(SetupScope.ASK).json())}))}.toByteArray()
        assertArrayEquals(hello(LEGACY_PHONE_CAPABILITIES+CAP_FILE_RESUME),hello(phoneComputerCapabilities(false)))
    }
    @Test fun batterySendsFreshCoalescesChangesClearsImmediatelyAndRefreshes()=runTest {
        val input=MutableStateFlow(BatteryState(82,true));val sent=mutableListOf<Pair<Long,BatteryState>>()
        val job=backgroundScope.launch(UnconfinedTestDispatcher(testScheduler)) {publishBattery(input,{testScheduler.currentTime}) {sent+=testScheduler.currentTime to it}}
        runCurrent();assertEquals(listOf(0L to BatteryState(82,true)),sent)
        input.value=BatteryState(81,true);runCurrent();advanceTimeBy(400)
        input.value=BatteryState(80,false);runCurrent();advanceTimeBy(599);runCurrent();assertEquals(1,sent.size)
        advanceTimeBy(1);runCurrent();assertEquals(1000L to BatteryState(80,false),sent.last())
        advanceTimeBy(10);input.value=BatteryState();runCurrent();assertEquals(1010L to BatteryState(),sent.last())
        advanceTimeBy(300_000);runCurrent();assertEquals(3,sent.size)
        input.value=BatteryState(79,false);runCurrent();assertEquals(301010L to BatteryState(79,false),sent.last())
        advanceTimeBy(300_000);runCurrent();assertEquals(601010L to BatteryState(79,false),sent.last())
        job.cancel()
    }
    @Test fun disconnectedPublisherDropsPendingSampleAndNewSessionSendsFresh()=runTest {
        val input=MutableStateFlow(BatteryState(82,false));val sent=mutableListOf<BatteryState>()
        val job=backgroundScope.launch(UnconfinedTestDispatcher(testScheduler)) {publishBattery(input,{testScheduler.currentTime}) {sent+=it}}
        runCurrent();input.value=BatteryState(81,true);runCurrent();job.cancelAndJoin()
        advanceTimeBy(2000);runCurrent();assertEquals(listOf(BatteryState(82,false)),sent)
        val next=backgroundScope.launch(UnconfinedTestDispatcher(testScheduler)) {publishBattery(input,{testScheduler.currentTime}) {sent+=it}}
        runCurrent();assertEquals(BatteryState(81,true),sent.last());next.cancel()
    }
    @Test fun ringRestartExtendsMonotonicDeadlineWithoutAnotherTransition() {
        val machine=RingMachine();assertTrue(machine.start(100))
        assertFalse(machine.start(29000));assertEquals(59000L,machine.deadline)
        assertFalse(machine.expire(30100));assertTrue(machine.ringing)
        assertFalse(machine.expire(58999));assertTrue(machine.expire(59000));assertFalse(machine.ringing)
    }
    @Test fun everyStopSourceIsIdempotentAndLeavesNoDeadline() {
        for(source in RingStopSource.entries) {
            val machine=RingMachine();machine.start(0)
            assertTrue(source.name,machine.stop(source));assertNull(machine.deadline)
            assertFalse(machine.stop(source));assertFalse(machine.expire(30000))
        }
    }
}
