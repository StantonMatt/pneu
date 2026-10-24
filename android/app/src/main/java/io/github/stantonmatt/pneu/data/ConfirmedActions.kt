package io.github.stantonmatt.pneu.data

/** Immutable identity shown when a user makes a choice, before launching work.
 * Revision also distinguishes a same-PC repair without rotating phone identity. */
data class ConfirmedBinding(val phoneId:String,val pin:String?,val pcId:String?,val revision:Long,val pcName:String) {
    fun matches(p:Prefs):Boolean {
        if(pin==null && pcId==null)return p.loaded && phoneId==p.deviceId && revision==p.pairingRevision
        if(pin==null && pcId!=null)return p.loaded && phoneId==p.deviceId && p.devices.none {it.deviceId==pcId} && revision==p.pairingRevision
        val view=if(pcId==null)p else p.forPeer(pcId)
        val generation=if(p.peers.isEmpty())p.pairingRevision else view.pairingRevision
        return p.loaded && phoneId==p.deviceId && pin==view.peer?.pin && pcId==view.peer?.deviceId && revision==generation
    }
    fun pendingCount(rows:List<Outgoing>)=rows.filter {it.peerId==pcId && it.bindingGeneration==revision && it.state in setOf("waiting","sending","failed")}.distinctBy {it.mediaId}.size
    companion object {
        fun forInvitation(p:Prefs,invitation:io.github.stantonmatt.pneu.protocol.Invitation):ConfirmedBinding {
            check(p.loaded && p.deviceId.isNotEmpty()) {"Settings not loaded"}
            return p.devices.find {it.pin==invitation.fingerprint}?.let {from(p,it.deviceId)}
                ?: ConfirmedBinding(p.deviceId,null,null,p.pairingRevision,"")
        }
        fun from(p:Prefs,peerId:String?=null):ConfirmedBinding {
            if(peerId!=null && p.devices.none {it.deviceId==peerId}) {
                check(p.loaded && p.deviceId.isNotEmpty()) {"Settings not loaded"}
                return ConfirmedBinding(p.deviceId,null,peerId,p.pairingRevision,"")
            }
            val p=if(p.peers.isEmpty() && peerId==null)p else (peerId ?: p.peer?.deviceId ?: p.devices.firstOrNull()?.deviceId)?.let(p::forPeer) ?: p
            check(p.loaded && p.deviceId.isNotEmpty()) {"Settings not loaded"}
            return ConfirmedBinding(p.deviceId,p.peer?.pin,p.peer?.deviceId,p.pairingRevision,p.peer?.name.orEmpty())
        }
    }
}
data class ConfirmedItem(val mediaId:String,val transferId:String,val revision:Long,val peerId:String="",val generation:Long=0) {
    fun matches(row:Outgoing)=peerId==row.peerId && generation==row.bindingGeneration && mediaId==row.mediaId && transferId==row.transferId && revision==row.revision && row.state in setOf("waiting","sending","failed")
    companion object {fun from(row:Outgoing)=ConfirmedItem(row.mediaId,row.transferId,row.revision,row.peerId,row.bindingGeneration)}
}
data class ConfirmedQueue(val binding:ConfirmedBinding,val items:List<ConfirmedItem>) {
    fun matches(p:Prefs)=p.loaded && binding.phoneId==p.deviceId && items.all {item->
        if(item.peerId.isEmpty())binding.matches(p) else
            DeliveryBinding.from(p,item.peerId)?.generation==item.generation
    }
    fun resolve(rows:List<Outgoing>):List<Outgoing>? = items.map {item->
        rows.find {it.peerId==item.peerId && it.mediaId==item.mediaId}?.takeIf(item::matches) ?: return null
    }
    companion object {
        fun from(p:Prefs,rows:List<Outgoing>)=ConfirmedQueue(ConfirmedBinding.from(p),rows.filter {it.state in setOf("waiting","sending","failed")}.map(ConfirmedItem::from))
        fun artifact(p:Prefs,item:ConfirmedItem,rows:List<Outgoing>):ConfirmedQueue =
            from(p,rows.filter {it.mediaId==item.mediaId && it.transferId==item.transferId}).let {
                if(item in it.items)it
                else it.copy(items=listOf(item.copy(revision=-1)))
            }
    }
}
