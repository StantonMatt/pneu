package io.github.stantonmatt.pneu.protocol

import com.google.gson.*
import com.google.gson.annotations.SerializedName

const val CAP_FILE_RESUME = "file_resume_v1"
const val CAP_FOLDER = "folder_v1"
const val CAP_BATTERY = "battery_v1"
const val CAP_RING = "ring_v1"
const val CAP_NOTIFICATIONS = "notifications_v1"
const val FOLDER_MIME = "application/vnd.pneu.folder"
const val MAX_MANIFEST_BYTES = 8_388_608
const val MAX_MANIFEST_ENTRIES = 10_000
const val MAX_MANIFEST_JSON_DEPTH = 64
const val MAX_FOLDER_DEPTH = 32
const val MAX_FOLDER_PATH_BYTES = 4096
const val MAX_FOLDER_COMPONENT_BYTES = 255
const val MAX_NOTIFICATION_ICON_BYTES = 8192
// Codec definitions alone must never opt a session into unfinished features.
val LEGACY_PHONE_CAPABILITIES = listOf("files","clipboard_text","clipboard_image","mesh","setup")

data class Folder(@SerializedName("manifest_len") val manifestLen:Long)
data class FileReady(@SerializedName("transfer_id") val transferId:String,val offset:Long) {
    fun validateOffset(beginSize:Long) { require(beginSize in 0..MAX_FILE && offset in 0..beginSize) }
}
data class Cancel(@SerializedName("transfer_id") val transferId:String)
data class BatteryState(val level:Int?=null,val charging:Boolean?=null)
enum class RingAction { @SerializedName("start") START, @SerializedName("stop") STOP }
data class Ring(val action:RingAction)
data class RingState(val ringing:Boolean)
enum class NotificationAction { @SerializedName("clear") CLEAR, @SerializedName("remove") REMOVE, @SerializedName("upsert") UPSERT }
data class NotificationEvent(val action:NotificationAction,val key:String?=null,@SerializedName("app_package") val appPackage:String?=null,
    @SerializedName("app_label") val appLabel:String?=null,val time:Long?=null,val title:String?=null,val text:String?=null,@SerializedName("icon_png") val iconPng:String?=null)
