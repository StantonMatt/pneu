package io.github.stantonmatt.pneu.data

/** Freeze before provider reads. A later connection/setting update cannot add
 * recipients to this operation; binding checks can only remove revoked ones. */
object RecipientSelection {
    fun hasAuthority(p:Prefs,captured:List<DeliveryBinding>,fallback:DeliveryBinding?)=
        if(captured.isEmpty())fallback?.matches(p)==true else captured.any {it.matches(p)}
    fun surviving(p:Prefs,captured:List<DeliveryBinding>)=captured.filter {it.matches(p)}
    fun freeze(p:Prefs,connected:Set<String>,kind:String,automatic:Boolean=false,selected:Set<String>?=null):List<DeliveryBinding> {
        val clipboard=kind in setOf("text","clipboard_image")
        val onlineOnly=clipboard || automatic && !p.queue
        val candidates=p.devices.filter {peer->
            (!clipboard || peer.clipboardSend) && (!automatic || peer.automaticMedia) &&
                (selected==null || peer.deviceId in selected) &&
                (peer.deviceId in connected || !onlineOnly && (automatic && p.queue || selected!=null || connected.isEmpty() && p.queue))
        }
        return candidates.mapNotNull {DeliveryBinding.from(p,it.deviceId)}
    }
}
