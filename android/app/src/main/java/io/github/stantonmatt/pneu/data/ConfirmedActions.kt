package io.github.stantonmatt.pneu.data

/** Immutable identity shown when a user makes a choice, before launching work.
 * Revision also distinguishes a same-PC repair without rotating phone identity. */
data class ConfirmedBinding(val phoneId:String,val pin:String?,val pcId:String?,val revision:Long,val pcName:String) {
    fun matches(p:Prefs)=p.loaded && phoneId==p.deviceId && pin==p.peer?.pin && pcId==p.peer?.deviceId && revision==p.pairingRevision
    companion object {
        fun from(p:Prefs):ConfirmedBinding {
            check(p.loaded && p.deviceId.isNotEmpty()) {"Settings not loaded"}
            return ConfirmedBinding(p.deviceId,p.peer?.pin,p.peer?.deviceId,p.pairingRevision,p.peer?.name.orEmpty())
        }
    }
}
data class ConfirmedItem(val mediaId:String,val transferId:String,val revision:Long) {
    fun matches(row:Outgoing)=mediaId==row.mediaId && transferId==row.transferId && revision==row.revision && row.state in setOf("waiting","sending","failed")
    companion object {fun from(row:Outgoing)=ConfirmedItem(row.mediaId,row.transferId,row.revision)}
}
data class ConfirmedQueue(val binding:ConfirmedBinding,val items:List<ConfirmedItem>) {
    companion object {fun from(p:Prefs,rows:List<Outgoing>)=ConfirmedQueue(ConfirmedBinding.from(p),rows.filter {it.state in setOf("waiting","sending","failed")}.map(ConfirmedItem::from))}
}
