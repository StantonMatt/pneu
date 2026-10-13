package io.github.stantonmatt.pneu

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
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState); enableEdgeToEdge(SystemBarStyle.auto(android.graphics.Color.TRANSPARENT,android.graphics.Color.TRANSPARENT),SystemBarStyle.auto(android.graphics.Color.TRANSPARENT,android.graphics.Color.TRANSPARENT))
        if(savedInstanceState==null)handleIntent(intent)
        else {pairingCandidate=savedInstanceState.getString("candidate");requestedScreen=savedInstanceState.getString("screen")}
        setContent { PneuScreen(requestedScreen,refresh,{requestedScreen=null},pairingCandidate) {pairingCandidate=it} }
    }
    override fun onSaveInstanceState(outState:Bundle) {
        outState.putString("candidate",pairingCandidate);outState.putString("screen",requestedScreen)
        super.onSaveInstanceState(outState)
    }
    override fun onNewIntent(intent: Intent) { super.onNewIntent(intent); setIntent(intent); handleIntent(intent) }
    private fun handleIntent(intent: Intent) {
        if(intent.action==Intent.ACTION_VIEW && intent.data!=null) {pairingCandidate=intent.data.toString()}
        if(intent.getBooleanExtra("home",false) || intent.action=="android.service.quicksettings.action.QS_TILE_PREFERENCES")requestedScreen="home"
        if(intent.getBooleanExtra("permissions",false)) requestedScreen="permissions"
        if(intent.getBooleanExtra("pair",false)) requestedScreen="pair"
    }
    override fun onResume() { super.onResume(); visible=true; refresh++;graph.requestMediaScan(); lifecycleScope.launch {UiActions.feedback(this@MainActivity).run(getString(R.string.toast_settings)) {val p=graph.awaitPrefs();if((p.peer!=null || p.pending!=null) && !p.pairingLost)PneuService.start(this@MainActivity)}} }
    override fun onPause() { visible=false; super.onPause() }
    companion object { @Volatile var visible=false }
}
suspend fun beginPair(context: android.content.Context, uri: String, expected: ConfirmedBinding=ConfirmedBinding.from(context.graph.prefs.value)): Boolean {
    val graph=context.graph
    val invitation=runCatching { Invitation.parse(uri) }.getOrNull()
    val persisted=graph.preferences.read().pending
    if(invitation == null || invitation.version != 1L) { graph.connection.state.update { it.withPairing(context.getString(R.string.pair_not_pneu)) }; return false }
    if(invitation != persisted && invitation.expires <= System.currentTimeMillis()) { graph.connection.state.update { it.withPairing(context.getString(R.string.pair_expired)) }; return false }
    check(graph.requestPairing(uri,expected)) {"Binding changed during pairing"}
    storageDeadline {graph.prefs.first { it.pending == invitation }}; PneuService.start(context)
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

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun PneuScreen(requested: String? = null, refresh: Int = 0, consume: () -> Unit = {},candidate:String?=null,offerPair:(String?)->Unit={}) {
    val context=LocalContext.current;val resources=LocalResources.current; val app=context.graph; val scope=rememberCoroutineScope()
    val p by app.prefs.collectAsState(); val link by app.connection.state.collectAsState(); val recent by app.recent.collectAsState()
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
    var scannerPairingRevision by rememberSaveable(p.deviceId) {mutableStateOf<Long?>(null)}
    var scannerSession by rememberSaveable(p.deviceId) {mutableIntStateOf(0)}
    var rejectedLinkFeedback by rememberSaveable(p.deviceId,stateSaver=UiFeedbackSaver) {mutableStateOf<UiFeedback?>(null)}
    fun navigate(next:String) {
        if(next!="pair") {
            // The approved pre-install floor belongs to this scanner session.
            // A later pair intent must start from the binding now installed.
            scannerPairingRevision=null
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
    val waiting by app.waitingCount.collectAsState()
    var unpair by remember { mutableStateOf<ConfirmedBinding?>(null) }
    var actionError by remember {mutableStateOf<Pair<UiFeedback,(() -> Unit)?>?>(null)}
    val actions=remember {UiActions {message,retry->actionError=UiFeedback(message) to retry}}
    var health by remember { mutableStateOf(context.health()) }
    LaunchedEffect(refresh,p.recordings,p.neverSleeping) { health=context.health() }
    LaunchedEffect(p.setup,p.deviceId) { if(screen == null && p.deviceId.isNotEmpty()) navigate(if(!p.setup) "setup" else if(p.peer == null && !p.pairingLost) "pair" else "home") }
    LaunchedEffect(requested) { requested?.let { navigate(it); consume() } }
    LaunchedEffect(p.peer,p.pending,p.pairingLost) { if(!p.pairingLost && (p.pending != null || (p.peer != null && screen != "pair"))) actions.run(resources.getString(R.string.ui_error_connection)) {PneuService.start(context)}
        if(p.peer == null && !p.pairingLost && p.setup && screen == "home") navigate("pair") }
    var backProgress by remember {mutableFloatStateOf(0f)}
    PredictiveBackHandler(enabled=screen in listOf("settings","permissions") || (screen=="pair" && p.peer != null)) { events ->
        try {events.collect {backProgress=it.progress};navigate(if(screen=="permissions") permissionOrigin else "home")}
        finally {backProgress=0f}
    }
    CompositionLocalProvider(LocalUiActions provides actions) {
    PneuTheme(dark=dark) {
        val background=LocalPneuColors.current.bg
        Surface(Modifier.fillMaxSize().graphicsLayer {scaleX=1f-backProgress*.06f;scaleY=scaleX;shape=RoundedCornerShape((24*backProgress).dp);clip=backProgress>0f},color=background) {
            AnimatedContent(screen,transitionSpec={fadeIn(androidx.compose.animation.core.tween(PneuMotion.short)) togetherWith fadeOut(androidx.compose.animation.core.tween(PneuMotion.short))},label="Screen") { visibleScreen ->
            when(visibleScreen) {
                "setup","permissions" -> SetupScreen(visibleScreen=="setup",health,refresh,{ navigate(permissionOrigin) },{health=context.health()}) {
                    scope.launch {if(actions.run(resources.getString(R.string.ui_error_setup)) {app.preferences.toggle("setup",true)})navigate("pair")}
                }
                "pair" -> {
                    val session=scannerSession
                    key(session) {PairScreen(p,link,{ if(screen=="pair" && session==scannerSession)navigate("home") },{ if(p.peer != null)navigate("home") else (context as Activity).finish() },{offerPair(it)},lastAttemptedInvitation=lastAttemptedInvitation,initialPairingRevision=scannerPairingRevision,validationFeedback=rejectedLinkFeedback)}
                }
                "home" -> {var homeScrolled by remember {mutableStateOf(false)};Scaffold(containerColor=background,contentWindowInsets=WindowInsets.statusBars,topBar={HomeTopBar(homeScrolled){navigate("settings")}}) { padding ->
                    HomeScreen(p,link,recent,health,Modifier.padding(padding),{permissionOrigin="home";navigate("permissions")},{navigate("pair")},{homeScrolled=it})
                }}
                "settings" -> SettingsScreen(p,health,refresh,{navigate("home")},{permissionOrigin="settings";navigate("permissions")},{unpair=ConfirmedBinding.from(p)})
            }
            }
        }
        val proposed=remember(candidate) {candidate?.let {runCatching {Invitation.parse(it)}.getOrNull()}}
        var confirmationAttempt by remember {mutableIntStateOf(0)}
        val pairingTarget=remember(candidate,confirmationAttempt) {candidate?.let {ConfirmedBinding.from(p)}}
        var permissionPair by rememberSaveable {mutableStateOf<List<String>?>(null)}
        var permissionRevision by remember {mutableIntStateOf(0)}
        fun startConfirmed(raw:String,target:ConfirmedBinding) {
            // Both camera and deep-link confirmations use this owner. Retain before
            // starting so a rejected attempt/pending removal cannot erase Retry.
            lastAttemptedInvitation=raw
            val baseline=p.pairingRevision
            scope.launch {if(actions.run(resources.getString(R.string.ui_error_pairing),{confirmationAttempt++;offerPair(raw)}) {beginPair(context,raw,target)}) {scannerPairingRevision=baseline;offerPair(null);navigate("pair")}}
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
            startConfirmed(approved[0],target)
        }
        val pairingPermission=rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {granted->
            permissionRevision++
            if(granted)finishApproved()
        }
        LaunchedEffect(refresh,candidate,p.deviceId,p.peer?.pin,p.peer?.deviceId,p.pairingRevision) {
            val approved=permissionPair
            if(approved!=null && (approved[0]!=candidate || approved[1]!=p.deviceId || approved[2]!=p.peer?.pin.orEmpty() || approved[3]!=p.peer?.deviceId.orEmpty() || approved[4]!=p.pairingRevision.toString())) {
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
            rejectedLinkFeedback=UiFeedback(resources.getString(R.string.pair_not_pneu))
            offerPair(null)
        }}
        if(proposed!=null && pairingTarget!=null && p.deviceId.isNotEmpty())PneuTheme(dark=if(screen=="pair")true else dark) {PneuDialog(pairingHeader=true,onDismissRequest={offerPair(null)},
            title={Text(if(pairingTarget.pin!=null)resources.getString(R.string.pair_replace_title,pairingTarget.pcName,proposed.name) else resources.getString(R.string.pair_confirm_title,proposed.name))},
            confirmButton={PneuTextButton(onClick={
                if(!pairingTarget.matches(p))actionError=UiFeedback(resources.getString(R.string.ui_error_pairing)) to {confirmationAttempt++;offerPair(candidate)}
                else if(Build.VERSION.SDK_INT>=37 && !context.granted(Manifest.permission.ACCESS_LOCAL_NETWORK)) {
                    permissionPair=listOf(candidate!!,pairingTarget.phoneId,pairingTarget.pin.orEmpty(),pairingTarget.pcId.orEmpty(),pairingTarget.revision.toString(),pairingTarget.pcName)
                    if(networkSettings)PermissionRequests.openSettings(context)
                    else {PermissionRequests.mark(context,Manifest.permission.ACCESS_LOCAL_NETWORK);pairingPermission.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)}
                }
                else startConfirmed(candidate!!,pairingTarget)
            }) {Text(if(networkSettings)resources.getString(R.string.action_open_settings) else if(pairingTarget.pin!=null)resources.getString(R.string.action_replace) else resources.getString(R.string.action_pair))}},dismissButton={PneuTextButton(onClick={offerPair(null)}) {Text(resources.getString(R.string.cd_cancel))}})}
        unpair?.let {confirmed->PneuDialog(destructive=true,onDismissRequest={unpair=null},title={Text(if(confirmed.pin!=null)resources.getString(R.string.unpair_title,confirmed.pcName) else resources.getString(R.string.unpair_finish_title))},
            text=if(waiting>0) ({Text(resources.getQuantityString(R.plurals.unpair_text,waiting,waiting))}) else null,
            confirmButton={PneuTextButton(onClick={unpair=null;scope.launch {actions.run(resources.getString(R.string.ui_error_unpair),{unpair=ConfirmedBinding.from(app.prefs.value)}) {
                check(app.connection.unpair(confirmed)) {"Binding changed during unpair"}
                PneuService.stop(context);navigate("pair")
            }}}) {Text(resources.getString(R.string.action_unpair))}},
            dismissButton={PneuTextButton(onClick={unpair=null}) {Text(resources.getString(R.string.cd_cancel))}})}
        actionError?.let {error->PneuTheme(dark=if(screen=="pair")true else dark) {PneuDialog(onDismissRequest={actionError=null},title={Text(error.first.message)},
            confirmButton={PneuTextButton(onClick={actionError=null;error.second?.invoke()}) {Text(if(error.second!=null)resources.getString(R.string.action_try_again) else resources.getString(R.string.action_close))}},
            dismissButton=if(error.second!=null)({PneuTextButton(onClick={actionError=null}) {Text(resources.getString(R.string.action_close))}}) else null)}}
    }
    }
}
@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun HomeTopBar(scrolled:Boolean,settings:()->Unit) {
    val c=LocalPneuColors.current
    TopAppBar(modifier=Modifier.drawWithContent {drawContent();if(scrolled)drawLine(c.separator,androidx.compose.ui.geometry.Offset(0f,size.height-1f),androidx.compose.ui.geometry.Offset(size.width,size.height-1f),1f)},
        title={Row(Modifier.padding(start=4.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(12.dp)) {
            Symbol(R.drawable.ic_pneu_mark,modifier=Modifier.size(28.dp),tint=Color.Unspecified)
            Text(stringResource(R.string.home_title),style=PneuType.title2)
        }},actions={IconButton(onClick=settings){Symbol(R.drawable.ic_settings,stringResource(R.string.cd_settings))}},
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
                PermissionRow(resources.getString(R.string.perm_photos),R.drawable.ic_photo_library,checked.photos,resources.getString(photoAction),if(limited)resources.getString(R.string.perm_photos_limited) else if(photoAction==R.string.action_settings)resources.getString(R.string.perm_denied) else null) {
                    if(photoAction==R.string.action_settings)details() else request(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED)
                }
                PermissionRow(resources.getString(R.string.perm_battery),R.drawable.ic_battery_full,checked.battery,resources.getString(R.string.action_allow)) {
                    if(PermissionPolicy.batterySettings(checked.backgroundRestricted))details()
                    else runCatching {context.startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,Uri.parse("package:${context.packageName}")))}.onFailure {details()}
                }
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

@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun PairScreen(p: Prefs, link: LinkState, home: () -> Unit, back: () -> Unit, offerPair:(String)->Unit,
                                   lastAttemptedInvitation:String?=null,initialPairingRevision:Long?=null,validationFeedback:UiFeedback?=null,cameraPreview:@Composable (Modifier,(String)->Unit)->Unit={modifier,onCode->PairCamera(modifier,onCode)}) {
    val context=LocalContext.current;val resources=LocalResources.current; val scope=rememberCoroutineScope();val actions=LocalUiActions.current;
    var camera by remember { mutableStateOf(context.granted(Manifest.permission.CAMERA)) }
    var cameraAsked by remember { mutableStateOf(PermissionRequests.asked(context,Manifest.permission.CAMERA)) }
    var lastCode by remember(p.peer?.pin,p.deviceId) { mutableStateOf("") }; var lastRead by remember { mutableLongStateOf(0) }
    // The recovery surface is state, not a replayable snackbar. Its consumed
    // event ID and its visibility must survive recreation together.
    var dialog by rememberSaveable(p.peer?.pin,p.deviceId) { mutableStateOf(false) }
    // A persisted successful install survives loss of READY, even if the UI
    // never observed pending or its transient success marker. Save the floor
    // across recreation; reopening this scanner starts a fresh floor.
    var pairingBaseline by rememberSaveable { mutableStateOf<Long?>(initialPairingRevision ?: if(p.deviceId.isNotEmpty())p.pairingRevision else null) }
    LaunchedEffect(p.deviceId) {if(pairingBaseline==null && p.deviceId.isNotEmpty())pairingBaseline=p.pairingRevision}
    val installed=pairingBaseline?.let {p.pairingRevision>it}==true
    val paired=installed && p.pending==null && DeliveryBinding.from(p)!=null
    val success=paired
    val disconnected=link.status !in listOf("Connected","Connecting…")
    val snackbar=remember { SnackbarHostState() }
    var consumedValidation by rememberSaveable {mutableStateOf<String?>(null)}
    LaunchedEffect(validationFeedback?.id) {validationFeedback?.let {
        if(it.id!=consumedValidation) {
            consumedValidation=it.id
            snackbar.showSnackbar(it.message,duration=SnackbarDuration.Short)
        }
    }}
    val permission=rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { camera=it;cameraAsked=true }
    LaunchedEffect(Unit) { if(!camera && !PermissionRequests.settings(context as Activity,Manifest.permission.CAMERA))actions.run(resources.getString(R.string.ui_error_camera)) {PermissionRequests.mark(context,Manifest.permission.CAMERA);permission.launch(Manifest.permission.CAMERA)} }
    val lifecycle=androidx.lifecycle.compose.LocalLifecycleOwner.current
    DisposableEffect(lifecycle) {
        val observer=androidx.lifecycle.LifecycleEventObserver { _,e -> if(e==androidx.lifecycle.Lifecycle.Event.ON_RESUME) camera=context.granted(Manifest.permission.CAMERA) }
        lifecycle.lifecycle.addObserver(observer);onDispose { lifecycle.lifecycle.removeObserver(observer) }
    }
    var feedbackTimedOut by rememberSaveable(p.pending) { mutableStateOf(false) }
    val busy=p.pending != null && !feedbackTimedOut && link.pairing != resources.getString(R.string.pair_unreachable_title)
    val scanBlocked=busy || (installed && p.peer!=null && !p.pairingLost)
    BackHandler(busy) {}
    LaunchedEffect(busy) {
        if(busy) { delay(15000);if(context.graph.prefs.value.pending==p.pending) {feedbackTimedOut=true;dialog=true} }
    }
    val pairingFeedback=link.pairingFeedback?.takeIf {it.message==link.pairing}
    var consumedPairing by rememberSaveable {mutableStateOf<String?>(null)}
    LaunchedEffect(pairingFeedback?.id) {
        val event=pairingFeedback ?: return@LaunchedEffect
        if(event.id==consumedPairing)return@LaunchedEffect
        consumedPairing=event.id
        when {
            event.message==resources.getString(R.string.pair_unreachable_title) -> dialog=true
            event.message in listOf(resources.getString(R.string.pair_not_pneu),resources.getString(R.string.pair_expired)) || event.message.startsWith("Your PC's clock") -> snackbar.showSnackbar(UiPresentation.pairing(context,event.message),duration=SnackbarDuration.Short)
        }
    }
    LaunchedEffect(paired,p.peer,p.pairingRevision,disconnected) {
        if(paired) {
            dialog=false
            if(!disconnected)delay(800)
            val current=context.graph.prefs.value
            if(current.pairingRevision==p.pairingRevision && DeliveryBinding.from(p)?.matches(current)==true)home()
        }
    }
    PneuTheme(dark=true) {
        Box(Modifier.fillMaxSize().background(Color.Black).semantics {paneTitle=resources.getString(R.string.pair_title)}) {
            if(camera) {
                cameraPreview(Modifier.fillMaxSize()) { raw ->
                    if(!scanBlocked && (raw != lastCode || android.os.SystemClock.elapsedRealtime()-lastRead>3000)) {
                        lastCode=raw;lastRead=android.os.SystemClock.elapsedRealtime()
                        offerPair(raw)
                    }
                }
                ScannerFrame(busy,success,if(busy) resources.getString(R.string.pair_pairing) else if(success) resources.getString(R.string.pair_success,p.peer?.name.orEmpty()) else resources.getString(R.string.pair_instruction))
            } else Column(Modifier.align(Alignment.Center),horizontalAlignment=Alignment.CenterHorizontally,verticalArrangement=Arrangement.spacedBy(22.dp)) {
                Box(Modifier.size(72.dp).background(Color.White.copy(alpha=.14f),CircleShape),contentAlignment=Alignment.Center){Symbol(R.drawable.ic_photo_camera,modifier=Modifier.size(32.dp),tint=Color.White)}
                if(PermissionRequests.settings(context as Activity,Manifest.permission.CAMERA)) Text(resources.getString(R.string.pair_camera_off),color=Color.White.copy(alpha=.75f))
                PneuButton(onClick={
                    if(PermissionRequests.settings(context as Activity,Manifest.permission.CAMERA)) PermissionRequests.openSettings(context)
                    else {PermissionRequests.mark(context,Manifest.permission.CAMERA);permission.launch(Manifest.permission.CAMERA)}
                }) {Text(resources.getString(PermissionPolicy.cameraAction(cameraAsked,(context as Activity).shouldShowRequestPermissionRationale(Manifest.permission.CAMERA))))}
            }
            IconButton(onClick=back,enabled=!busy,modifier=Modifier.statusBarsPadding().padding(start=12.dp,top=8.dp).size(44.dp).background(Color.White.copy(alpha=.14f),CircleShape)) {Symbol(R.drawable.ic_arrow_back,resources.getString(R.string.cd_back),tint=Color.White.copy(alpha=if(busy).4f else 1f))}
            PneuSnackbarHost(snackbar,Modifier.align(Alignment.BottomCenter).navigationBarsPadding())
            if(dialog) PneuDialog(dismissible=false,onDismissRequest={},title={Text(resources.getString(R.string.pair_unreachable_title))},text={Text(resources.getString(R.string.pair_unreachable_text))},confirmButton={PneuTextButton(onClick={val confirmed=ConfirmedBinding.from(p);val pending=p.pending;val code=lastAttemptedInvitation ?: lastCode.takeIf(String::isNotEmpty);dialog=false;feedbackTimedOut=false;scope.launch {if(code!=null)offerPair(code) else actions.run(resources.getString(R.string.ui_error_retry_connection),{dialog=true}) {check(context.graph.retryConnection(confirmed,pending,feedback=pending==null)) {"Pairing changed during retry"}}}}) {Text(resources.getString(R.string.action_try_again))}},dismissButton={PneuTextButton(onClick={val confirmed=ConfirmedBinding.from(p);val pending=p.pending;dialog=false;scope.launch {actions.run(resources.getString(R.string.ui_error_cancel_pairing),{dialog=true}) {check(context.graph.cancelPairing(pending,confirmed)) {"Pairing changed during cancellation"};context.graph.connection.state.update {it.withPairing("")};context.graph.connection.retry();if(p.peer==null) PneuService.stop(context)}}}){Text(resources.getString(R.string.cd_cancel))}})
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable internal fun HomeScreen(p: Prefs,link: LinkState,recent: List<Outgoing>,health: Health,modifier: Modifier,permissions: () -> Unit,pair: () -> Unit,onScrolled:(Boolean)->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current; val app=context.graph; val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    var queueMenu by remember { mutableStateOf(false) }
    val waiting by app.waitingCount.collectAsState()
    var showProgress by remember(link.transferMediaId) {mutableStateOf(false)}
    LaunchedEffect(link.transferMediaId) {if(link.transferMediaId.isNotEmpty()){delay(1000);showProgress=true}}
    val title=if(p.pairingLost) "Pairing lost" else if(p.paused) "Paused" else link.status
    val network=RoutePolicy.active(context.getSystemService(android.net.ConnectivityManager::class.java))
    val tailscaleOff=title=="PC unreachable" && p.tailscale && !network.vpn && p.peer?.addresses?.any {it.tailscale}==true
    val detail=when(title) {
        "Pairing lost" -> resources.getString(R.string.detail_pairing_lost,p.lostPcName.ifEmpty {p.peer?.name.orEmpty()})
        "Paused" -> resources.getString(R.string.detail_paused)
        "Connected" -> resources.getString(R.string.detail_route,p.peer?.name.orEmpty(),UiPresentation.status(context,link.route))
        "Connecting…" -> p.peer?.name.orEmpty()
        "Waiting for Wi-Fi" -> resources.getString(R.string.detail_mobile_data_off)
        else -> if(tailscaleOff) resources.getString(R.string.detail_tailscale_off) else if(p.lastConnected>0) resources.getString(R.string.detail_last_connected,lastConnectedLabel(context,p.lastConnected)) else ""
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
            item {
                Surface(shape=RoundedCornerShape(24.dp),border=if(LocalPneuColors.current.dark)BorderStroke(1.dp,LocalPneuColors.current.hairline)else null,color=groupColor(),modifier=Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(start=20.dp,end=20.dp,top=24.dp,bottom=20.dp),horizontalAlignment=Alignment.CenterHorizontally) {
                        LinkView(link.copy(status=title),sending=link.transferMediaId.isNotEmpty())
                        Crossfade(title,animationSpec=androidx.compose.animation.core.tween(PneuMotion.short),label="Connection status") {Text(UiPresentation.status(context,it),Modifier.fillMaxWidth().padding(top=16.dp),style=PneuType.title1,textAlign=TextAlign.Center)}
                        if(detail.isNotEmpty()) Text(detail,Modifier.padding(top=4.dp),style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onSurfaceVariant,maxLines=2,overflow=TextOverflow.Ellipsis,textAlign=TextAlign.Center)
                        FlowRow(Modifier.fillMaxWidth().padding(top=20.dp),horizontalArrangement=Arrangement.spacedBy(10.dp),verticalArrangement=Arrangement.spacedBy(10.dp),maxItemsInEachRow=if(androidx.compose.ui.platform.LocalDensity.current.fontScale>1.3f)1 else 2) {
                            if(p.pairingLost) PneuButton(pair,Modifier.fillMaxWidth().height(48.dp)) {Symbol(R.drawable.ic_qr_code_scanner,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_pair_again))}
                            else {
                                PneuTonalButton(onClick={if(tailscaleOff) context.packageManager.getLaunchIntentForPackage("com.tailscale.ipn")?.let { context.startActivity(it) } else context.startActivity(ManualEntries.clipboardIntent(context,p))},modifier=Modifier.weight(1f).height(48.dp),enabled=link.status=="Connected" || tailscaleOff,contentPadding=PaddingValues(horizontal=12.dp)) {Symbol(if(tailscaleOff)R.drawable.ic_open_in_new else R.drawable.ic_content_paste_go,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(if(tailscaleOff) resources.getString(R.string.action_open_tailscale) else resources.getString(R.string.tile_send_clipboard))}
                                if(p.paused) PneuButton(onClick={scope.launch {actions.run(resources.getString(R.string.ui_error_resume)) {app.preferences.toggle("paused",false)}}},modifier=Modifier.weight(1f).height(48.dp)) {Symbol(R.drawable.ic_play_arrow,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_resume))}
                                else PneuNeutralButton(onClick={scope.launch {actions.run(resources.getString(R.string.ui_error_pause)) {app.preferences.toggle("paused",true)}}},modifier=Modifier.weight(1f).height(48.dp)) {Symbol(R.drawable.ic_pause,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_pause))}
                            }
                        }
                    }
                }
            }
            item {
                Row(Modifier.fillMaxWidth().padding(start=16.dp,end=8.dp,top=20.dp,bottom=8.dp).height(32.dp),verticalAlignment=Alignment.CenterVertically) {
                    Text(resources.getString(R.string.recent_title),Modifier.weight(1f),style=PneuType.section,color=LocalPneuColors.current.text2)
                    if(waiting>0) Box {
                        Row(Modifier.height(32.dp).clip(RoundedCornerShape(10.dp)).pneuClick {queueMenu=true}.padding(start=10.dp,end=6.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(2.dp)) {
                            Text(resources.getQuantityString(R.plurals.recent_waiting,waiting,waiting),style=PneuType.section,color=LocalPneuColors.current.accentText)
                            Symbol(R.drawable.ic_expand_more,modifier=Modifier.size(18.dp),tint=LocalPneuColors.current.accentText)
                        }
                        DropdownMenu(queueMenu,{queueMenu=false},modifier=Modifier.widthIn(min=196.dp),shape=RoundedCornerShape(16.dp),containerColor=LocalPneuColors.current.raised,shadowElevation=PneuElevation.menu,border=if(LocalPneuColors.current.dark)BorderStroke(1.dp,LocalPneuColors.current.hairline)else null) {
                            DropdownMenuItem(modifier=Modifier.height(44.dp),text={Text(resources.getString(R.string.recent_send_now),style=PneuType.body.copy(fontSize=15.sp))},onClick={val confirmed=ConfirmedBinding.from(p);val pending=p.pending;queueMenu=false;scope.launch {actions.run(resources.getString(R.string.ui_error_retry_connection)) {check(app.retryConnection(confirmed,pending)) {"Pairing changed during retry"}}}})
                            DropdownMenuItem(modifier=Modifier.height(44.dp),text={Text(resources.getString(R.string.recent_dont_send_any),style=PneuType.body.copy(fontSize=15.sp),color=LocalPneuColors.current.danger)},onClick={val selected=ConfirmedQueue.from(p,app.work.value);queueMenu=false;scope.launch {actions.run(resources.getString(R.string.ui_error_clear_queue)) {check(app.discardAll(selected)) {"Queue changed"}}}})
                        }
                    }
                }
            }
            if(recent.isEmpty()) item {PneuCard {Column(Modifier.fillMaxWidth().height(132.dp),horizontalAlignment=Alignment.CenterHorizontally,verticalArrangement=Arrangement.Center) {Symbol(R.drawable.ic_image,modifier=Modifier.size(28.dp),tint=LocalPneuColors.current.text3);Text(resources.getString(R.string.recent_empty),Modifier.padding(top=8.dp),style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onSurfaceVariant)}}}
            else items(recent,key={it.mediaId}) {row ->
                Surface(color=groupColor(),modifier=Modifier.then(if(LocalPneuMotion.current)Modifier.animateItem(fadeInSpec=androidx.compose.animation.core.tween(200),fadeOutSpec=androidx.compose.animation.core.tween(200))else Modifier).fillMaxWidth().pneuCardRowHairline(row==recent.first(),row==recent.last()),shape=RoundedCornerShape(topStart=if(row==recent.first())24.dp else 0.dp,topEnd=if(row==recent.first())24.dp else 0.dp,bottomStart=if(row==recent.last())24.dp else 0.dp,bottomEnd=if(row==recent.last())24.dp else 0.dp)) {
                    val progress=link.transferMediaId==row.mediaId && showProgress
                    Row(Modifier.fillMaxWidth().pneuRow(row!=recent.first(),76.dp).heightIn(min=68.dp).pneuClick(enabled=row.state=="sent" && (if(row.kind=="text")row.text.matches(Regex("""https?://\S+""")) else row.uri.isNotEmpty())) {scope.launch {if(row.kind=="text")runCatching {context.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(row.text)))} else RecentItems.openOriginal(context,row)}}.padding(horizontal=16.dp,vertical=12.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
                        MediaThumb(row)
                        Column(Modifier.weight(1f)) {
                            Text(when(row.kind) {"photo"->resources.getString(R.string.kind_photo);"recording"->resources.getString(R.string.kind_recording);"file"->row.fileName;"text"->row.text.lineSequence().firstOrNull().orEmpty().removePrefix("https://");"clipboard_image"->resources.getString(R.string.kind_image);else->resources.getString(R.string.kind_screenshot)},style=MaterialTheme.typography.bodyLarge,maxLines=1,overflow=if(row.kind=="file")TextOverflow.MiddleEllipsis else TextOverflow.Ellipsis)
                            val supporting=when {
                                progress -> resources.getString(R.string.item_sending,android.text.format.Formatter.formatShortFileSize(context,link.sent),android.text.format.Formatter.formatShortFileSize(context,link.total))
                                row.state=="sent" -> if(row.kind=="clipboard_image" || row.kind=="text" && row.origin=="clipboard")resources.getString(R.string.item_clipboard_time,lastConnectedLabel(context,row.createdAt)) else lastConnectedLabel(context,row.createdAt)
                                row.state=="failed" -> resources.getString(UiPresentation.failedItem(row.error))
                                DeliveryPolicy.hold(row.kind,row.automatic,false,network.mobile,p.originals,row.attempted,row.optimized) -> resources.getString(R.string.item_waiting_wifi)
                                else -> resources.getString(R.string.item_waiting)
                            }
                            Text(supporting,style=MaterialTheme.typography.bodyMedium,color=if(row.state=="failed") LocalPneuColors.current.danger else if(progress)LocalPneuColors.current.accentText else LocalPneuColors.current.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
                            if(progress)LinearProgressIndicator(progress={if(link.total>0)(link.sent.toFloat()/link.total).coerceIn(0f,1f)else 0f},modifier=Modifier.fillMaxWidth().padding(top=7.dp).height(3.dp).clip(RoundedCornerShape(2.dp)),color=LocalPneuColors.current.accent,trackColor=LocalPneuColors.current.fill2,gapSize=0.dp,drawStopIndicator={})
                        }
                        if(row.state=="failed") IconButton(modifier=Modifier.size(40.dp),onClick={val selected=ConfirmedQueue(ConfirmedBinding.from(p),listOf(ConfirmedItem.from(row)));scope.launch {actions.run(resources.getString(R.string.ui_error_retry_item)) {check(app.retry(row.mediaId,selected)) {"Queue item changed"}}}}) {Symbol(R.drawable.ic_refresh,resources.getString(R.string.cd_retry),modifier=Modifier.size(22.dp),tint=LocalPneuColors.current.text2)}
                        if(row.state!="sent") Box(contentAlignment=Alignment.Center) {
                            IconButton(modifier=Modifier.size(40.dp),onClick={val selected=ConfirmedQueue(ConfirmedBinding.from(p),listOf(ConfirmedItem.from(row)));scope.launch {actions.run(resources.getString(R.string.ui_error_remove_item)) {check(app.discard(selected)) {"Queue item changed"}}}}) {Symbol(R.drawable.ic_close,if(progress)resources.getString(R.string.cd_cancel) else if(row.state=="failed")resources.getString(R.string.cd_remove) else resources.getString(R.string.cd_dont_send),modifier=Modifier.size(22.dp),tint=LocalPneuColors.current.text2)}
                        }
                    }
                }
            }
        }
    }
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
@Composable private fun SettingsScreen(p: Prefs,health: Health,refresh:Int,back: () -> Unit,permissions: () -> Unit,unpair: () -> Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val app=context.graph;val scope=rememberCoroutineScope();val actions=LocalUiActions.current;val snackbar=remember {SnackbarHostState()}
    var pendingMedia by rememberSaveable {mutableStateOf("")}
    fun mediaPermissionFor(key:String)=if(key=="recordings")Manifest.permission.READ_MEDIA_VIDEO else Manifest.permission.READ_MEDIA_IMAGES
    LaunchedEffect(refresh,pendingMedia) {if(pendingMedia.isNotEmpty() && context.granted(mediaPermissionFor(pendingMedia))) {val key=pendingMedia;if(actions.run(resources.getString(R.string.ui_error_setting)) {app.preferences.toggle(key,true)})pendingMedia=""}}
    val mediaPermission=rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {result ->
        val key=pendingMedia
        if(key.isNotEmpty() && result[mediaPermissionFor(key)]==true) {scope.launch {if(actions.run(resources.getString(R.string.ui_error_setting)) {app.preferences.toggle(key,true)})pendingMedia=""}}
    }
    val tailscaleInstalled=remember {runCatching {context.packageManager.getPackageInfo("com.tailscale.ipn",0)}.isSuccess}
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
                PneuCard {
                    Row(Modifier.fillMaxWidth().heightIn(min=76.dp).padding(16.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
                        val c=LocalPneuColors.current
                        Box(Modifier.size(44.dp).background(if(p.pairingLost)c.dangerSoft else c.accentSoft,RoundedCornerShape(14.dp)),contentAlignment=Alignment.Center){Symbol(if(p.pairingLost)R.drawable.ic_link_off else R.drawable.ic_computer,tint=if(p.pairingLost)c.danger else c.onAccentSoft)}
                        Column(Modifier.weight(1f)) {
                            Text(p.peer?.name ?: p.lostPcName.ifEmpty {resources.getString(R.string.not_paired)},style=PneuType.headline)
                            val identityDetail=if(p.pairingLost)resources.getString(R.string.status_pairing_lost) else p.peer?.addresses?.let {UiPresentation.pairedAddresses(it).joinToString(" · ")}.orEmpty()
                            if(identityDetail.isNotEmpty())Text(identityDetail,style=PneuType.subhead,color=if(p.pairingLost)c.danger else c.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
                        }
                    }
                    SettingRow(resources.getString(R.string.action_unpair),error=true,click=unpair)
                }
                Header(resources.getString(R.string.group_auto))
                PneuCard {
                    ToggleRow(resources.getString(R.string.pref_screenshots),p.screenshots,divider=false) {toggle("screenshots",it)}
                    ToggleRow(resources.getString(R.string.pref_photos),p.photos) {toggle("photos",it)}
                    ToggleRow(resources.getString(R.string.pref_recordings),p.recordings) {toggle("recordings",it)}
                }
                Header(resources.getString(R.string.group_clipboard))
                PneuCard {
                    ToggleRow(resources.getString(R.string.pref_clipboard_button),p.clipboardButton,divider=false) {toggle("clipboardButton",it)}
                    ToggleRow(resources.getString(R.string.pref_clipboard_from_pc),p.clipboardFromPc) {toggle("clipboardFromPc",it)}
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
                    ToggleRow(resources.getString(R.string.pref_share_sheet),p.shareSheet,divider=false) {toggle("shareSheet",it)}
                    ToggleRow(resources.getString(R.string.pref_quick_settings),p.quickSettings) {toggle("quickSettings",it)}
                    SettingRow(resources.getString(R.string.pref_permissions),if(health.issues.isEmpty())resources.getString(R.string.pref_permissions_ok) else resources.getQuantityString(R.plurals.pref_permissions_issues,health.issues.size,health.issues.size),error=health.issues.isNotEmpty(),click=permissions)}
                Text(resources.getString(R.string.version,BuildConfig.VERSION_NAME),Modifier.fillMaxWidth().padding(24.dp),style=PneuType.caption,color=LocalPneuColors.current.text3,textAlign=TextAlign.Center)
            }
        }
    }
}
@Composable private fun ToggleRow(title: String,checked: Boolean,enabled: Boolean=true,detail: String?=null,divider:Boolean=true,change: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().pneuRow(divider).pneuToggle(checked,enabled,change).heightIn(min=if(detail==null)56.dp else 68.dp).padding(horizontal=16.dp,vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
        Column(Modifier.weight(1f).graphicsLayer {alpha=if(enabled)1f else .4f}) {Text(title,style=PneuType.body);if(detail!=null) Text(detail,style=PneuType.subhead,color=LocalPneuColors.current.danger)}
        PneuSwitch(checked=checked,onCheckedChange=null,enabled=enabled)
    }
}
@Composable private fun SettingRow(title: String,detail: String?=null,error: Boolean=false,click: (() -> Unit)?=null) {
    val c=LocalPneuColors.current
    Row(Modifier.fillMaxWidth().pneuRow(true).then(if(click!=null)Modifier.pneuClick(onClick=click)else Modifier).heightIn(min=if(detail==null)56.dp else 68.dp).padding(horizontal=16.dp,vertical=12.dp),verticalAlignment=Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(title,style=PneuType.body,color=if(error&&detail==null)c.danger else c.text,maxLines=1,overflow=TextOverflow.Ellipsis)
            if(detail!=null)Text(detail,style=PneuType.subhead,color=if(error)c.warn else c.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
        }
        if(click!=null && detail!=null)Symbol(R.drawable.ic_chevron_right,modifier=Modifier.size(20.dp),tint=c.text3)
    }
}
