package io.github.stantonmatt.pneu.features

import android.app.*
import android.content.*
import android.graphics.Color
import android.graphics.drawable.Icon
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.lifecycleScope
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.protocol.*
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

object ManualEntries {
    private const val PHONE="pneu.admission.phone"
    private const val PIN="pneu.admission.pin"
    private const val PC="pneu.admission.pc"
    fun bind(intent:Intent,p:Prefs):Intent=DeliveryBinding.from(p)?.let {binding->
        intent.setIdentifier("admission:${binding.phoneId}:${binding.pin}:${binding.pcId}").putExtra(PHONE,binding.phoneId).putExtra(PIN,binding.pin).putExtra(PC,binding.pcId)
    } ?: intent.putExtra(PHONE,"")
    fun admission(intent:Intent,p:Prefs):DeliveryBinding? = if(intent.hasExtra(PHONE)) {
        val phone=intent.getStringExtra(PHONE).orEmpty();val pin=intent.getStringExtra(PIN).orEmpty();val pc=intent.getStringExtra(PC).orEmpty()
        if(phone.isEmpty() || pin.isEmpty() || pc.isEmpty())null else DeliveryBinding(phone,pin,pc)
    } else DeliveryBinding.from(p)
    fun clipboardIntent(context:Context,p:Prefs)=bind(Intent(context,SendClipboardActivity::class.java),p)
    suspend fun text(app:AppGraph,text:String,origin:String):Outgoing=text(app,text,origin,DeliveryBinding.from(app.awaitPrefs()))
    suspend fun text(app: AppGraph,text: String,origin: String,binding:DeliveryBinding?): Outgoing {
        if(binding==null)throw ShareFailure("binding_mismatch")
        val timestamp=shareStep("settings") {app.newTextTimestamp()}
        val row=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();kind="text";automatic=false;this.text=text;this.origin=origin;createdAt=timestamp;mime="text/plain"}
        shareStep("invalid_text") {
            Protocol.validate(Type.CLIPBOARD,json("transfer_id" to row.transferId,"text" to text,"created_at" to row.createdAt,"origin" to origin))
            Protocol.write(java.io.ByteArrayOutputStream(),Frame.Control(Type.CLIPBOARD,json("transfer_id" to row.transferId,"text" to text,"created_at" to row.createdAt,"origin" to origin)))
        }
        if(!shareStep("storage_failed") {app.enqueue(row,binding)})throw ShareFailure("binding_mismatch")
        app.connection.kick();return row
    }
    suspend fun file(app:AppGraph,uri:Uri,kind:String):Outgoing=file(app,uri,kind,DeliveryBinding.from(app.awaitPrefs()))
    suspend fun file(app: AppGraph,uri: Uri,kind: String,binding:DeliveryBinding?,resolvedMime:String?=null): Outgoing {
        if(binding==null || !binding.matches(shareStep("settings") {app.preferences.read()}))throw ShareFailure("binding_mismatch")
        shareStep("metadata_failed") {ShareAdmission.rejection(app.context,uri)}?.let {throw ShareFailure(it)}
        val type=resolvedMime ?: shareStep("metadata_failed") {ProviderReads.call {app.context.contentResolver.getType(uri)}}
        val row=Outgoing().apply {mediaId="manual:${randomId()}";transferId=randomId();this.uri=uri.toString();this.kind=kind;automatic=false;createdAt=System.currentTimeMillis();mime=type ?: "application/octet-stream";fileName=app.context.getString(R.string.shared_item)}
        row.fileName=shareStep("metadata_failed") {ProviderReads.call {signal->app.context.contentResolver.query(uri,arrayOf(OpenableColumns.DISPLAY_NAME),null,null,null,signal)?.use {if(it.moveToFirst())it.getString(0) else null}}} ?: row.fileName
        row.fileName=safeFilename(row.fileName)
        shareStep("read_failed") {app.staging.capture(row,binding)};app.connection.kick();return row
    }
    fun configureShareTarget(context:Context,enabled:Boolean) {
        context.packageManager.setComponentEnabledSetting(ComponentName(context,ShareActivity::class.java),if(enabled)android.content.pm.PackageManager.COMPONENT_ENABLED_STATE_ENABLED else android.content.pm.PackageManager.COMPONENT_ENABLED_STATE_DISABLED,android.content.pm.PackageManager.DONT_KILL_APP)
    }
    fun clearLegacyShortcuts(context:Context) {
        context.getSystemService(android.content.pm.ShortcutManager::class.java).apply {removeAllDynamicShortcuts();removeLongLivedShortcuts(listOf("pc"))}
    }

}

