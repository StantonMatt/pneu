package io.github.stantonmatt.pneu

import io.github.stantonmatt.pneu.connection.ConnectionStatus
import io.github.stantonmatt.pneu.connection.PairingIssue
import android.Manifest
import android.app.Activity
import android.content.*
import android.net.Uri
import android.os.Bundle
import android.os.Build
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.*
import androidx.activity.enableEdgeToEdge
import androidx.lifecycle.lifecycleScope
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.Crossfade
import androidx.compose.animation.AnimatedContent
import androidx.activity.SystemBarStyle
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.semantics.*
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.foundation.selection.toggleable
import io.github.stantonmatt.pneu.ui.*
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.*
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.flow.first
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.platform.LocalResources
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.togetherWith
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.*
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.connection.RoutePolicy
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.Invitation
import kotlinx.coroutines.*
import java.text.SimpleDateFormat
import java.util.*

class MainActivity : ComponentActivity() {
    private var requestedScreen by mutableStateOf<String?>(null)
    private var refresh by mutableIntStateOf(0)
    private var pairingCandidate by mutableStateOf<String?>(null)
    private var requestedRemove by mutableStateOf<String?>(null)
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState); enableEdgeToEdge(SystemBarStyle.auto(android.graphics.Color.TRANSPARENT,android.graphics.Color.TRANSPARENT),SystemBarStyle.auto(android.graphics.Color.TRANSPARENT,android.graphics.Color.TRANSPARENT))
        if(savedInstanceState==null)handleIntent(intent)
        else {pairingCandidate=savedInstanceState.getString("candidate");requestedScreen=savedInstanceState.getString("screen");requestedRemove=savedInstanceState.getString("remove")}
        setContent { PneuScreen(requestedScreen,refresh,{requestedScreen=null},pairingCandidate,requestedRemove,{requestedRemove=null}) {pairingCandidate=it} }
    }
    override fun onSaveInstanceState(outState:Bundle) {
        outState.putString("candidate",pairingCandidate);outState.putString("screen",requestedScreen);outState.putString("remove",requestedRemove)
        super.onSaveInstanceState(outState)
    }
    override fun onNewIntent(intent: Intent) { super.onNewIntent(intent); setIntent(intent); handleIntent(intent) }
    private fun handleIntent(intent: Intent) {
        if(intent.action==Intent.ACTION_VIEW && intent.data!=null) {pairingCandidate=intent.data.toString()}
        if(intent.getBooleanExtra("home",false) || intent.action=="android.service.quicksettings.action.QS_TILE_PREFERENCES")requestedScreen="home"
        if(intent.getBooleanExtra("permissions",false)) requestedScreen="permissions"
        if(intent.getBooleanExtra("pair",false)) requestedScreen="pair"
        // Joined notifications open a device page, and Remove asks there.
        intent.getStringExtra("page")?.takeIf {it.startsWith(DEVICE) || it.startsWith(MEMBER)}?.let {requestedScreen=it}
        intent.getStringExtra("remove")?.let {requestedRemove=it}
        intent.getStringExtra("cancel")?.let {getSystemService(android.app.NotificationManager::class.java).cancel(it,0)}
    }
    override fun onResume() { super.onResume(); visible=true; foreground.value=true; refresh++;graph.requestMediaScan(); lifecycleScope.launch {UiActions.feedback(this@MainActivity).run(getString(R.string.toast_settings)) {val p=graph.awaitPrefs();if((graph.connection.attempt.value!=null || p.peer!=null && !p.pairingLost))PneuService.start(this@MainActivity)}} }
    override fun onPause() { visible=false; foreground.value=false; super.onPause() }
    companion object { @Volatile var visible=false
        /** Pneu in front shows link requests and joins itself instead of notifying. */
        val foreground=kotlinx.coroutines.flow.MutableStateFlow(false) }
}
suspend fun beginPair(context: android.content.Context, uri: String, expected: ConfirmedBinding?=null, setupEnabled: Boolean=true): Boolean {
    val graph=context.graph
    val invitation=runCatching { Invitation.parse(uri) }.getOrNull()
    val active=graph.connection.attempt.value
    if(invitation == null || invitation.version != 1L) { graph.connection.state.update { it.withPairing(context.getString(R.string.pair_not_pneu)) }; return false }
    if(invitation != active?.invitation && invitation.expires <= System.currentTimeMillis()) { graph.connection.state.update { it.withPairing(PairingIssue.EXPIRED_MESSAGE) }; return false }
    check(graph.requestPairing(uri,expected ?: ConfirmedBinding.forInvitation(graph.prefs.value,invitation),setupEnabled)) {"Binding changed during pairing"}
    PneuService.start(context)
    return true
}
fun lastConnectedLabel(context: android.content.Context, timestamp: Long): String {
    if(timestamp == 0L) return "—"
    val now=Calendar.getInstance(); val date=Calendar.getInstance().apply { timeInMillis=timestamp }
    if(now.get(Calendar.YEAR)==date.get(Calendar.YEAR) && now.get(Calendar.DAY_OF_YEAR)==date.get(Calendar.DAY_OF_YEAR)) return android.text.format.DateFormat.getTimeFormat(context).format(Date(timestamp))
    now.add(Calendar.DAY_OF_YEAR,-1)
    if(now.get(Calendar.YEAR)==date.get(Calendar.YEAR) && now.get(Calendar.DAY_OF_YEAR)==date.get(Calendar.DAY_OF_YEAR)) return context.getString(R.string.time_yesterday)
    return SimpleDateFormat("d MMM",Locale.getDefault()).format(Date(timestamp))
}