enum class FolderEntryType { @SerializedName("file") FILE, @SerializedName("directory") DIRECTORY }
data class FolderEntry(val path:String,val type:FolderEntryType,val size:Long?=null,val sha256:String?=null)
data class FolderManifest(val entries:List<FolderEntry>) {
    fun validate(manifestLen:Long,streamSize:Long) {
        require(manifestLen in 1..MAX_MANIFEST_BYTES.toLong() && streamSize in manifestLen..MAX_FILE)
        require(entries.size <= MAX_MANIFEST_ENTRIES)
        val paths=mutableSetOf<String>();val directories=mutableSetOf<String>();var total=manifestLen
        for(entry in entries) {
            validateFolderPath(entry.path)
            require(paths.add(entry.path))
            if('/' in entry.path)require(entry.path.substringBeforeLast('/') in directories)
            when(entry.type) {
                FolderEntryType.DIRECTORY -> {require(entry.size==null && entry.sha256==null);directories.add(entry.path)}
                FolderEntryType.FILE -> {
                    val size=requireNotNull(entry.size);require(size in 0..MAX_FILE)
                    require(entry.sha256?.matches(Regex("[0-9a-f]{64}"))==true)
                    require(size != 0L || entry.sha256 == sha256(byteArrayOf()))
                    require(size <= MAX_FILE-total);total+=size
                }
            }
        }
        require(total==streamSize)
    }
    companion object {
        fun parse(bytes:ByteArray,begin:JsonObject):FolderManifest {
            Protocol.validate(Type.FILE_BEGIN,begin)
            val m=begin["folder"].asJsonObject["manifest_len"].asLong
            require(bytes.size <= MAX_MANIFEST_BYTES && bytes.size.toLong()==m)
            val o=Protocol.parseObject(bytes,setOf("entries"),maxDepth=MAX_MANIFEST_JSON_DEPTH,strictUnknownUnicode=true)
            val entries=o["entries"];require(entries is JsonArray && entries.size() <= MAX_MANIFEST_ENTRIES)
            val parsed=entries.map { e ->
                require(e is JsonObject)
                val path=ExtensionWire.string(e["path"],MAX_FOLDER_PATH_BYTES)
                val type=when(ExtensionWire.string(e["type"],9)) {"file"->FolderEntryType.FILE;"directory"->FolderEntryType.DIRECTORY;else->error("Entry type")}
                val size=if(e.has("size"))ExtensionWire.number(e["size"],MAX_FILE) else null
                val hash=if(e.has("sha256"))ExtensionWire.string(e["sha256"],64).also {require(it.matches(Regex("[0-9a-f]{64}")))} else null
                FolderEntry(path,type,size,hash)
            }
            return FolderManifest(parsed).also {it.validate(m,begin["size"].asLong)}
        }
    }
}
fun validateFolderPath(path:String) {
    ExtensionWire.string(JsonPrimitive(path),MAX_FOLDER_PATH_BYTES)
    require('\\' !in path)
    val parts=path.split('/');require(parts.size <= MAX_FOLDER_DEPTH)
    for(part in parts) {
        ExtensionWire.string(JsonPrimitive(part),MAX_FOLDER_COMPONENT_BYTES)
        require(part!="." && part!=".." && !Regex("^[A-Za-z]:").containsMatchIn(part))
    }
}
internal object ExtensionWire {
    val notificationFields=setOf("action","key","app_package","app_label","time","title","text","icon_png")
    fun string(e:JsonElement?,max:Int,min:Int=1):String {
        require(e is JsonPrimitive && e.isString)
        return e.asString.also {v ->
            require(v.toByteArray(Charsets.UTF_8).size in min..max && '\u0000' !in v)
            var i=0
            while(i<v.length) {val c=v[i++];if(c.isHighSurrogate())require(i<v.length && v[i++].isLowSurrogate()) else require(!c.isLowSurrogate())}
        }
    }
    fun number(e:JsonElement?,max:Long):Long {
        require(e is JsonPrimitive && e.isNumber && e.asString.matches(Regex("0|[1-9][0-9]*")))
        return e.asString.toBigInteger().also {require(it <= max.toBigInteger())}.longValueExact()
    }
    private fun bool(e:JsonElement?) {require(e is JsonPrimitive && e.isBoolean)}
    private fun id(e:JsonElement?) {require(string(e,32).matches(Regex("[0-9a-f]{32}")))}
    fun validateFolderBegin(o:JsonObject) {
        val f=o["folder"];require(f is JsonObject)
        val m=number(f["manifest_len"],MAX_MANIFEST_BYTES.toLong());require(m>=1 && m<=o["size"].asLong)
        require(o["resume"]?.let {it is JsonPrimitive && it.isBoolean && it.asBoolean}==true)
        require(o["kind"].asString=="file" && o["mime"].asString==FOLDER_MIME)
        require(listOf("source_media_id","hlc","source_device","sensitive").none(o::has))
    }
    fun validate(type:Type,o:JsonObject) {
        when(type) {
            Type.FILE_READY -> {id(o["transfer_id"]);number(o["offset"],MAX_FILE)}
            Type.CANCEL -> id(o["transfer_id"])
            Type.BATTERY_STATE -> {
                require(o.has("level")==o.has("charging"))
                if(o.has("level")) {number(o["level"],100);bool(o["charging"])}
            }
            Type.RING -> require(string(o["action"],5) in setOf("start","stop"))
            Type.RING_STATE -> bool(o["ringing"])
            Type.NOTIFICATION_EVENT -> {
                val action=string(o["action"],6)
                val allowed=when(action) {
                    "clear"->setOf("action")
                    "remove"->setOf("action","key")
                    "upsert"->notificationFields
                    else->error("Notification action")
                }
                require(notificationFields.none {it !in allowed && o.has(it)})
                if(action!="clear")id(o["key"])
                if(action=="upsert") {
                    string(o["app_package"],255);string(o["app_label"],128);number(o["time"],MAX_TIMESTAMP)
                    if(o.has("title"))string(o["title"],1024,0)
                    if(o.has("text"))string(o["text"],8192,0)
                    if(o.has("icon_png")) {
                        val icon=string(o["icon_png"],MAX_NOTIFICATION_ICON_BYTES*2,2)
                        require(icon.length%2==0 && icon.matches(Regex("[0-9a-f]+")))
                        // Invalid bounded PNG may render with a generic icon. Decode/pixel caps are renderer responsibilities.
                    }
                }
            }
            else->error("Not an extension control")
        }
    }
}