abstract class ManualEntryActivity : ComponentActivity() {
    internal var operation:ManualEntryViewModel?=null
        private set
    internal val operationReady=CompletableDeferred<ManualEntryViewModel>()
    override fun onCreate(state:Bundle?) {
        super.onCreate(state)
        val operation=ViewModelProvider(this)[ManualEntryViewModel::class.java]
        this.operation=operation
        // A process-death restore has no retained operation. Fail closed rather
        // than replaying an intent whose earlier items may already be in Room.
        if(state!=null && !operation.started)operation.interrupted(if(this is SendClipboardActivity)getString(R.string.toast_clipboard_interrupted) else getString(R.string.toast_share_interrupted))
        onBackPressedDispatcher.addCallback(this,object:OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {moveTaskToBack(true)}
        })
        lifecycleScope.launch {
            val result=operation.result.filterNotNull().first()
            if(result.pair)startActivity(Intent(this@ManualEntryActivity,MainActivity::class.java).putExtra("pair",true))
            result.message?.let {Toast.makeText(this@ManualEntryActivity,it,Toast.LENGTH_LONG).show()}
            finish()
        }
        operationReady.complete(operation)
    }
}
class SendClipboardActivity : ManualEntryActivity() {
    private var focused=false
    private var bindingReady=false
    override fun onCreate(state:Bundle?) {
        // Internal buttons/tiles/notifications embed their original binding.
        // Capture unbound explicit entries here too, before waiting for focus.
        val source=Intent(intent)
        val binding=graph.captureBinding {ManualEntries.admission(source,it)}
        super.onCreate(state)
        requireNotNull(operation).captureClipboardBinding(binding)
        bindingReady=true
        if(focused)sendFocusedClipboard()
    }
    override fun onWindowFocusChanged(hasFocus:Boolean) {
        super.onWindowFocusChanged(hasFocus)
        focused=hasFocus
        if(hasFocus)sendFocusedClipboard()
    }
    private fun sendFocusedClipboard() {
        val operation=operation ?: return
        if(!bindingReady || operation.started)return
        try {
            // Clipboard access still happens only in the focused Activity.
            operation.clipboard(getSystemService(ClipboardManager::class.java).primaryClip)
        }catch(_:Exception) {operation.failure(getString(R.string.toast_not_sent))}
    }
}
class ShareActivity : ManualEntryActivity() {
    override fun onCreate(state:Bundle?) {
        // External callers cannot choose a recipient. Capture our current
        // binding before lifecycle/admission work can suspend or touch a provider.
        val binding=graph.captureBinding()
        super.onCreate(state)
        val operation=requireNotNull(operation)
        if(operation.started)return
        if(intent.hasExtra(Intent.EXTRA_SHORTCUT_ID)) {ShareAdmission.warn("shortcut");finish();return}
        if(!ShareAdmission.supported(intent.action)) {ShareAdmission.warn("unsupported_action");finish();return}
        var failureReason="invalid_intent"
        try {
            val streams=if(intent.action==Intent.ACTION_SEND_MULTIPLE)intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM,Uri::class.java).orEmpty()
                else listOfNotNull(intent.getParcelableExtra(Intent.EXTRA_STREAM,Uri::class.java))
            val items=if(streams.isNotEmpty())streams.toList() else (0 until (intent.clipData?.itemCount ?: 0)).mapNotNull {intent.clipData?.getItemAt(it)?.uri}
            val texts=if(intent.action==Intent.ACTION_SEND_MULTIPLE)intent.getCharSequenceArrayListExtra(Intent.EXTRA_TEXT)?.map {it.toString()}.orEmpty()
                else listOfNotNull(intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString())
            failureReason="metadata_failed"
            items.firstNotNullOfOrNull {ShareAdmission.rejection(this,it)}?.let {
                ShareAdmission.warn(it);operation.failure(getString(R.string.toast_cant_read),it);return
            }
            // The bounded provider open/read proves access, including chooser
            // grants or our media permission. Keep temporary grants service-owned.
            val request=ShareRequest(binding,items,texts)
            failureReason="service_start"
            ShareAdmissionService.start(this,operation,request)
        }catch(_:Exception) {ShareAdmission.warn(failureReason);operation.failure(getString(R.string.toast_cant_read),failureReason)}
    }
}

