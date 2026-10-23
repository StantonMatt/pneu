package io.github.stantonmatt.pneu.connection

/** Typed presentation contract. The legacy keys are retained for existing callers;
 * UI consumers must use this enum, never compare display text. */
enum class ConnectionStatus(val key:String) {
    UNREACHABLE("PC unreachable"), CONNECTING("Connecting…"), CONNECTED("Connected"),
    UPDATE_REQUIRED("Update required"), PAIRING_LOST("Pairing lost"), FINGERPRINT_MISMATCH("Fingerprint mismatch"),
    WAITING_WIFI("Waiting for Wi-Fi"), NO_NETWORK("No network"), PAUSED("Paused"),
    LOCAL_NETWORK_OFF("Local network access is off"), STORAGE_UNAVAILABLE("Storage unavailable"), LOADING("Loading");
    companion object {
        fun fromKey(key:String)=entries.firstOrNull {it.key==key} ?: UNREACHABLE
        fun remoteError(code:String)=when(code) {
            "version_mismatch", "unsupported_version", "unsupported_capability" -> UPDATE_REQUIRED
            else -> null
        }
        /** Legacy v1 peers may omit mesh; that alone never requires an update. */
        fun hello(version:Long,capabilities:Set<String>)=if(version!=1L || capabilities.none {it in setOf("files","clipboard_text","clipboard_image")}) UPDATE_REQUIRED else null
    }
}

data class IncomingTransfer(val peerId:String,val transferId:String,val fileName:String,val received:Long,val total:Long)

/** Legacy feedback is classified once at the connection boundary; UI uses the type. */
enum class PairingIssue { NONE, UNREACHABLE, CLOCK, FEEDBACK;
    companion object {
        fun fromMessage(message:String)=when {
            message=="Can't reach your PC" -> UNREACHABLE
            message.startsWith("Your PC's clock") -> CLOCK
            message.isEmpty() || message=="Pairing…" || message.startsWith("Paired with ") -> NONE
            else -> FEEDBACK
        }
    }
}
