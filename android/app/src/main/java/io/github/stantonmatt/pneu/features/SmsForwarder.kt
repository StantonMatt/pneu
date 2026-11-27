package io.github.stantonmatt.pneu.features

import android.Manifest
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.provider.Telephony
import android.telephony.PhoneNumberUtils
import android.telephony.TelephonyManager
import android.os.SystemClock
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update
import java.util.Locale

/** Only RAM owns SMS. Monotonic expiry cannot be extended by a wall-clock change. */
internal class SmsQueue(private val elapsed:()->Long=SystemClock::elapsedRealtime) {
    class Entry(val id:String,val sender:String,val text:String,val receivedAt:Long,val expires:Long,
                val targets:MutableSet<DeliveryBinding>,val epoch:Long,val switches:Map<String,PairField>)
    private val entries=ArrayDeque<Entry>()
    @Synchronized fun add(sender:String,text:String,receivedAt:Long,targets:Collection<DeliveryBinding>,epoch:Long=0,switches:Map<String,PairField> = emptyMap(),arrivedElapsed:Long=elapsed()) {
        expire()
        if(targets.isEmpty())return
        if(entries.size>=128)entries.removeFirst()
        entries.add(Entry(randomId(),sender,text,receivedAt,arrivedElapsed+120_000,targets.toMutableSet(),epoch,switches))
    }
    @Synchronized fun remaining():Long? {expire();return entries.minOfOrNull {it.expires-elapsed()}}
    @Synchronized fun expire() {entries.removeAll {it.expires<=elapsed() || it.targets.isEmpty()}}
    @Synchronized fun prune(p:Prefs,permission:Boolean) {
        entries.forEach {entry->entry.targets.removeAll {target-> !allowed(p,permission,target) || entry.epoch!=p.smsEpoch || entry.switches[target.pcId]?.let {it!=p.devices.find {it.deviceId==target.pcId}?.settings?.get("${p.deviceId}/${target.pcId}/sms")}==true}}
        expire()
    }
    /** Snapshot emissions only wake cleanup; admission and pruning share current policy. */
    suspend fun pruneCurrent(policy:AdmissionPolicy,read:suspend ()->Prefs,permission:()->Boolean)=policy.locked {
        prune(read(),permission())
    }
    @Synchronized fun next(binding:DeliveryBinding):Entry? {expire();return entries.firstOrNull {binding in it.targets}}
    @Synchronized fun delivered(id:String,binding:DeliveryBinding) {entries.find {it.id==id}?.targets?.remove(binding);expire()}
    companion object {
        fun allowed(p:Prefs,permission:Boolean,target:DeliveryBinding):Boolean {
            val peer=p.devices.find {it.deviceId==target.pcId} ?: return false
            return p.sms && permission && !p.paused && !p.deliveryReset && target.phoneId==p.deviceId &&
                peer.pin==target.pin && peer.generation==target.generation && !peer.pairingLost &&
                peer.deviceId !in p.resetPeers && peer.smsSupported && peer.sendSms
        }
    }
}
class SmsForwarder(private val app:AppGraph) {
    internal val queue=SmsQueue()
    val changed=MutableStateFlow(0L)
    private val cleanup=Channel<Unit>(Channel.CONFLATED)
    init {
        app.scope.launch {
            app.prefs.collect {pruneCurrent()}
        }
        app.scope.launch {while(isActive) {
            pruneCurrent()
            val remaining=queue.remaining()
            if(remaining==null)cleanup.receive() else withTimeoutOrNull(remaining.coerceAtLeast(1)) {cleanup.receive()}
        }}
    }
    private suspend fun pruneCurrent()=queue.pruneCurrent(app.preferences.policy,app.preferences::read) {
        app.context.granted(Manifest.permission.RECEIVE_SMS)
    }
    suspend fun received(sender:String,text:String,receivedAt:Long,arrivedElapsed:Long=SystemClock.elapsedRealtime()) {
        if(sender.isEmpty() || sender.toByteArray().size>256 || text.toByteArray().size>16384 || '\u0000' in sender || '\u0000' in text)return
        app.preferences.policy.locked {
            val p=app.preferences.read()
            val targets=p.devices.map {DeliveryBinding(p.deviceId,it.pin,it.deviceId,it.generation)}
                .filter {SmsQueue.allowed(p,app.context.granted(Manifest.permission.RECEIVE_SMS),it)}
            queue.add(sender,text,receivedAt,targets,p.smsEpoch,p.devices.mapNotNull {peer->peer.settings["${p.deviceId}/${peer.deviceId}/sms"]?.let {peer.deviceId to it}}.toMap(),arrivedElapsed)
        }
        changed.update {it+1};cleanup.trySend(Unit)
        app.connection.kick()
    }
}
/** RECEIVE_SMS is signature-protected; no SMS provider/READ_SMS/contacts access. */
class SmsReceiver:BroadcastReceiver() {
    override fun onReceive(context:Context,intent:Intent) {
        if(intent.action!=Telephony.Sms.Intents.SMS_RECEIVED_ACTION)return
        val receivedAt=System.currentTimeMillis()
        val arrivedElapsed=SystemClock.elapsedRealtime()
        val messages=Telephony.Sms.Intents.getMessagesFromIntent(intent)
        val pending=goAsync()
        context.graph.scope.launch {
            try {
                withTimeout(9000) {
                    val app=context.graph
                    app.awaitPrefs()
                    if(!app.prefs.value.sms || app.prefs.value.paused || !context.granted(Manifest.permission.RECEIVE_SMS))return@withTimeout
                    val country=context.getSystemService(TelephonyManager::class.java).simCountryIso.uppercase(Locale.ROOT)
                    messages.groupBy {it.originatingAddress.orEmpty()}.forEach {(address,parts)->
                        val sender=if(address.any(Char::isLetter) || address.count(Char::isDigit)<=6)address
                            else PhoneNumberUtils.formatNumber(address,country) ?: address
                        app.sms.received(sender,parts.joinToString("") {it.messageBody.orEmpty()},receivedAt,arrivedElapsed)
                    }
                    // SMS broadcasts are an Android background-start exemption.
                    PneuService.start(context)
                }
            } catch(_:Exception) { /* Never put sender, text, or exceptions containing PDUs in logs. */ }
            finally {pending.finish()}
        }
    }
}

/** A connection kept alive only for SMS must not invite ordinary incoming data. */
internal object SmsReceiveCapabilities {
    fun forRoute(mobile:Boolean,mobileAllowed:Boolean):List<String> =
        if(mobile && !mobileAllowed)listOf("mesh","sms")
        else listOf("files","clipboard_text","clipboard_image","mesh","sms")
}