/** Device pages are "device:{id}"; another phone in your devices is "member:{roster key}". */
internal const val DEVICE="device:"
internal const val MEMBER="member:"

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun PneuScreen(requested: String? = null, refresh: Int = 0, consume: () -> Unit = {},candidate:String?=null,removeRequest:String?=null,consumeRemove:()->Unit={},offerPair:(String?)->Unit={}) {
    val context=LocalContext.current;val resources=LocalResources.current; val app=context.graph; val scope=rememberCoroutineScope()
    val p by app.prefs.collectAsState(); val link by app.connection.state.collectAsState(); val recentModel=remember(context) {androidx.lifecycle.ViewModelProvider(context as ComponentActivity)[RecentViewModel::class.java]}
    val recentItems by recentModel.items.collectAsState()
    val devicesModel=remember(context) {androidx.lifecycle.ViewModelProvider(context as ComponentActivity)[DevicesViewModel::class.java]}
    val deviceStates by devicesModel.devices.collectAsState()
    val recent=recentItems.map {it.row}
    val dark=isSystemInDarkTheme()
    if(!p.loaded || p.deviceId.isEmpty()) {
        var failed by remember {mutableStateOf(false)}
        var attempt by remember {mutableIntStateOf(0)}
        LaunchedEffect(attempt) {
            failed=false
            try {storageDeadline {app.prefs.first {it.loaded && it.deviceId.isNotEmpty()}}}catch(e:Exception) {if(e is CancellationException)throw e;failed=true}
        }
        PneuTheme(dark=dark) {Surface(Modifier.fillMaxSize()) {Box(Modifier.fillMaxSize(),contentAlignment=Alignment.Center) {
            if(failed)Column(horizontalAlignment=Alignment.CenterHorizontally) {Text(resources.getString(R.string.load_failed));PneuTextButton(onClick={attempt++}) {Text(resources.getString(R.string.action_try_again))}}
            else CircularProgressIndicator(Modifier.size(36.dp),color=LocalPneuColors.current.accent,trackColor=LocalPneuColors.current.accentSoft,strokeWidth=3.dp)
        }}}
        return
    }
    var screen by rememberSaveable { mutableStateOf<String?>(null) }
    var lastAttemptedInvitation by rememberSaveable(p.deviceId) {mutableStateOf<String?>(null)}
    var scannerAttemptId by rememberSaveable(p.deviceId) {mutableStateOf<String?>(null)}
    var scannerPairingRevision by rememberSaveable(p.deviceId) {mutableStateOf<Long?>(null)}
    var scannerSession by rememberSaveable(p.deviceId) {mutableIntStateOf(0)}
    var rejectedLinkFeedback by rememberSaveable(p.deviceId,stateSaver=UiFeedbackSaver) {mutableStateOf<UiFeedback?>(null)}
    fun navigate(next:String) {
        if(next!="pair") {
            // The approved pre-install floor belongs to this scanner session.
            // A later pair intent must start from the binding now installed.
            scannerPairingRevision=null
            scannerAttemptId=null
            rejectedLinkFeedback=null
        }
        // AnimatedContent may still retain the leaving scanner when another
        // intent arrives. A new entry must not reuse its saved local floor.
        if(next=="pair" && screen!="pair")scannerSession++
        screen=next
    }
    DisposableEffect(screen,dark) {
        val activity=context as Activity
        androidx.core.view.WindowCompat.getInsetsController(activity.window,activity.window.decorView).apply {
            isAppearanceLightStatusBars=screen != "pair" && !dark
            isAppearanceLightNavigationBars=screen != "pair" && !dark
        }
        onDispose {}
    }
    var permissionOrigin by rememberSaveable {mutableStateOf("home")}
    val historyState by app.savedClips.state.collectAsState()
    val waiting by app.waitingCount.collectAsState()
    val pendingDeliveries by app.work.collectAsState()
    val pairingAttempt by app.connection.attempt.collectAsState()
    var unpair by remember { mutableStateOf<ConfirmedBinding?>(null) }
    var actionError by remember {mutableStateOf<Pair<UiFeedback,(() -> Unit)?>?>(null)}
    val actions=remember {UiActions {message,retry->actionError=UiFeedback(message) to retry}}
    // Linking your devices: requests, Remove from all devices, and Home's one-time messages.
    val setupStore=context.setupUi
    val setupUi by setupStore.state.collectAsState()
    val dismissedPrompts by setupStore.dismissedPrompts.collectAsState()
    val homeSnackbar=remember {SnackbarHostState()}
    val lifecycleState by androidx.lifecycle.compose.LocalLifecycleOwner.current.lifecycle.currentStateFlow.collectAsState()
    val resumed=lifecycleState.isAtLeast(androidx.lifecycle.Lifecycle.State.RESUMED)
    val displayedPrompts=SetupPresentation.prompts(p)
    val displayedAllPrompts=p.membership.prompts.toList()
    val displayedBindings=p.devices.associate {it.deviceId to ConfirmedBinding.from(p,it.deviceId)}
    var showingNews by remember {mutableStateOf<SetupNews?>(null)}
    suspend fun presentedNews(data:SnackbarData) {
        val item=showingNews ?: return
        if(data.visuals.message!=SetupPresentation.newsText(resources,item))return
        withContext(NonCancellable) {
            when(item) {
                is SetupNews.AlsoLinked->{setupStore.presentNotices(item.notices.toSet());runCatching {app.dismissSetupNotices(item.notices.toSet())};setupStore.update {it.copy(joinedPin="",joinedAt=0)}}
                is SetupNews.Joined->{setupStore.presentNotices(setOf(item.notice.id));runCatching {app.dismissSetupNotices(setOf(item.notice.id))}}
                is SetupNews.Separate->setupStore.update {it.copy(announcedSeparate=it.announcedSeparate+item.peerId)}
                SetupNews.Linked->setupStore.update {it.copy(linkStarted=false)}
            }
        }
    }
    var removing by rememberSaveable(stateSaver=ConfirmedSetupMemberSaver) {mutableStateOf<ConfirmedSetupMember?>(null)}
    var removeFailed by rememberSaveable {mutableStateOf<String?>(null)}
    var answering by remember {mutableStateOf(emptySet<String>())}
    fun askRemove(key:String) {removing=runCatching {devicesModel.confirmRemoveFromAllDevices(key)}.getOrNull()}
    fun linkDevices(confirmed:ConfirmedBinding,displayedPrompts:List<SetupPrompt>) {
        setupStore.update {it.copy(joinedPin="",joinedAt=0)}
        val targets=SetupPresentation.linkTargets(p,confirmed.pcId)
        scope.launch {if(actions.run(resources.getString(R.string.ui_error_link)) {devicesModel.linkExistingDevice(confirmed,displayedPrompts)})setupStore.update {it.copy(linkStarted=true,linkTargets=targets)}}
    }
    /** One Allow covers every device that asked, and this phone's other undecided pairings. */
    fun answer(prompts:List<SetupPrompt>,allow:Boolean) {
        val keys=prompts.map(SetupPresentation::promptKey).toSet()
        if(allow)setupStore.update {it.copy(joinedPin="",joinedAt=0)}
        answering=answering+keys
        scope.launch {
            actions.run(resources.getString(R.string.ui_error_link)) {app.answerSetupRequests(keys,allow);if(allow)PneuService.start(context)}
            answering=answering-keys
        }
    }
    LaunchedEffect(removeRequest) {removeRequest?.let {askRemove(it);consumeRemove()}}
    var health by remember { mutableStateOf(Health(true,true,true,true)) }
    LaunchedEffect(refresh,p.recordings,p.neverSleeping) { health=withContext(Dispatchers.IO) {context.health()} }
    LaunchedEffect(p.setup,p.deviceId,SetupPresentation.hasDevices(p)) { if(screen == null && p.deviceId.isNotEmpty()) navigate(if(!p.setup) "setup" else if(!SetupPresentation.hasDevices(p) && !p.pairingLost) "pair" else "home") }
    LaunchedEffect(requested) { requested?.let { navigate(it); consume() } }
    LaunchedEffect(p.peer,pairingAttempt,p.pairingLost,SetupPresentation.hasDevices(p)) { if(pairingAttempt!=null || !p.pairingLost && p.peer != null && screen != "pair") actions.run(resources.getString(R.string.ui_error_connection)) {PneuService.start(context)}
        if(!SetupPresentation.hasDevices(p) && !p.pairingLost && p.setup && screen == "home") navigate("pair") }
    var filesFor by rememberSaveable(stateSaver=CapturedAuthoritySaver) {mutableStateOf<CapturedAuthority?>(null)}
    val files=rememberLauncherForActivityResult(ActivityResultContracts.GetMultipleContents()) {uris->
        val authority=filesFor;filesFor=null
        if(uris.isNotEmpty())runCatching {sendFilesTo(context as Activity,requireNotNull(authority),uris)}
            .onFailure {actionError=UiFeedback(resources.getString(R.string.toast_cant_read)) to null}
    }
    fun sendFiles(id:String) {
        runCatching {
            filesFor=CapturedAuthority(requireNotNull(DeliveryBinding.from(p,id)))
            files.launch("*/*")
        }.onFailure {filesFor=null;actionError=UiFeedback(resources.getString(R.string.toast_cant_read)) to null}
    }
    var backProgress by remember {mutableFloatStateOf(0f)}
    PredictiveBackHandler(enabled=screen in listOf("settings","permissions","history") || screen?.startsWith(DEVICE)==true || screen?.startsWith(MEMBER)==true || (screen=="pair" && SetupPresentation.hasDevices(p))) { events ->
        try {events.collect {backProgress=it.progress};navigate(if(screen=="permissions") permissionOrigin else "home")}
        finally {backProgress=0f}
    }
    CompositionLocalProvider(LocalUiActions provides actions) {
    PneuTheme(dark=dark) {
        val background=LocalPneuColors.current.bg
        Surface(Modifier.fillMaxSize().graphicsLayer {scaleX=1f-backProgress*.06f;scaleY=scaleX;shape=RoundedCornerShape((24*backProgress).dp);clip=backProgress>0f},color=background) {
            AnimatedContent(screen,transitionSpec={fadeIn(androidx.compose.animation.core.tween(PneuMotion.short)) togetherWith fadeOut(androidx.compose.animation.core.tween(PneuMotion.short))},label="Screen") { visibleScreen ->
            when(visibleScreen) {
                "setup","permissions" -> SetupScreen(visibleScreen=="setup",health,refresh,{ navigate(permissionOrigin) },{scope.launch {health=withContext(Dispatchers.IO) {context.health()}}}) {
                    scope.launch {if(actions.run(resources.getString(R.string.ui_error_setup)) {app.preferences.toggle("setup",true)})navigate("pair")}
                }
                "pair" -> {
                    val session=scannerSession
                    key(session) {PairScreen(p,link,{ if(screen=="pair" && session==scannerSession)navigate("home") },{ if(SetupPresentation.hasDevices(p))navigate("home") else (context as Activity).finish() },{offerPair(it)},lastAttemptedInvitation=lastAttemptedInvitation,initialPairingRevision=scannerPairingRevision,initialAttemptId=scannerAttemptId,validationFeedback=rejectedLinkFeedback)}
                }
                "home" -> {var homeScrolled by remember {mutableStateOf(false)};Scaffold(containerColor=background,contentWindowInsets=WindowInsets.statusBars,topBar={HomeTopBar(homeScrolled,history=if(historyState.enabled)({navigate("history")})else null){navigate("settings")}},snackbarHost={PneuSnackbarHost(homeSnackbar,Modifier.navigationBarsPadding(),::presentedNews)}) { padding ->
                    HomeScreen(p,link,recent,health,Modifier.padding(padding),{permissionOrigin="home";navigate("permissions")},{navigate("pair")},{homeScrolled=it},recentItems.associateBy {it.row.mediaId},deviceStates,
                        setup=HomeSetup(SetupPresentation.banner(p,setupUi),SetupPresentation.phones(p),{id->displayedBindings[id]?.let {linkDevices(it,displayedAllPrompts)}},{setupStore.update {it.copy(bannerDismissed=true)}}) {navigate(MEMBER+it)}) {navigate(DEVICE+it)}
                }}
                "history" -> {
                    LaunchedEffect(historyState.enabled) {if(!historyState.enabled)navigate("home")}
                    if(historyState.enabled)ClipboardHistoryScreen {navigate("home")}
                }
                "settings" -> SettingsScreen(p,health,refresh,{navigate("home")},{permissionOrigin="settings";navigate("permissions")})
                else -> if(visibleScreen?.startsWith(MEMBER)==true) {
                    val key=visibleScreen.removePrefix(MEMBER)
                    val member=SetupPresentation.phones(p).find {it.key==key}
                    LaunchedEffect(member==null) {if(member==null && screen==visibleScreen)navigate("home")}
                    if(member!=null)PhoneMemberScreen(member,{navigate("home")}) {askRemove(key)}
                } else visibleScreen?.removePrefix(DEVICE)?.takeIf {visibleScreen.startsWith(DEVICE)}?.let {id->
                    // Unpaired here or on the other device: the page has nothing left to show.
                    val state=deviceStates.find {it.peer.deviceId==id}
                    LaunchedEffect(state==null,p.devices) {if(state==null && p.devices.none {it.deviceId==id} && screen==visibleScreen)navigate("home")}
                    if(state!=null)DeviceScreen(p,state,{navigate("home")},{navigate("pair")},{unpair=ConfirmedBinding.from(p,id)},{sendFiles(id)},
                        SetupActions(link={displayedBindings[id]?.let {linkDevices(it,displayedAllPrompts)}},allow={answer(displayedPrompts,true)},remove={SetupPresentation.memberKey(p,state.peer)?.let(::askRemove)})) {key,value->
                        val authority=DeliveryBinding.from(p,id)?.let(::CapturedAuthority)
                        if(authority!=null)scope.launch {actions.run(resources.getString(R.string.ui_error_setting)) {devicesModel.setPairSwitch(authority,key,value)}}
                    }
                }
            }
            }
        }
        val proposed=remember(candidate) {candidate?.let {runCatching {Invitation.parse(it)}.getOrNull()}}
        var confirmationAttempt by remember {mutableIntStateOf(0)}
        val pairingTarget=remember(candidate,confirmationAttempt) {proposed?.let {ConfirmedBinding.forInvitation(p,it)}}
        // Also link with your other devices: on by default, off and fixed at the limit.
        // A device already paired keeps its own choice, so its confirmation has no switch.
        var alsoLink by rememberSaveable(candidate) {mutableStateOf(true)}
        val atLimit=SetupPresentation.atLimit(p)
        val offersLink=proposed!=null && p.devices.none {it.pin==proposed.fingerprint}
        val linkChoice=offersLink && alsoLink && !atLimit
        // Saved state restores the candidate preview, never an accepted approval.
        // Permission callbacks after recreation therefore require another Pair tap.
        var permissionPair by remember {mutableStateOf<List<String>?>(null)}
        var permissionRevision by remember {mutableIntStateOf(0)}
        fun startConfirmed(raw:String,target:ConfirmedBinding,setupEnabled:Boolean) {
            // Both camera and deep-link confirmations use this owner. Retain before
            // starting so a rejected attempt/pending removal cannot erase Retry.
            lastAttemptedInvitation=raw
            val baseline=p.pairingRevision
            // Who this phone joins through: its notices say "Also linked", never "joined".
            val pin=runCatching {Invitation.parse(raw).fingerprint}.getOrDefault("")
            setupStore.update {if(setupEnabled)it.copy(joinedPin=pin,joinedAt=System.currentTimeMillis()) else it.copy(joinedPin="",joinedAt=0)}
            scope.launch {if(actions.run(resources.getString(R.string.ui_error_pairing),{confirmationAttempt++;offerPair(raw)}) {beginPair(context,raw,target,setupEnabled)}) {scannerPairingRevision=baseline;scannerAttemptId=app.connection.lastStartedAttemptId;offerPair(null);navigate("pair")}}
        }
        fun finishApproved() {
            val approved=permissionPair ?: return
            val target=ConfirmedBinding(approved[1],approved[2].ifEmpty {null},approved[3].ifEmpty {null},approved[4].toLong(),approved[5])
            if(approved[0]!=candidate || !target.matches(p)) {
                permissionPair=null
                if(approved[0]==candidate)actionError=UiFeedback(resources.getString(R.string.ui_error_pairing)) to {confirmationAttempt++;offerPair(approved[0])}
                return
            }
            permissionPair=null
            startConfirmed(approved[0],target,approved.getOrNull(6)!="0" && !SetupPresentation.atLimit(p))
        }
        val pairingPermission=rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {granted->
            permissionRevision++
            if(granted)finishApproved()
        }
        LaunchedEffect(refresh,candidate,p.deviceId,p.peer?.pin,p.peer?.deviceId,p.pairingRevision) {
            val approved=permissionPair
            if(approved!=null && (approved[0]!=candidate || approved[1]!=p.deviceId || !ConfirmedBinding(approved[1],approved[2].ifEmpty {null},approved[3].ifEmpty {null},approved[4].toLong(),approved[5]).matches(p))) {
                permissionPair=null
                if(approved[0]==candidate)actionError=UiFeedback(resources.getString(R.string.ui_error_pairing)) to {confirmationAttempt++;offerPair(approved[0])}
            }
            else if(approved!=null && context.granted(Manifest.permission.ACCESS_LOCAL_NETWORK))finishApproved()
        }
        val networkSettings=remember(refresh,permissionRevision) {Build.VERSION.SDK_INT>=37 && !context.granted(Manifest.permission.ACCESS_LOCAL_NETWORK) && PermissionRequests.settings(context as Activity,Manifest.permission.ACCESS_LOCAL_NETWORK)}
        LaunchedEffect(candidate) {if(candidate!=null && proposed==null) {
            // PairScreen owns validation snackbars. Route rejected links there
            // from every entry screen, with a fresh floor for the current binding.
            scannerPairingRevision=p.pairingRevision
            navigate("pair")
            // Keep this UI event independent of connection-state updates while
            // AnimatedContent brings the scanner into composition.
            // The computer's Phone tab shows the download code until Pairing Code is clicked.
            rejectedLinkFeedback=UiFeedback(resources.getString(if(isDownloadCode(candidate,resources.getString(R.string.download_url)))R.string.pair_download_code else R.string.pair_not_pneu))
            offerPair(null)
        }}
        if(proposed!=null && pairingTarget!=null && p.deviceId.isNotEmpty())PneuTheme(dark=if(screen=="pair")true else dark) {PairConfirmDialog(
            resources.getString(R.string.pair_confirm_title,proposed.name),
            if(networkSettings)resources.getString(R.string.action_open_settings) else resources.getString(R.string.action_pair),
            if(offersLink)alsoLink else null,atLimit,{alsoLink=it},{offerPair(null)}) {
                if(!pairingTarget.matches(p))actionError=UiFeedback(resources.getString(R.string.ui_error_pairing)) to {confirmationAttempt++;offerPair(candidate)}
                else if(Build.VERSION.SDK_INT>=37 && !context.granted(Manifest.permission.ACCESS_LOCAL_NETWORK)) {
                    permissionPair=listOf(candidate!!,pairingTarget.phoneId,pairingTarget.pin.orEmpty(),pairingTarget.pcId.orEmpty(),pairingTarget.revision.toString(),pairingTarget.pcName,if(linkChoice)"1" else "0")
                    if(networkSettings)PermissionRequests.openSettings(context)
                    else {PermissionRequests.mark(context,Manifest.permission.ACCESS_LOCAL_NETWORK);pairingPermission.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)}
                }
                else startConfirmed(candidate!!,pairingTarget,linkChoice)
            }}
        unpair?.let {confirmed->val affected=confirmed.pendingCount(pendingDeliveries);PneuDialog(destructive=true,onDismissRequest={unpair=null},title={Text(if(confirmed.pin!=null)resources.getString(R.string.unpair_title,confirmed.pcName) else resources.getString(R.string.unpair_finish_title))},
            text=if(affected>0) ({Text(resources.getQuantityString(R.plurals.unpair_text,affected,affected))}) else null,
            confirmButton={PneuTextButton(onClick={unpair=null;scope.launch {actions.run(resources.getString(R.string.ui_error_unpair),{unpair=ConfirmedBinding.from(app.prefs.value,confirmed.pcId)}) {
                check(app.connection.unpair(confirmed)) {"Binding changed during unpair"}
                val remaining=app.preferences.read()
                if(!SetupPresentation.hasDevices(remaining)) {PneuService.stop(context);navigate("pair")}
                else {if(!remaining.pairingLost)PneuService.start(context);navigate("home")}
            }}}) {Text(resources.getString(R.string.action_unpair))}},
            dismissButton={PneuTextButton(onClick={unpair=null}) {Text(resources.getString(R.string.cd_cancel))}})}
        val removedSelf=p.membership.withdrawalRevision>setupUi.seenWithdrawal
        val asking=displayedPrompts.filter {SetupPresentation.promptKey(it) !in answering}
        val askingKeys=asking.map(SetupPresentation::promptKey)
        val showingMigrationRequest=resumed && asking.isNotEmpty() && askingKeys.any {it !in dismissedPrompts} && screen !in setOf(null,"setup","permissions","pair") && !removedSelf
        if(showingMigrationRequest) {
            LaunchedEffect(askingKeys,resumed) {if(resumed)SetupNotifications.cancelRequest(context)}
            LinkRequestDialog(SetupPresentation.requestTitle(resources,SetupPresentation.promptNames(p,asking)),{setupStore.dismissPrompts(askingKeys)},{answer(asking,false)}) {answer(asking,true)}
        }
        val readmit=SetupPresentation.readmissions(p).firstOrNull {it.actionKey !in dismissedPrompts}
        if(resumed && readmit!=null && !showingMigrationRequest && screen !in setOf(null,"setup","permissions","pair") && !removedSelf) {
            LaunchedEffect(readmit.actionKey) {SetupNotifications.cancelReadmission(context,readmit.actionKey)}
            fun decide(allow:Boolean) {scope.launch {actions.run(resources.getString(R.string.setup_readmission_error),errorMessage={readmissionError(context,it)}) {
                app.answerReadmission(readmit,allow)
            }}}
            ReadmissionDialog(SetupPresentation.readmissionText(resources,p,readmit),{setupStore.dismissPrompts(listOf(readmit.actionKey))},{decide(false)}) {decide(true)}
        }
        var dismissedConflicts by remember {mutableStateOf(emptySet<String>())}
        val certificateConflict=p.membership.conflicts.firstOrNull {it.reason.startsWith("Different certificate") && p.membership.barriers.values.any {b->!b.cleared && b.removed.deviceId==it.deviceId && b.removed.fingerprint!=it.fingerprint} &&
            "${p.membership.generation}/${it.deviceId}/${it.fingerprint}" !in dismissedConflicts && p.devices.none {peer->peer.deviceId==it.deviceId}}
        if(resumed && certificateConflict!=null && readmit==null && asking.isEmpty() && !removedSelf && screen !in setOf(null,"setup","permissions","pair")) {
            val name=p.membership.barriers.values.firstOrNull {it.removed.deviceId==certificateConflict.deviceId}?.removed?.name.orEmpty()
            SetupNoticeDialog(resources.getString(R.string.setup_different_certificate,name),null) {
                dismissedConflicts=dismissedConflicts+"${p.membership.generation}/${certificateConflict.deviceId}/${certificateConflict.fingerprint}"
            }
        }
        removing?.let {confirmed->
            val name=p.devices.find {it.deviceId==confirmed.member.deviceId}?.name ?: confirmed.member.name
            RemoveFromAllDialog(name,confirmed.binding?.pendingCount(pendingDeliveries) ?: 0,{removing=null}) {
                removing=null
                scope.launch {
                    val removed=try {devicesModel.removeFromAllDevices(confirmed)}catch(e:Exception) {
                        if(e is CancellationException)throw e
                        runCatching {android.util.Log.w("PneuUiAction",e.javaClass.simpleName)};false
                    }
                    if(!removed) {removeFailed=name;return@launch}
                    val remaining=runCatching {app.preferences.read()}.getOrDefault(app.prefs.value)
                    if(!SetupPresentation.hasDevices(remaining)) {PneuService.stop(context);navigate("pair")}
                    else {
                        if(!remaining.pairingLost)PneuService.start(context)
                        navigate("home")
                        homeSnackbar.showSnackbar(resources.getString(R.string.setup_removed,name),duration=SnackbarDuration.Long)
                    }
                }
            }
        }
        removeFailed?.let {name->SetupNoticeDialog(resources.getString(R.string.setup_remove_failed_title,name),resources.getString(R.string.setup_remove_failed_text)) {removeFailed=null}}
        if(removedSelf)PneuTheme(dark=if(screen=="pair")true else dark) {
            SetupNoticeDialog(resources.getString(R.string.setup_removed_self),null) {setupStore.update {it.copy(seenWithdrawal=maxOf(it.seenWithdrawal,p.membership.withdrawalRevision))}}
        }
        // Home says each of these once: what this phone also linked with, separate devices, joins, then Linked.
        val presented by setupStore.presentedNotices.collectAsState()
        val news=if(screen=="home" && !removedSelf && resumed)SetupPresentation.news(p,setupUi,System.currentTimeMillis(),presented) else null
        val nextNews by rememberUpdatedState(news)
        // Keep the displayed snackbar alive after acknowledging its source notice.
        LaunchedEffect(screen,removedSelf,resumed) {
            if(screen!="home" || removedSelf || !resumed)return@LaunchedEffect
            while(isActive) {
                val item=snapshotFlow {nextNews}.first {it!=null}!!
                showingNews=item
                val removable=(item as? SetupNews.Joined)?.key
                val result=homeSnackbar.showSnackbar(SetupPresentation.newsText(resources,item),actionLabel=removable?.let {resources.getString(R.string.setup_action_remove)},
                    duration=if(item is SetupNews.Joined || item is SetupNews.Separate)SnackbarDuration.Long else SnackbarDuration.Short)
                showingNews=null
                if(result==SnackbarResult.ActionPerformed && removable!=null && item is SetupNews.Joined) {navigate(DEVICE+item.notice.peerId);askRemove(removable)}
                snapshotFlow {nextNews?.id}.first {it!=item.id}
            }
        }
        LaunchedEffect(p.membership,p.devices,setupUi.linkStarted) {
            val separate=p.membership.separateLinks
            setupStore.update {it.copy(announcedSeparate=it.announcedSeparate.intersect(separate),
                linkStarted=it.linkStarted && !(SetupPresentation.linkSettled(p) && !SetupPresentation.linkSucceeded(p,setupUi)))}
        }
        actionError?.let {error->PneuTheme(dark=if(screen=="pair")true else dark) {PneuDialog(onDismissRequest={actionError=null},title={Text(error.first.message)},
            confirmButton={PneuTextButton(onClick={actionError=null;error.second?.invoke()}) {Text(if(error.second!=null)resources.getString(R.string.action_try_again) else resources.getString(R.string.action_close))}},
            dismissButton=if(error.second!=null)({PneuTextButton(onClick={actionError=null}) {Text(resources.getString(R.string.action_close))}}) else null)}}
    }
    }
}
@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun HomeTopBar(scrolled:Boolean,history:(()->Unit)?=null,settings:()->Unit) {
    val c=LocalPneuColors.current
    TopAppBar(modifier=Modifier.drawWithContent {drawContent();if(scrolled)drawLine(c.separator,androidx.compose.ui.geometry.Offset(0f,size.height-1f),androidx.compose.ui.geometry.Offset(size.width,size.height-1f),1f)},
        title={Row(Modifier.padding(start=4.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(12.dp)) {
            Symbol(R.drawable.ic_pneu_mark,modifier=Modifier.size(28.dp),tint=Color.Unspecified)
            Text(stringResource(R.string.home_title),style=PneuType.title2)
        }},actions={if(history!=null)IconButton(onClick=history){Symbol(R.drawable.ic_content_paste,stringResource(R.string.history_title))};IconButton(onClick=settings){Symbol(R.drawable.ic_settings,stringResource(R.string.cd_settings))}},
        colors=TopAppBarDefaults.topAppBarColors(containerColor=c.bg))
}
@Composable fun Symbol(id: Int, description: String? = null, modifier: Modifier = Modifier.size(24.dp), tint: Color = LocalContentColor.current) { Icon(painterResource(id),description,modifier,tint) }
@Composable private fun groupColor() = LocalPneuColors.current.surface
@Composable private fun Header(text: String) { Text(text,Modifier.padding(start=16.dp,top=22.dp,bottom=8.dp),style=PneuType.section,color=LocalPneuColors.current.text2) }
@Composable private fun NavIcon(back: () -> Unit) { IconButton(onClick=back) { Symbol(R.drawable.ic_arrow_back,stringResource(R.string.cd_back)) } }

@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun SetupScreen(first: Boolean, health: Health, refresh: Int, back: () -> Unit, repair:()->Unit, next: () -> Unit) {
    val context=LocalContext.current;val resources=LocalResources.current; val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    var recheck by remember { mutableIntStateOf(0) }; var returnedFromSleeping by remember { mutableStateOf(false) }
    val checked=remember(health,refresh,recheck) { context.health() }
    val permissions=rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        recheck++;context.graph.requestMediaScan()
    }
    fun details() {PermissionRequests.openSettings(context)}
    fun request(vararg names:String) {
        PermissionRequests.mark(context,*names)
        permissions.launch(arrayOf(*names))
    }
    LaunchedEffect(refresh) { if(returnedFromSleeping && actions.run(resources.getString(R.string.ui_error_battery)) {withContext(Dispatchers.IO) {ServiceEvidence.acknowledge(context)};context.graph.preferences.toggle("neverSleeping",true)}) {returnedFromSleeping=false;recheck++;repair()} }
    val scroll=TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    val density=LocalDensity.current
    if(first)SideEffect {scroll.state.heightOffsetLimit=with(density){-84.dp.toPx()}}
    Scaffold(modifier=Modifier.nestedScroll(scroll.nestedScrollConnection),containerColor=Color.Transparent,contentWindowInsets=WindowInsets.statusBars,topBar={
        if(first)Column(Modifier.statusBarsPadding()) {
            val iconHeight=(100.dp+with(density){scroll.state.heightOffset.toDp()}).coerceIn(16.dp,100.dp)
            Box(Modifier.fillMaxWidth().height(iconHeight).clipToBounds()) {
                Symbol(R.drawable.ic_launcher_preview_crop,modifier=Modifier.padding(start=20.dp,top=28.dp).size(56.dp).clip(RoundedCornerShape(17.dp)),tint=Color.Unspecified)
            }
            Text(resources.getString(R.string.setup_title),Modifier.padding(start=20.dp,end=20.dp,bottom=14.dp),style=if(scroll.state.collapsedFraction>.5f)PneuType.title2 else PneuType.largeTitle)
        }
        else LargeTopAppBar(expandedHeight=114.dp,collapsedHeight=56.dp,scrollBehavior=scroll,title={Text(resources.getString(R.string.permissions_title),Modifier.padding(start=4.dp),style=if(scroll.state.collapsedFraction>.5f)PneuType.title2 else PneuType.largeTitle)},navigationIcon={NavIcon(back)},colors=TopAppBarDefaults.topAppBarColors(containerColor=LocalPneuColors.current.bg,scrolledContainerColor=LocalPneuColors.current.bg))
    },bottomBar={if(first) Box(Modifier.navigationBarsPadding().padding(16.dp)) { PneuButton(next,Modifier.fillMaxWidth().height(52.dp),kind=ButtonKind.PrimaryLarge) {Text(resources.getString(R.string.action_continue))} }}) { padding ->
        Column(Modifier.padding(padding).fillMaxWidth().widthIn(max=600.dp).padding(horizontal=16.dp).verticalScroll(rememberScrollState()).navigationBarsPadding()) {
            PneuCard {
                val activity=context as Activity
                if(Build.VERSION.SDK_INT>=37)PermissionRow(resources.getString(R.string.perm_local_network),R.drawable.ic_lan,checked.localNetwork,if(PermissionRequests.settings(activity,Manifest.permission.ACCESS_LOCAL_NETWORK))resources.getString(R.string.settings_title) else resources.getString(R.string.action_allow),divider=false) {
                    if(PermissionRequests.settings(activity,Manifest.permission.ACCESS_LOCAL_NETWORK))details() else request(Manifest.permission.ACCESS_LOCAL_NETWORK)
                }
                val notificationSettings=checked.channelBlocked || context.granted(Manifest.permission.POST_NOTIFICATIONS) || PermissionRequests.settings(activity,Manifest.permission.POST_NOTIFICATIONS)
                PermissionRow(resources.getString(R.string.perm_notifications),R.drawable.ic_notifications,checked.notifications,if(notificationSettings)resources.getString(R.string.settings_title) else resources.getString(R.string.action_allow),if(notificationSettings && !checked.notifications)resources.getString(R.string.perm_denied) else null,divider=Build.VERSION.SDK_INT>=37) {
                    if(notificationSettings)context.startActivity(Intent(if(checked.channelBlocked)Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS else Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE,context.packageName).putExtra(Settings.EXTRA_CHANNEL_ID,StatusNotificationChannel.id(context)))
                    else request(Manifest.permission.POST_NOTIFICATIONS)
                }
                val limited=context.granted(Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED) && !checked.photos
                val photoAction=PermissionPolicy.photoAction(limited,PermissionRequests.asked(context,Manifest.permission.READ_MEDIA_IMAGES),activity.shouldShowRequestPermissionRationale(Manifest.permission.READ_MEDIA_IMAGES) || activity.shouldShowRequestPermissionRationale(Manifest.permission.READ_MEDIA_VIDEO))
                // Play's prominent disclosure: say what is sent automatically next to the button that requests media access.
                PermissionRow(resources.getString(R.string.perm_photos),R.drawable.ic_photo_library,checked.photos,resources.getString(photoAction),when {checked.photos->null;limited->resources.getString(R.string.perm_photos_limited);photoAction==R.string.action_settings->resources.getString(R.string.perm_denied);else->resources.getString(R.string.perm_photos_disclosure)}) {
                    if(photoAction==R.string.action_settings)details() else request(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED)
                }
                PermissionRow(resources.getString(R.string.perm_battery),R.drawable.ic_battery_full,checked.battery,resources.getString(R.string.action_open_settings)) {details()}
                if(checked.killed && (checked.sleeping || !Build.MANUFACTURER.equals("samsung",true)))PermissionRow(resources.getString(R.string.alert_killed_title),R.drawable.ic_bedtime_off,false,resources.getString(R.string.action_dismiss)) {
                    scope.launch {if(actions.run(resources.getString(R.string.ui_error_background_stop)) {withContext(Dispatchers.IO) {ServiceEvidence.acknowledge(context)}}) {recheck++;repair()}}
                }
                if(Build.MANUFACTURER.equals("samsung",true)) PermissionRow(resources.getString(R.string.perm_never_sleeping),R.drawable.ic_bedtime_off,checked.sleeping,resources.getString(R.string.action_open),if(checked.sleeping) null else resources.getString(R.string.perm_never_sleeping_hint)) {
                    returnedFromSleeping=SettingsNavigation.openSamsungBattery(context.packageName,context::startActivity)
                }
            }
        }
    }
}
@Composable private fun PermissionRow(title: String, icon: Int, done: Boolean, action: String, detail: String? = null, divider:Boolean=true, click: () -> Unit) {
    Row(Modifier.fillMaxWidth().pneuRow(divider,66.dp).heightIn(min=if(detail==null)60.dp else 68.dp).padding(horizontal=16.dp,vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
        Box(Modifier.size(36.dp).background(LocalPneuColors.current.fill,RoundedCornerShape(11.dp)),contentAlignment=Alignment.Center){Symbol(icon,modifier=Modifier.size(20.dp),tint=LocalPneuColors.current.text2)}
        Column(Modifier.weight(1f)) {Text(title,style=MaterialTheme.typography.bodyLarge);if(detail!=null) Text(detail,style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onSurfaceVariant)}
        if(done) Symbol(R.drawable.ic_check_circle,stringResource(R.string.cd_granted),tint=MaterialTheme.colorScheme.primary) else PneuButton(click,kind=ButtonKind.SmallTonal,contentPadding=PaddingValues(horizontal=16.dp)) {Text(action)}
    }
}

internal fun pairingScanBlocked(attemptPending:Boolean,installed:Boolean,expired:Boolean):Boolean =
    attemptPending || installed || expired

@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun PairScreen(p: Prefs, link: LinkState, home: () -> Unit, back: () -> Unit, offerPair:(String)->Unit,
                                   lastAttemptedInvitation:String?=null,initialPairingRevision:Long?=null,initialAttemptId:String?=null,validationFeedback:UiFeedback?=null,cameraPreview:@Composable (Modifier,(String)->Unit)->Unit={modifier,onCode->PairCamera(modifier,onCode)}) {
    val context=LocalContext.current;val resources=LocalResources.current; val scope=rememberCoroutineScope();val actions=LocalUiActions.current;
    var camera by remember { mutableStateOf(context.granted(Manifest.permission.CAMERA)) }
    var cameraAsked by remember { mutableStateOf(PermissionRequests.asked(context,Manifest.permission.CAMERA)) }
    var lastCode by remember(p.peer?.pin,p.deviceId) { mutableStateOf("") }; var lastRead by remember { mutableLongStateOf(0) }
    // The recovery surface is state, not a replayable snackbar. Its consumed
    // event ID and its visibility must survive recreation together.
    val activeAttempt by context.graph.connection.attempt.collectAsState()
    val attempt=activeAttempt // Callbacks retain the attempt rendered with their controls.
    var dialog by rememberSaveable(p.peer?.pin,p.deviceId,attempt?.id) { mutableStateOf(false) }
    // A persisted successful install survives loss of READY, even if the UI
    // never observed pending or its transient success marker. Save the floor
    // across recreation; reopening this scanner starts a fresh floor.
    var pairingBaseline by rememberSaveable { mutableStateOf<Long?>(initialPairingRevision ?: if(p.deviceId.isNotEmpty())p.pairingRevision else null) }
    LaunchedEffect(p.deviceId) {if(pairingBaseline==null && p.deviceId.isNotEmpty())pairingBaseline=p.pairingRevision}
    val installation by context.graph.connection.installation.collectAsState()
    val installationFloor by rememberSaveable {mutableStateOf(installation?.attemptId)}
    val scannerPin=lastAttemptedInvitation?.let {runCatching {Invitation.parse(it).fingerprint}.getOrNull()} ?: attempt?.invitation?.fingerprint
    val completed=installation?.takeIf {event->event.attemptId!=installationFloor || event.attemptId==initialAttemptId ||
        initialPairingRevision?.let {event.binding.generation>it}==true}
        ?.takeIf {initialAttemptId==null || it.attemptId==initialAttemptId}
        ?.binding?.takeIf {it.matches(p) && (scannerPin==null || it.pin==scannerPin)}
    val installedBinding=pairingBaseline?.let {floor->scannerPin?.let {p.installedAfter(floor,it)}} ?: completed
    val installed=installedBinding!=null
    val paired=installed
    val success=paired
    val disconnected=link.connectionStatus !in setOf(ConnectionStatus.CONNECTED,ConnectionStatus.CONNECTING)
    val snackbar=remember { SnackbarHostState() }
    var consumedValidation by rememberSaveable {mutableStateOf<String?>(null)}
    LaunchedEffect(validationFeedback?.id) {validationFeedback?.let {
        if(it.id!=consumedValidation) {
            consumedValidation=it.id
            snackbar.showSnackbar(it.message,duration=SnackbarDuration.Short)
        }
    }}
    // Rationale and the request history aren't observable; recheck after every result and resume,
    // or a permanent denial keeps the first denial's button and hides why the camera is off.
    var cameraCheck by remember {mutableIntStateOf(0)}
    val permission=rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { camera=it;cameraAsked=true;cameraCheck++ }
    LaunchedEffect(Unit) { if(!camera && !PermissionRequests.settings(context as Activity,Manifest.permission.CAMERA))actions.run(resources.getString(R.string.ui_error_camera)) {PermissionRequests.mark(context,Manifest.permission.CAMERA);permission.launch(Manifest.permission.CAMERA)} }
    val lifecycle=androidx.lifecycle.compose.LocalLifecycleOwner.current
    DisposableEffect(lifecycle) {
        val observer=androidx.lifecycle.LifecycleEventObserver { _,e -> if(e==androidx.lifecycle.Lifecycle.Event.ON_RESUME) {camera=context.granted(Manifest.permission.CAMERA);cameraCheck++} }
        lifecycle.lifecycle.addObserver(observer);onDispose { lifecycle.lifecycle.removeObserver(observer) }
    }
    var feedbackTimedOut by rememberSaveable(attempt?.id) { mutableStateOf(false) }
    val busy=attempt != null && !feedbackTimedOut && link.pairingFeedback?.issue != PairingIssue.UNREACHABLE
    BackHandler(busy) {}
    LaunchedEffect(busy,attempt?.id) {
        if(busy) { delay(15000);if(context.graph.connection.attempt.value?.id==attempt?.id) {feedbackTimedOut=true;dialog=true} }
    }
    val pairingFeedback=link.pairingFeedback?.takeIf {it.message==link.pairing}
    var expired by rememberSaveable {mutableStateOf(false)}
    val scanBlocked=pairingScanBlocked(attempt!=null,installed,expired)
    var getPneu by rememberSaveable {mutableStateOf(false)}
    val network=rememberPhoneNetwork();val tailscaleInstalled=rememberTailscaleInstalled()
    val cameraSettings=remember(camera,cameraAsked,cameraCheck) {PermissionRequests.settings(context as Activity,Manifest.permission.CAMERA)}
    val cameraLabel=remember(camera,cameraAsked,cameraCheck) {PermissionPolicy.cameraAction(cameraAsked,(context as Activity).shouldShowRequestPermissionRationale(Manifest.permission.CAMERA))}
    var consumedPairing by rememberSaveable {mutableStateOf<String?>(null)}
    LaunchedEffect(pairingFeedback?.id) {
        val event=pairingFeedback ?: return@LaunchedEffect
        if(event.id==consumedPairing)return@LaunchedEffect
        consumedPairing=event.id
        when {
            event.issue==PairingIssue.UNREACHABLE -> dialog=true
            event.issue==PairingIssue.EXPIRED -> expired=true
            event.issue in setOf(PairingIssue.CLOCK,PairingIssue.FEEDBACK) -> snackbar.showSnackbar(UiPresentation.pairing(context,event),duration=SnackbarDuration.Short)
        }
    }
    LaunchedEffect(paired,p.peer,p.pairingRevision,disconnected) {
        if(paired) {
            dialog=false
            if(!disconnected)delay(800)
            val current=context.graph.prefs.value
            if(installedBinding?.matches(current)==true)home()
        }
    }
    PneuTheme(dark=true) {
        Box(Modifier.fillMaxSize().background(Color.Black).semantics {paneTitle=resources.getString(R.string.pair_title)}) {
            if(camera) {
                cameraPreview(Modifier.fillMaxSize()) { raw ->
                    if(!scanBlocked && (raw != lastCode || android.os.SystemClock.elapsedRealtime()-lastRead>3000)) {
                        lastCode=raw;lastRead=android.os.SystemClock.elapsedRealtime()
                        val invitation=runCatching {Invitation.parse(raw)}.getOrNull()
                        val known=invitation?.takeIf {it.version==1L}?.let {invite->
                            context.graph.prefs.value.devices.find {it.pin==invite.fingerprint && !it.pairingLost && context.graph.connection.connected(it.deviceId)}
                        }
                        if(known!=null)scope.launch {snackbar.showSnackbar(resources.getString(R.string.pair_already_paired,known.name),duration=SnackbarDuration.Short)}
                        else {getPneu=false;offerPair(raw)}
                    }
                }
                ScannerFrame(busy,success,if(busy) resources.getString(R.string.pair_pairing) else if(success) resources.getString(R.string.pair_success,p.devices.find {it.deviceId==installedBinding?.pcId}?.name.orEmpty()) else resources.getString(R.string.pair_instruction),
                    extra=if(busy || success) null else ({GetPneuButton {getPneu=true}}))
            } else Column(Modifier.align(Alignment.Center).padding(horizontal=24.dp),horizontalAlignment=Alignment.CenterHorizontally) {
                Box(Modifier.size(72.dp).background(Color.White.copy(alpha=.12f),CircleShape),contentAlignment=Alignment.Center){Symbol(R.drawable.ic_no_photography,modifier=Modifier.size(32.dp),tint=Color.White)}
                Text(resources.getString(R.string.pair_camera_off),Modifier.padding(top=16.dp),color=Color.White,style=PneuType.headline,textAlign=TextAlign.Center)
                PneuButton(onClick={
                    if(cameraSettings) PermissionRequests.openSettings(context)
                    else {PermissionRequests.mark(context,Manifest.permission.CAMERA);permission.launch(Manifest.permission.CAMERA)}
                },Modifier.padding(top=16.dp),contentPadding=PaddingValues(horizontal=18.dp)) {Text(resources.getString(cameraLabel))}
                if(!busy && !success)Box(Modifier.padding(top=40.dp)) {GetPneuButton {getPneu=true}}
            }
            IconButton(onClick=back,enabled=!busy,modifier=Modifier.statusBarsPadding().padding(start=12.dp,top=8.dp).size(44.dp).background(Color.White.copy(alpha=.14f),CircleShape)) {Symbol(R.drawable.ic_arrow_back,resources.getString(R.string.cd_back),tint=Color.White.copy(alpha=if(busy).4f else 1f))}
            PneuSnackbarHost(snackbar,Modifier.align(Alignment.BottomCenter).navigationBarsPadding())
            if(dialog) {
                // Name the computer from the code being tried; the phone's own network picks the likely cause.
                val computer=(attempt?.invitation ?: (lastAttemptedInvitation ?: lastCode.takeIf(String::isNotEmpty))?.let {runCatching {Invitation.parse(it)}.getOrNull()})
                    ?.name?.takeIf(String::isNotBlank) ?: resources.getString(R.string.pair_your_computer)
                fun cancel() {val pending=attempt;dialog=false;scope.launch {actions.run(resources.getString(R.string.ui_error_cancel_pairing),{dialog=true}) {check(context.graph.cancelPairing(pending)) {"Pairing changed during cancellation"}}}}
                PneuDialog(dismissible=false,onDismissRequest={},title={Text(resources.getString(R.string.pair_unreachable_title,computer))},
                text={Text(resources.getString(when {!network.wifi->R.string.pair_not_on_wifi;tailscaleInstalled->R.string.pair_unreachable_tailscale;else->R.string.pair_unreachable_text},computer))},
                confirmButton={if(!network.wifi)PneuTextButton(onClick={cancel();scope.launch {actions.run(resources.getString(R.string.ui_error_wifi_settings)) {ReachabilityHints.openWifiSettings(context)}}}) {Text(resources.getString(R.string.action_wifi_settings))}
                else PneuTextButton(onClick={val confirmed=ConfirmedBinding.from(p);val pending=attempt;val code=lastAttemptedInvitation ?: lastCode.takeIf(String::isNotEmpty);dialog=false;feedbackTimedOut=false;scope.launch {if(pending==null && code!=null)offerPair(code) else actions.run(resources.getString(R.string.ui_error_retry_connection),{dialog=true}) {check(context.graph.retryConnection(confirmed,pending,feedback=pending==null)) {"Pairing changed during retry"}}}}) {Text(resources.getString(R.string.action_try_again))}},dismissButton={PneuTextButton(onClick={cancel()}){Text(resources.getString(R.string.cd_cancel))}})
            }
            if(expired) PneuDialog(onDismissRequest={expired=false},title={Text(resources.getString(R.string.pair_expired_title))},text={Text(resources.getString(R.string.pair_expired_text))},
                confirmButton={PneuTextButton(onClick={expired=false}) {Text(resources.getString(R.string.action_ok))}})
            if(getPneu) GetPneuSheet {getPneu=false}
        }
    }
}

@Composable internal fun HomeScreen(p: Prefs,link: LinkState,recent: List<Outgoing>,health: Health,modifier: Modifier,permissions: () -> Unit,pair: () -> Unit,onScrolled:(Boolean)->Unit,outcomes:Map<String,RecentItem> = emptyMap(),
                                    devices:List<DeviceState>?=null,deviceLinks:Map<String,LinkState>?=null,setup:HomeSetup=HomeSetup(),openDevice:(String)->Unit={}) {
    val context=LocalContext.current;val resources=LocalResources.current; val app=context.graph; val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    val liveLinks by app.connection.devices.collectAsState()
    val links=deviceLinks ?: liveLinks
    val deviceList=devices ?: remember(p,links) {DeviceProjection.states(p,links)}
    var details by rememberSaveable {mutableStateOf<String?>(null)}
    val clipboardReady=rememberClipboardAvailable(p,links)
    var queueMenu by remember { mutableStateOf(false) }
    val waiting by app.waitingCount.collectAsState()
    var showProgress by remember(link.transferMediaId) {mutableStateOf(false)}
    LaunchedEffect(link.transferMediaId) {if(link.transferMediaId.isNotEmpty()){delay(1000);showProgress=true}}
    val title=if(p.pairingLost) ConnectionStatus.PAIRING_LOST else if(p.paused) ConnectionStatus.PAUSED else link.connectionStatus
    val network=RoutePolicy.active(context.getSystemService(android.net.ConnectivityManager::class.java))
    val hint=unreachableHint(title==ConnectionStatus.UNREACHABLE,p.devices,p.tailscale)
    var anywhere by rememberSaveable {mutableStateOf(false)}
    val detail=when(title) {
        ConnectionStatus.PAIRING_LOST -> resources.getString(R.string.detail_pairing_lost,p.lostPcName.ifEmpty {p.peer?.name.orEmpty()})
        ConnectionStatus.PAUSED -> resources.getString(R.string.detail_paused)
        ConnectionStatus.CONNECTED -> UiPresentation.connectionDetail(context,p,links,link)
        ConnectionStatus.CONNECTING -> UiPresentation.connectionDetail(context,p,links,link)
        ConnectionStatus.WAITING_WIFI -> resources.getString(R.string.detail_mobile_data_off)
        else -> hint?.let {unreachableDetail(context,it,p.devices)} ?: if(p.lastConnected>0) resources.getString(R.string.detail_last_connected,lastConnectedLabel(context,p.lastConnected)) else ""
    }
    val navigationBottom=WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding()
    val list=androidx.compose.foundation.lazy.rememberLazyListState()
    LaunchedEffect(list) {snapshotFlow {list.firstVisibleItemIndex>0 || list.firstVisibleItemScrollOffset>0}.collect(onScrolled)}
    Box(modifier.fillMaxSize(),contentAlignment=Alignment.TopCenter) {
        LazyColumn(Modifier.widthIn(max=600.dp).fillMaxWidth().padding(horizontal=16.dp),state=list,contentPadding=PaddingValues(bottom=24.dp+navigationBottom)) {
            if(p.localCapacity) item {
                PneuBanner(resources.getString(R.string.banner_capacity),resources.getString(R.string.recent_dont_send_any),capacity=true,click={
                    val selected=ConfirmedQueue.from(p,app.work.value)
                    scope.launch {actions.run(resources.getString(R.string.ui_error_clear_queue)) {check(app.discardAll(selected)) {"Queue changed"}}}
                })
            }
            else if(health.issues.isNotEmpty()) item {
                PneuBanner(if(health.issues.size==1) resources.getString(health.issues.single()) else resources.getQuantityString(R.plurals.banner_many,health.issues.size,health.issues.size),resources.getString(R.string.action_fix),permissions)
            }
            setup.banner?.let {banner-> item {
                PneuLinkBanner(SetupPresentation.bannerText(resources,banner),if(banner is LinkBanner.Offer)resources.getString(R.string.action_link) else null,
                    {if(banner is LinkBanner.Offer)setup.link(banner.peerId)},setup.dismissBanner,resources.getString(R.string.action_close))
            }}
            item {
                Surface(shape=RoundedCornerShape(24.dp),border=if(LocalPneuColors.current.dark)BorderStroke(1.dp,LocalPneuColors.current.hairline)else null,color=groupColor(),modifier=Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(start=20.dp,end=20.dp,top=24.dp,bottom=20.dp),horizontalAlignment=Alignment.CenterHorizontally) {
                        LinkView(link.copy(status=title.key),sending=link.transferMediaId.isNotEmpty(),receiving=link.incoming!=null,others=if(deviceList.size>1)linkTones(p,deviceList) else emptyList())
                        Crossfade(title,animationSpec=androidx.compose.animation.core.tween(PneuMotion.short),label="Connection status") {Text(UiPresentation.status(context,it),Modifier.fillMaxWidth().padding(top=16.dp),style=PneuType.title1,textAlign=TextAlign.Center)}
                        if(detail.isNotEmpty()) Text(detail,Modifier.padding(top=4.dp),style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onSurfaceVariant,maxLines=2,overflow=TextOverflow.Ellipsis,textAlign=TextAlign.Center)
                        PneuButtonPair(Modifier.fillMaxWidth().padding(top=20.dp)) {
                            if(p.pairingLost) PneuButton(pair,Modifier.fillMaxWidth().height(48.dp)) {Symbol(R.drawable.ic_qr_code_scanner,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_pair_again))}
                            else {
                                // Unreachable: what helps reconnect takes Send clipboard's place, as Open Tailscale did.
                                if(hint?.hasAction()==true)UnreachableAction(hint,Modifier.height(48.dp)) {anywhere=true}
                                else PneuTonalButton(onClick={scope.launch {actions.run(resources.getString(R.string.toast_not_sent)) {
                                    context.startActivity(ManualEntries.clipboardIntent(context,p))
                                }}},modifier=Modifier.height(48.dp),enabled=clipboardReady,contentPadding=PaddingValues(horizontal=12.dp)) {Symbol(R.drawable.ic_content_paste_go,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.tile_send_clipboard))}
                                if(p.paused) PneuButton(onClick={scope.launch {actions.run(resources.getString(R.string.ui_error_resume)) {app.preferences.toggle("paused",false)}}},modifier=Modifier.height(48.dp)) {Symbol(R.drawable.ic_play_arrow,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_resume))}
                                else PneuNeutralButton(onClick={scope.launch {actions.run(resources.getString(R.string.ui_error_pause)) {app.preferences.toggle("paused",true)}}},modifier=Modifier.height(48.dp)) {Symbol(R.drawable.ic_pause,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_pause))}
                            }
                        }
                    }
                }
            }
            item {DevicesCard(p,deviceList,openDevice,pair,setup.phones,setup.openPhone)}
            item {
                Row(Modifier.fillMaxWidth().padding(start=16.dp,end=8.dp,top=20.dp,bottom=8.dp).height(32.dp),verticalAlignment=Alignment.CenterVertically) {
                    Text(resources.getString(R.string.recent_title),Modifier.weight(1f),style=PneuType.section,color=LocalPneuColors.current.text2)
                    if(waiting>0) Box {
                        Row(Modifier.height(32.dp).clip(RoundedCornerShape(10.dp)).pneuClick {queueMenu=true}.padding(start=10.dp,end=6.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(2.dp)) {
                            Text(resources.getQuantityString(R.plurals.recent_waiting,waiting,waiting),style=PneuType.section,color=LocalPneuColors.current.accentText)
                            Symbol(R.drawable.ic_expand_more,modifier=Modifier.size(18.dp),tint=LocalPneuColors.current.accentText)
                        }
                        DropdownMenu(queueMenu,{queueMenu=false},modifier=Modifier.widthIn(min=196.dp),shape=RoundedCornerShape(16.dp),containerColor=LocalPneuColors.current.raised,shadowElevation=PneuElevation.menu,border=if(LocalPneuColors.current.dark)BorderStroke(1.dp,LocalPneuColors.current.hairline)else null) {
                            DropdownMenuItem(modifier=Modifier.height(44.dp),text={Text(resources.getString(R.string.recent_send_now),style=PneuType.body.copy(fontSize=15.sp))},onClick={val confirmed=p.devices.map {ConfirmedBinding.from(p,it.deviceId)};queueMenu=false;scope.launch {actions.run(resources.getString(R.string.ui_error_retry_connection)) {check(app.retryConnections(confirmed)) {"Pairing changed during retry"}}}})
                            DropdownMenuItem(modifier=Modifier.height(44.dp),text={Text(resources.getString(R.string.recent_dont_send_any),style=PneuType.body.copy(fontSize=15.sp),color=LocalPneuColors.current.danger)},onClick={val selected=ConfirmedQueue.from(p,app.work.value);queueMenu=false;scope.launch {actions.run(resources.getString(R.string.ui_error_clear_queue)) {check(app.discardAll(selected)) {"Queue changed"}}}})
                        }
                    }
                }
            }
            if(recent.isEmpty()) item {PneuCard {Column(Modifier.fillMaxWidth().height(132.dp),horizontalAlignment=Alignment.CenterHorizontally,verticalArrangement=Arrangement.Center) {Symbol(R.drawable.ic_image,modifier=Modifier.size(28.dp),tint=LocalPneuColors.current.text3);Text(resources.getString(R.string.recent_empty),Modifier.padding(top=8.dp),style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onSurfaceVariant)}}}
            else items(recent,key={it.mediaId}) {row ->
                Surface(color=groupColor(),modifier=Modifier.then(if(LocalPneuMotion.current)Modifier.animateItem(fadeInSpec=androidx.compose.animation.core.tween(200),fadeOutSpec=androidx.compose.animation.core.tween(200))else Modifier).fillMaxWidth().pneuCardRowHairline(row==recent.first(),row==recent.last()),shape=RoundedCornerShape(topStart=if(row==recent.first())24.dp else 0.dp,topEnd=if(row==recent.first())24.dp else 0.dp,bottomStart=if(row==recent.last())24.dp else 0.dp,bottomEnd=if(row==recent.last())24.dp else 0.dp)) {
                    val outcome=outcomes[row.mediaId]
                    val multi=outcome!=null && outcome.recipients.size>1
                    val settled=outcome?.let {!it.actionable} ?: (row.state !in setOf("waiting","sending","failed"))
                    val progress=outcome?.sending==true || link.transferMediaId==row.mediaId && showProgress
                    val sent=if(outcome?.sending==true)outcome.sent else link.sent
                    val total=if(outcome?.sending==true)outcome.total else link.total
                    Row(Modifier.fillMaxWidth().pneuRow(row!=recent.first(),76.dp).heightIn(min=68.dp).pneuClick(enabled=multi && !settled || row.state=="sent" && (if(row.kind=="text")row.text.matches(Regex("""https?://\S+""")) else row.uri.isNotEmpty())) {if(multi && !settled)details=row.mediaId else scope.launch {actions.run(resources.getString(R.string.toast_cant_read)) {
                        if(row.kind=="text")context.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(row.text)))
                        else outcome?.received?.let {file->context.startActivity(ReceivedFileNotifications.openIntent(context,file))}
                            ?: check(RecentItems.openOriginal(context,row)) {"Original unavailable"}
                    }}}.padding(horizontal=16.dp,vertical=12.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
                        MediaThumb(row)
                        Column(Modifier.weight(1f)) {
                            Text(when(row.kind) {"photo"->resources.getString(R.string.kind_photo);"recording"->resources.getString(R.string.kind_recording);"file"->row.fileName;"text"->row.text.lineSequence().firstOrNull().orEmpty().removePrefix("https://");"clipboard_image"->resources.getString(R.string.kind_image);else->resources.getString(R.string.kind_screenshot)},style=MaterialTheme.typography.bodyLarge,maxLines=1,overflow=if(row.kind=="file")TextOverflow.MiddleEllipsis else TextOverflow.Ellipsis)
                            // With one paired device the lines stay v2's; with several they name recipients.
                            val several=p.devices.size>1 && outcome!=null
                            val clipboard=row.kind=="clipboard_image" || row.kind=="text" && row.origin=="clipboard"
                            val time=lastConnectedLabel(context,row.createdAt)
                            val failing=outcome?.failed?.isNotEmpty()==true
                            val supporting=when {
                                outcome?.received!=null -> UiPresentation.recentReceived(context,outcome,time,p.devices.size)!!
                                several && settled -> UiPresentation.recentSent(context,outcome,clipboard,time) ?: time
                                several && failing -> UiPresentation.recentOutcome(context,outcome!!)
                                progress -> if(multi)UiPresentation.recentOutcome(context,outcome!!) else resources.getString(R.string.item_sending,android.text.format.Formatter.formatShortFileSize(context,sent),android.text.format.Formatter.formatShortFileSize(context,total))
                                row.state=="sent" -> if(clipboard)resources.getString(R.string.item_clipboard_time,time) else time
                                row.state=="failed" -> resources.getString(UiPresentation.failedItem(row.error))
                                DeliveryPolicy.hold(row.kind,row.automatic,false,network.mobile,p.originals,row.attempted,row.optimized) -> resources.getString(R.string.item_waiting_wifi)
                                several -> UiPresentation.recentOutcome(context,outcome!!)
                                else -> resources.getString(R.string.item_waiting)
                            }
                            Text(supporting,style=MaterialTheme.typography.bodyMedium,color=if(row.state=="failed" || several && failing) LocalPneuColors.current.danger else if(progress)LocalPneuColors.current.accentText else LocalPneuColors.current.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
                            if(progress)LinearProgressIndicator(progress={if(total>0)(sent.toFloat()/total).coerceIn(0f,1f)else 0f},modifier=Modifier.fillMaxWidth().padding(top=7.dp).height(3.dp).clip(RoundedCornerShape(2.dp)),color=LocalPneuColors.current.accent,trackColor=LocalPneuColors.current.fill2,gapSize=0.dp,drawStopIndicator={})
                        }
                        if(row.state=="failed") IconButton(modifier=Modifier.size(40.dp),onClick={val reference=ConfirmedItem.from(row);scope.launch {actions.run(resources.getString(R.string.ui_error_retry_item)) {val selected=app.confirmArtifact(p,reference);check(app.retry(reference.mediaId,selected)) {"Queue item changed"}}}}) {Symbol(R.drawable.ic_refresh,resources.getString(R.string.cd_retry),modifier=Modifier.size(22.dp),tint=LocalPneuColors.current.text2)}
                        if(row.state!="sent") Box(contentAlignment=Alignment.Center) {
                            IconButton(modifier=Modifier.size(40.dp),onClick={val reference=ConfirmedItem.from(row);scope.launch {actions.run(resources.getString(R.string.ui_error_remove_item)) {val selected=app.confirmArtifact(p,reference);check(app.discard(selected)) {"Queue item changed"}}}}) {Symbol(R.drawable.ic_close,if(progress)resources.getString(R.string.cd_cancel) else if(row.state=="failed")resources.getString(R.string.cd_remove) else resources.getString(R.string.cd_dont_send),modifier=Modifier.size(22.dp),tint=LocalPneuColors.current.text2)}
                        }
                    }
                }
            }
        }
    }
    RecentDetails(p,details,outcomes,links) {details=null}
    if(anywhere)ConnectAnywhereSheet(p.tailscale,p.devices) {anywhere=false}
}
@Composable private fun RecentDetails(p:Prefs,details:String?,outcomes:Map<String,RecentItem>,links:Map<String,LinkState>,close:()->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val app=context.graph;val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    val item=details?.let(outcomes::get)
    LaunchedEffect(details,item==null) {if(details!=null && item==null)close()}
    if(item==null)return
    val row=item.row
    val title=when(row.kind) {"photo"->resources.getString(R.string.kind_photo);"recording"->resources.getString(R.string.kind_recording);"file"->row.fileName;"text"->row.text.lineSequence().firstOrNull().orEmpty().removePrefix("https://");"clipboard_image"->resources.getString(R.string.kind_image);else->resources.getString(R.string.kind_screenshot)}
    RecipientsSheet(title,item,p.devices.associate {it.deviceId to (links[it.deviceId]?.connectionStatus==ConnectionStatus.CONNECTED && !it.pairingLost && it.deviceId !in p.resetPeers)},close,
        stop={recipient->val selected=ConfirmedQueue(ConfirmedBinding.from(p),listOfNotNull(recipient.item));scope.launch {actions.run(resources.getString(R.string.ui_error_remove_item)) {check(app.discard(selected)) {"Queue item changed"}}}},
        retry={recipient->val selected=ConfirmedQueue(ConfirmedBinding.from(p),listOfNotNull(recipient.item));scope.launch {actions.run(resources.getString(R.string.ui_error_retry_item)) {check(app.retry(row.mediaId,selected)) {"Queue item changed"}}}})
}
@Composable private fun MediaThumb(row: Outgoing) {
    val context=LocalContext.current
    val bitmap by produceState<android.graphics.Bitmap?>(null,row.path,row.mime) {
        value=MediaPreview.load(context,row)
    }
    Surface(Modifier.size(44.dp),shape=RoundedCornerShape(10.dp),color=MaterialTheme.colorScheme.secondaryContainer) {
        val b=bitmap
        if(b!=null) androidx.compose.foundation.Image(b.asImageBitmap(),null,contentScale=androidx.compose.ui.layout.ContentScale.Crop)
        else Box(contentAlignment=Alignment.Center) {Symbol(when {row.kind=="text" && row.text.matches(Regex("""https?://\S+"""))->R.drawable.ic_link;row.kind=="text"->R.drawable.ic_content_paste;row.kind=="file"->R.drawable.ic_description;else->R.drawable.ic_image},modifier=Modifier.size(22.dp),tint=LocalPneuColors.current.text2)}
        if(b!=null && row.mime.startsWith("video/")) Box(Modifier.fillMaxSize().padding(4.dp),contentAlignment=Alignment.BottomStart) {
            Box(Modifier.size(18.dp).background(Color.Black.copy(alpha=.55f),CircleShape),contentAlignment=Alignment.Center) {Symbol(R.drawable.ic_play_arrow,modifier=Modifier.size(12.dp),tint=Color.White)}
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun SettingsScreen(p: Prefs,health: Health,refresh:Int,back: () -> Unit,permissions: () -> Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val app=context.graph;val scope=rememberCoroutineScope();val actions=LocalUiActions.current;val snackbar=remember {SnackbarHostState()}
    val devicesModel=remember(context) {androidx.lifecycle.ViewModelProvider(context as ComponentActivity)[DevicesViewModel::class.java]}
    val history by app.savedClips.state.collectAsState()
    var historyOff by rememberSaveable {mutableStateOf(false)}
    var renaming by rememberSaveable {mutableStateOf(false)}
    var pendingMedia by rememberSaveable {mutableStateOf("")}
    fun mediaPermissionFor(key:String)=if(key=="recordings")Manifest.permission.READ_MEDIA_VIDEO else Manifest.permission.READ_MEDIA_IMAGES
    LaunchedEffect(refresh,pendingMedia) {if(pendingMedia.isNotEmpty() && context.granted(mediaPermissionFor(pendingMedia))) {val key=pendingMedia;if(actions.run(resources.getString(R.string.ui_error_setting)) {app.preferences.toggle(key,true)})pendingMedia=""}}
    val mediaPermission=rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {result ->
        val key=pendingMedia
        if(key.isNotEmpty() && result[mediaPermissionFor(key)]==true) {scope.launch {if(actions.run(resources.getString(R.string.ui_error_setting)) {app.preferences.toggle(key,true)})pendingMedia=""}}
    }
    val tailscaleInstalled=rememberTailscaleInstalled()
    fun toggle(key: String,value: Boolean) {
        if((key=="wifi" && !value && !p.tailscale)||(key=="tailscale" && !value && !p.wifi)) {scope.launch {snackbar.showSnackbar(resources.getString(R.string.snackbar_need_route))};return}
        if(!value && pendingMedia==key)pendingMedia=""
        if(value && key in listOf("screenshots","photos","recordings") && !context.granted(mediaPermissionFor(key))) {
            pendingMedia=key
            if(PermissionRequests.settings(context as Activity,mediaPermissionFor(key)))PermissionRequests.openSettings(context)
            else {PermissionRequests.mark(context,mediaPermissionFor(key));mediaPermission.launch(arrayOf(mediaPermissionFor(key)))}
        } else scope.launch {actions.run(resources.getString(R.string.ui_error_setting),{toggle(key,value)}) {try {app.preferences.toggle(key,value)}catch(_:IllegalArgumentException) {snackbar.showSnackbar(resources.getString(R.string.snackbar_need_route))}}}
    }
    val scroll=TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    Scaffold(modifier=Modifier.nestedScroll(scroll.nestedScrollConnection),containerColor=Color.Transparent,contentWindowInsets=WindowInsets.statusBars,topBar={LargeTopAppBar(expandedHeight=114.dp,collapsedHeight=56.dp,scrollBehavior=scroll,title={Text(resources.getString(R.string.settings_title),Modifier.padding(start=4.dp),style=if(scroll.state.collapsedFraction>.5f)PneuType.title2 else PneuType.largeTitle)},navigationIcon={NavIcon(back)},colors=TopAppBarDefaults.topAppBarColors(containerColor=LocalPneuColors.current.bg,scrolledContainerColor=LocalPneuColors.current.bg))},snackbarHost={PneuSnackbarHost(snackbar,Modifier.navigationBarsPadding())}) {padding ->
        Box(Modifier.padding(padding).fillMaxSize(),contentAlignment=Alignment.TopCenter) {
            Column(Modifier.widthIn(max=600.dp).fillMaxWidth().verticalScroll(rememberScrollState()).navigationBarsPadding().padding(horizontal=16.dp)) {
                Header(resources.getString(R.string.group_auto))
                PneuCard {
                    ToggleRow(resources.getString(R.string.pref_screenshots),p.screenshots,divider=false) {toggle("screenshots",it)}
                    ToggleRow(resources.getString(R.string.pref_photos),p.photos) {toggle("photos",it)}
                    ToggleRow(resources.getString(R.string.pref_recordings),p.recordings) {toggle("recordings",it)}
                }
                Header(resources.getString(R.string.group_sharing))
                PneuCard {
                    ToggleRow(resources.getString(R.string.pref_share_sheet),p.shareSheet,divider=false) {toggle("shareSheet",it)}
                    ToggleRow(resources.getString(R.string.pref_choose_devices),p.chooseDevicesEachTime) {toggle("chooseDevicesEachTime",it)}
                }
                Header(resources.getString(R.string.group_clipboard))
                PneuCard {
                    ToggleRow(resources.getString(R.string.pref_clipboard_button),p.clipboardButton,divider=false) {toggle("clipboardButton",it)}
                    ToggleRow(resources.getString(R.string.pref_keep_history),history.enabled,enabled=history.loaded) {value->
                        if(!value && history.items.isNotEmpty())historyOff=true
                        else scope.launch {actions.run(resources.getString(R.string.ui_error_setting)) {app.savedClips.setEnabled(value)}}
                    }
                }
                Header(resources.getString(R.string.group_connection))
                PneuCard {
                    ToggleRow(resources.getString(R.string.pref_wifi),p.wifi,divider=false) {toggle("wifi",it)}
                    ToggleRow(resources.getString(R.string.pref_tailscale),p.tailscale,detail=if(!tailscaleInstalled)resources.getString(R.string.pref_tailscale_missing) else null) {toggle("tailscale",it)}
                    ToggleRow(resources.getString(R.string.pref_mobile_data),p.mobile) {toggle("mobile",it)}
                    ToggleRow(resources.getString(R.string.pref_originals),p.originals,enabled=p.mobile) {toggle("originals",it)}
                    ToggleRow(resources.getString(R.string.pref_queue),p.queue) {toggle("queue",it)}
                }
                Header(resources.getString(R.string.group_phone))
                PneuCard {
                    SettingRow(resources.getString(R.string.pref_name),selfName(context,p),divider=false) {renaming=true}
                    ToggleRow(resources.getString(R.string.pref_quick_settings),p.quickSettings) {toggle("quickSettings",it)}
                    SettingRow(resources.getString(R.string.pref_permissions),if(health.issues.isEmpty())resources.getString(R.string.pref_permissions_ok) else resources.getQuantityString(R.plurals.pref_permissions_issues,health.issues.size,health.issues.size),error=health.issues.isNotEmpty(),click=permissions)}
                Spacer(Modifier.height(24.dp))
                PneuCard {
                    SettingRow(resources.getString(R.string.pref_privacy),divider=false,external=true) {scope.launch {actions.run(resources.getString(R.string.ui_error_privacy)) {context.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(resources.getString(R.string.privacy_url))))}}}
                }
                Text(resources.getString(R.string.version,BuildConfig.VERSION_NAME),Modifier.fillMaxWidth().padding(24.dp),style=PneuType.caption,color=LocalPneuColors.current.text3,textAlign=TextAlign.Center)
            }
        }
    }
    if(historyOff)HistoryDeleteDialog(history.items.size,true,{historyOff=false}) {
        scope.launch {if(actions.run(resources.getString(R.string.ui_error_setting)) {app.savedClips.setEnabled(false)})historyOff=false}
    }
    if(renaming)RenameDialog(selfName(context,p),{renaming=false}) {name->
        scope.launch {if(actions.run(resources.getString(R.string.ui_error_setting)) {devicesModel.renameSelf(name)})renaming=false}
    }
}
@Composable private fun ToggleRow(title: String,checked: Boolean,enabled: Boolean=true,detail: String?=null,divider:Boolean=true,change: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().pneuRow(divider).pneuToggle(checked,enabled,onChange=change).heightIn(min=if(detail==null)56.dp else 68.dp).padding(horizontal=16.dp,vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
        Column(Modifier.weight(1f).graphicsLayer {alpha=if(enabled)1f else .4f}) {Text(title,style=PneuType.body);if(detail!=null) Text(detail,style=PneuType.subhead,color=LocalPneuColors.current.danger)}
        PneuSwitch(checked=checked,onCheckedChange=null,enabled=enabled)
    }
}
@Composable private fun SettingRow(title: String,detail: String?=null,error: Boolean=false,divider:Boolean=true,external:Boolean=false,click: (() -> Unit)?=null) {
    val c=LocalPneuColors.current
    Row(Modifier.fillMaxWidth().pneuRow(divider).then(if(click!=null)Modifier.pneuClick(onClick=click)else Modifier).heightIn(min=if(detail==null)56.dp else 68.dp).padding(horizontal=16.dp,vertical=12.dp),verticalAlignment=Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(title,style=PneuType.body,color=if(error&&detail==null)c.danger else c.text,maxLines=1,overflow=TextOverflow.Ellipsis)
            if(detail!=null)Text(detail,style=PneuType.subhead,color=if(error)c.warn else c.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
        }
        if(click!=null && (detail!=null || external))Symbol(if(external)R.drawable.ic_open_in_new else R.drawable.ic_chevron_right,modifier=Modifier.size(20.dp),tint=c.text3)
    }
}