abstract class PneuTileBase : TileService() {
    protected val scope=CoroutineScope(SupervisorJob()+Dispatchers.Main.immediate)
    private var listening: Job?=null
    override fun onStartListening() {
        super.onStartListening();listening?.cancel()
        listening=scope.launch {combine(graph.prefs,graph.connection.state) {p,s->p to s}.collect {(p,s)->if(p.loaded)render(p,s) else qsTile?.apply {state=Tile.STATE_UNAVAILABLE;updateTile()}}}
    }
    abstract fun render(p: Prefs,s: io.github.stantonmatt.pneu.connection.LinkState)
    override fun onStopListening() {listening?.cancel();super.onStopListening()}
    protected fun open(activity: Class<*>,p:Prefs) {val intent=if(activity==SendClipboardActivity::class.java)ManualEntries.clipboardIntent(this,p) else Intent(this,activity);startActivityAndCollapse(PendingIntent.getActivity(this,0,intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK).putExtra("home",true),PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT))}
    override fun onDestroy() {scope.cancel();super.onDestroy()}
}
class PneuTile : PneuTileBase() {
    override fun render(p: Prefs,s: io.github.stantonmatt.pneu.connection.LinkState) {
        val presentation=TilePolicy.present(p,s.status,s.route)
        qsTile?.apply {
            label=this@PneuTile.getString(R.string.tile_pneu);icon=Icon.createWithResource(this@PneuTile,R.drawable.ic_stat_pneu)
            state=when {!presentation.available->Tile.STATE_UNAVAILABLE;presentation.active->Tile.STATE_ACTIVE;else->Tile.STATE_INACTIVE}
            subtitle=presentation.subtitle?.let {getString(it)};updateTile()
        }
    }
    override fun onClick() {super.onClick();scope.launch {UiActions.feedback(this@PneuTile).run(getString(R.string.toast_settings)) {
        val p=graph.awaitPrefs()
        if(!p.quickSettings)return@run
        if(p.pairingLost)open(MainActivity::class.java,p) else if(p.peer!=null)UiActions.feedback(this@PneuTile).run(getString(R.string.toast_pause)) {graph.preferences.toggle("paused",!p.paused);PneuService.start(this@PneuTile)}
    }}}
}
class ClipboardTile : PneuTileBase() {
    override fun render(p: Prefs,s: io.github.stantonmatt.pneu.connection.LinkState) {
        val presentation=TilePolicy.present(p,s.status,s.route,clipboard=true)
        qsTile?.apply {label=this@ClipboardTile.getString(R.string.tile_send_clipboard);icon=Icon.createWithResource(this@ClipboardTile,R.drawable.ic_content_paste_go);state=if(presentation.available)Tile.STATE_INACTIVE else Tile.STATE_UNAVAILABLE;subtitle=presentation.subtitle?.let {getString(it)};updateTile()}
    }
    override fun onClick() {super.onClick();scope.launch {UiActions.feedback(this@ClipboardTile).run(getString(R.string.toast_settings)) {
        val p=graph.awaitPrefs();if(!p.quickSettings)return@run
        if(p.pairingLost)open(MainActivity::class.java,p) else if(p.peer!=null)open(SendClipboardActivity::class.java,p)
    }}}
}
