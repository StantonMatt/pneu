package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.data.*

internal object BindingTransitions {
    fun scoped(p:Prefs,pin:String?):Prefs {
        if(pin==null)return p
        val peer=p.devices.find {it.pin==pin}
        return p.copy(peer=peer,lastLan=peer?.lastLan.orEmpty(),
            lastConnected=peer?.lastConnected ?: 0,pairingLost=peer?.pairingLost ?: false)
    }
    fun mayDial(p:Prefs,attempt:PairingAttempt?=null)=p.deviceId.isNotEmpty() && (attempt!=null || p.peer!=null && !p.pairingLost)
    fun hasWork(p:Prefs,attempt:PairingAttempt?=null)=attempt!=null || p.devices.any {!it.pairingLost}
    fun owns(p:Prefs,binding:DeliveryBinding?)=binding!=null && binding.phoneId==p.deviceId &&
        p.peer?.let {it.deviceId==binding.pcId && it.pin==binding.pin && it.generation==binding.generation}==true
    fun restart(old:Prefs,p:Prefs,binding:DeliveryBinding?)=
        old.wifi!=p.wifi || old.tailscale!=p.tailscale ||
        old.mobile!=p.mobile || old.sms!=p.sms || old.smsActivated!=p.smsActivated || old.selfName!=p.selfName || old.peer?.generation!=p.peer?.generation && !owns(p,binding)
}
internal object SessionTasks {
    suspend fun settings(close:()->Unit,body:suspend ()->Unit) {try {body()}finally {close()}}
}
