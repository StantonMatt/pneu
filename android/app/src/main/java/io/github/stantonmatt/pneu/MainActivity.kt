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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.flow.first
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
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
        super.onCreate(savedInstanceState); enableEdgeToEdge()
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
        if(intent.action==Intent.ACTION_VIEW && intent.data!=null) {requestedScreen="pair";pairingCandidate=intent.data.toString()}
        if(intent.getBooleanExtra("home",false) || intent.action=="android.service.quicksettings.action.QS_TILE_PREFERENCES")requestedScreen="home"
        if(intent.getBooleanExtra("permissions",false)) requestedScreen="permissions"
        if(intent.getBooleanExtra("pair",false)) requestedScreen="pair"
    }
    override fun onResume() { super.onResume(); visible=true; refresh++;graph.requestMediaScan(); lifecycleScope.launch {UiActions.feedback(this@MainActivity).run("Couldn't load Pneu settings. Open Pneu and try again.") {val p=graph.awaitPrefs();if((p.peer!=null || p.pending!=null) && !p.pairingLost)PneuService.start(this@MainActivity)}} }
    override fun onPause() { visible=false; super.onPause() }
    companion object { @Volatile var visible=false }
}
suspend fun beginPair(context: android.content.Context, uri: String, expected: ConfirmedBinding=ConfirmedBinding.from(context.graph.prefs.value)): Boolean {
    val graph=context.graph
    val invitation=runCatching { Invitation.parse(uri) }.getOrNull()
    val persisted=graph.preferences.read().pending
    if(invitation == null || invitation.version != 1L) { graph.connection.state.update { it.copy(pairing="That isn't a Pneu code") }; return false }
    if(invitation != persisted && invitation.expires <= System.currentTimeMillis()) { graph.connection.state.update { it.copy(pairing="This code expired. Scan the new one.") }; return false }
    check(graph.requestPairing(uri,expected)) {"Binding changed during pairing"}
    storageDeadline {graph.prefs.first { it.pending == invitation }}; PneuService.start(context)
    return true
}
fun lastConnectedLabel(context: android.content.Context, timestamp: Long): String {
    if(timestamp == 0L) return "—"
    val now=Calendar.getInstance(); val date=Calendar.getInstance().apply { timeInMillis=timestamp }
    if(now.get(Calendar.YEAR)==date.get(Calendar.YEAR) && now.get(Calendar.DAY_OF_YEAR)==date.get(Calendar.DAY_OF_YEAR)) return android.text.format.DateFormat.getTimeFormat(context).format(Date(timestamp))
    now.add(Calendar.DAY_OF_YEAR,-1)
    if(now.get(Calendar.YEAR)==date.get(Calendar.YEAR) && now.get(Calendar.DAY_OF_YEAR)==date.get(Calendar.DAY_OF_YEAR)) return "Yesterday"
    return SimpleDateFormat("d MMM",Locale.getDefault()).format(Date(timestamp))
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun PneuScreen(requested: String? = null, refresh: Int = 0, consume: () -> Unit = {},candidate:String?=null,offerPair:(String?)->Unit={}) {
    val context=LocalContext.current; val app=context.graph; val scope=rememberCoroutineScope()
    val p by app.prefs.collectAsState(); val link by app.connection.state.collectAsState(); val recent by app.recent.collectAsState()
    val dark=isSystemInDarkTheme()
    val colors=if(dark) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
    if(!p.loaded || p.deviceId.isEmpty()) {
        var failed by remember {mutableStateOf(false)}
        var attempt by remember {mutableIntStateOf(0)}
        LaunchedEffect(attempt) {
            failed=false
            try {storageDeadline {app.prefs.first {it.loaded && it.deviceId.isNotEmpty()}}}catch(e:Exception) {if(e is CancellationException)throw e;failed=true}
        }
        MaterialTheme(colorScheme=colors) {Surface(Modifier.fillMaxSize()) {Box(Modifier.fillMaxSize(),contentAlignment=Alignment.Center) {
            if(failed)Column(horizontalAlignment=Alignment.CenterHorizontally) {Text("Couldn't load Pneu settings");TextButton(onClick={attempt++}) {Text("Try again")}}
            else CircularProgressIndicator()
        }}}
        return
    }
    var screen by rememberSaveable { mutableStateOf<String?>(null) }
    var lastAttemptedInvitation by rememberSaveable(p.deviceId) {mutableStateOf<String?>(null)}
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
    var actionError by remember {mutableStateOf<Pair<String,(() -> Unit)?>?>(null)}
    val actions=remember {UiActions {message,retry->actionError=message to retry}}
    var health by remember { mutableStateOf(context.health()) }
    LaunchedEffect(refresh,p.recordings,p.neverSleeping) { health=context.health() }
    LaunchedEffect(p.setup,p.deviceId) { if(screen == null && p.deviceId.isNotEmpty()) screen=if(!p.setup) "setup" else if(p.peer == null && !p.pairingLost) "pair" else "home" }
    LaunchedEffect(requested) { requested?.let { screen=it; consume() } }
    LaunchedEffect(p.peer,p.pending,p.pairingLost) { if(!p.pairingLost && (p.pending != null || (p.peer != null && screen != "pair"))) actions.run("Couldn't start connection") {PneuService.start(context)}
        if(p.peer == null && !p.pairingLost && p.setup && screen == "home") screen="pair" }
    BackHandler(enabled=screen in listOf("settings","permissions") || (screen=="pair" && p.peer != null)) { screen=if(screen=="permissions") permissionOrigin else "home" }
    CompositionLocalProvider(LocalUiActions provides actions) {
    MaterialTheme(colorScheme=colors) {
        val background=if(dark) colors.surface else colors.surfaceContainer
        Surface(Modifier.fillMaxSize(),color=background) {
            when(screen) {
                "setup","permissions" -> SetupScreen(screen=="setup",health,refresh,{ screen=permissionOrigin },{health=context.health()}) {
                    scope.launch {if(actions.run("Couldn't save setup") {app.preferences.toggle("setup",true)})screen="pair"}
                }
                "pair" -> PairScreen(p,link,{ screen="home" },{ if(p.peer != null) screen="home" else (context as Activity).finish() },{offerPair(it)},lastAttemptedInvitation=lastAttemptedInvitation)
                "home" -> Scaffold(containerColor=background,topBar={ TopAppBar(title={Text("Pneu")},actions={ IconButton(onClick={screen="settings"}) { Symbol(R.drawable.ic_settings,"Settings") } },colors=TopAppBarDefaults.topAppBarColors(containerColor=background)) }) { padding ->
                    HomeScreen(p,link,recent,health,Modifier.padding(padding),{permissionOrigin="home";screen="permissions"},{screen="pair"})
                }
                "settings" -> SettingsScreen(p,health,refresh,{screen="home"},{permissionOrigin="settings";screen="permissions"},{unpair=ConfirmedBinding.from(p)})
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
            scope.launch {if(actions.run("Couldn't start pairing",{confirmationAttempt++;offerPair(raw)}) {beginPair(context,raw,target)})offerPair(null)}
        }
        fun finishApproved() {
            val approved=permissionPair ?: return
            val target=ConfirmedBinding(approved[1],approved[2].ifEmpty {null},approved[3].ifEmpty {null},approved[4].toLong(),approved[5])
            if(approved[0]!=candidate || !target.matches(p)) {
                permissionPair=null
                if(approved[0]==candidate)actionError="Couldn't start pairing" to {confirmationAttempt++;offerPair(approved[0])}
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
                if(approved[0]==candidate)actionError="Couldn't start pairing" to {confirmationAttempt++;offerPair(approved[0])}
            }
            else if(approved!=null && context.granted(Manifest.permission.ACCESS_LOCAL_NETWORK))finishApproved()
        }
        val networkSettings=remember(refresh,permissionRevision) {Build.VERSION.SDK_INT>=37 && !context.granted(Manifest.permission.ACCESS_LOCAL_NETWORK) && PermissionRequests.settings(context as Activity,Manifest.permission.ACCESS_LOCAL_NETWORK)}
        LaunchedEffect(candidate) {if(candidate!=null && proposed==null) {app.connection.state.update {it.copy(pairing="That isn't a Pneu code")};offerPair(null)}}
        if(proposed!=null && pairingTarget!=null && p.deviceId.isNotEmpty())AlertDialog(onDismissRequest={offerPair(null)},
            title={Text(if(pairingTarget.pin!=null)"Replace ${pairingTarget.pcName} with ${proposed.name}?" else "Pair with ${proposed.name}?")},
            confirmButton={TextButton(onClick={
                if(!pairingTarget.matches(p))actionError="Couldn't start pairing" to {confirmationAttempt++;offerPair(candidate)}
                else if(Build.VERSION.SDK_INT>=37 && !context.granted(Manifest.permission.ACCESS_LOCAL_NETWORK)) {
                    permissionPair=listOf(candidate!!,pairingTarget.phoneId,pairingTarget.pin.orEmpty(),pairingTarget.pcId.orEmpty(),pairingTarget.revision.toString(),pairingTarget.pcName)
                    if(networkSettings)PermissionRequests.openSettings(context)
                    else {PermissionRequests.mark(context,Manifest.permission.ACCESS_LOCAL_NETWORK);pairingPermission.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)}
                }
                else startConfirmed(candidate!!,pairingTarget)
            }) {Text(if(networkSettings)"Open settings" else if(pairingTarget.pin!=null)"Replace" else "Pair")}},dismissButton={TextButton(onClick={offerPair(null)}) {Text("Cancel")}})
        unpair?.let {confirmed->AlertDialog(onDismissRequest={unpair=null},title={Text(if(confirmed.pin!=null)"Unpair from ${confirmed.pcName}?" else "Finish unpairing?")},
            text=if(waiting>0) ({Text(context.resources.getQuantityString(R.plurals.unpair_text,waiting,waiting))}) else null,
            confirmButton={TextButton(onClick={unpair=null;scope.launch {actions.run("Couldn't unpair",{unpair=ConfirmedBinding.from(app.prefs.value)}) {
                check(app.connection.unpair(confirmed)) {"Binding changed during unpair"}
                context.stopService(Intent(context,PneuService::class.java));screen="pair"
            }}}) {Text("Unpair")}},
            dismissButton={TextButton(onClick={unpair=null}) {Text("Cancel")}})}
        actionError?.let {error->AlertDialog(onDismissRequest={actionError=null},title={Text(error.first)},
            confirmButton={TextButton(onClick={actionError=null;error.second?.invoke()}) {Text(if(error.second!=null)"Try again" else "Close")}},
            dismissButton=if(error.second!=null)({TextButton(onClick={actionError=null}) {Text("Close")}}) else null)}
    }
    }
}
@Composable fun Symbol(id: Int, description: String? = null, modifier: Modifier = Modifier.size(24.dp), tint: Color = LocalContentColor.current) { Icon(painterResource(id),description,modifier,tint) }
@Composable private fun groupColor() = if(isSystemInDarkTheme()) MaterialTheme.colorScheme.surfaceContainerHigh else MaterialTheme.colorScheme.surfaceContainerLowest
@Composable private fun Group(content: @Composable ColumnScope.() -> Unit) { Surface(shape=RoundedCornerShape(24.dp),color=groupColor(),modifier=Modifier.fillMaxWidth()) { Column(content=content) } }
@Composable private fun Header(text: String) { Text(text,Modifier.padding(start=16.dp,top=24.dp,bottom=8.dp),style=MaterialTheme.typography.titleSmall,color=MaterialTheme.colorScheme.primary) }
@Composable private fun NavIcon(back: () -> Unit) { IconButton(onClick=back) { Symbol(R.drawable.ic_arrow_back,"Back") } }

@OptIn(ExperimentalMaterial3Api::class)
@Composable private fun SetupScreen(first: Boolean, health: Health, refresh: Int, back: () -> Unit, repair:()->Unit, next: () -> Unit) {
    val context=LocalContext.current; val scope=rememberCoroutineScope();val actions=LocalUiActions.current
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
    LaunchedEffect(refresh) { if(returnedFromSleeping && actions.run("Couldn't save battery settings") {withContext(Dispatchers.IO) {ServiceEvidence.acknowledge(context)};context.graph.preferences.toggle("neverSleeping",true)}) {returnedFromSleeping=false;recheck++;repair()} }
    Scaffold(containerColor=Color.Transparent,topBar={LargeTopAppBar(title={Text(if(first) "Set up Pneu" else "Permissions and battery")},navigationIcon={if(!first) NavIcon(back)},colors=TopAppBarDefaults.topAppBarColors(containerColor=Color.Transparent))},bottomBar={if(first) Box(Modifier.navigationBarsPadding().padding(16.dp)) { Button(next,Modifier.fillMaxWidth().height(56.dp)) {Text("Continue")} }}) { padding ->
        Column(Modifier.padding(padding).fillMaxWidth().widthIn(max=600.dp).padding(horizontal=16.dp).verticalScroll(rememberScrollState())) {
            Group {
                val activity=context as Activity
                if(Build.VERSION.SDK_INT>=37)PermissionRow("Local network",R.drawable.ic_computer,checked.localNetwork,if(PermissionRequests.settings(activity,Manifest.permission.ACCESS_LOCAL_NETWORK))"Settings" else "Allow") {
                    if(PermissionRequests.settings(activity,Manifest.permission.ACCESS_LOCAL_NETWORK))details() else request(Manifest.permission.ACCESS_LOCAL_NETWORK)
                }
                val notificationSettings=checked.channelBlocked || context.granted(Manifest.permission.POST_NOTIFICATIONS) || PermissionRequests.settings(activity,Manifest.permission.POST_NOTIFICATIONS)
                PermissionRow("Notifications",R.drawable.ic_notifications,checked.notifications,if(notificationSettings)"Settings" else "Allow") {
                    if(notificationSettings)context.startActivity(Intent(if(checked.channelBlocked)Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS else Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE,context.packageName).putExtra(Settings.EXTRA_CHANNEL_ID,"status"))
                    else request(Manifest.permission.POST_NOTIFICATIONS)
                }
                val limited=context.granted(Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED) && !checked.photos
                val photoAction=PermissionPolicy.photoAction(limited,PermissionRequests.asked(context,Manifest.permission.READ_MEDIA_IMAGES),activity.shouldShowRequestPermissionRationale(Manifest.permission.READ_MEDIA_IMAGES) || activity.shouldShowRequestPermissionRationale(Manifest.permission.READ_MEDIA_VIDEO))
                PermissionRow("Photos and videos",R.drawable.ic_photo_library,checked.photos,photoAction,if(limited)"Limited access" else if(photoAction=="Settings")"Turned off in settings" else null) {
                    if(photoAction=="Settings")details() else request(Manifest.permission.READ_MEDIA_IMAGES,Manifest.permission.READ_MEDIA_VIDEO,Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED)
                }
                PermissionRow("Unrestricted battery use",R.drawable.ic_battery_full,checked.battery,"Allow") {
                    if(PermissionPolicy.batterySettings(checked.backgroundRestricted))details()
                    else runCatching {context.startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,Uri.parse("package:${context.packageName}")))}.onFailure {details()}
                }
                if(checked.killed && (checked.sleeping || !Build.MANUFACTURER.equals("samsung",true)))PermissionRow("Pneu was stopped in the background",R.drawable.ic_bedtime_off,false,"Dismiss") {
                    scope.launch {if(actions.run("Couldn't acknowledge background stop") {withContext(Dispatchers.IO) {ServiceEvidence.acknowledge(context)}}) {recheck++;repair()}}
                }
                if(Build.MANUFACTURER.equals("samsung",true)) PermissionRow("Never sleeping apps",R.drawable.ic_bedtime_off,checked.sleeping,"Open",if(checked.sleeping) null else "Add Pneu to the list") {
                    returnedFromSleeping=SettingsNavigation.openSamsungBattery(context.packageName,context::startActivity)
                }
            }
        }
    }
}
@Composable private fun PermissionRow(title: String, icon: Int, done: Boolean, action: String, detail: String? = null, click: () -> Unit) {
    Row(Modifier.fillMaxWidth().heightIn(min=if(detail==null)56.dp else 72.dp).padding(horizontal=16.dp,vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
        Symbol(icon,tint=MaterialTheme.colorScheme.onSurfaceVariant)
        Column(Modifier.weight(1f)) {Text(title,style=MaterialTheme.typography.bodyLarge);if(detail!=null) Text(detail,style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onSurfaceVariant)}
        if(done) Symbol(R.drawable.ic_check_circle,"Granted",tint=MaterialTheme.colorScheme.primary) else FilledTonalButton(click,contentPadding=PaddingValues(horizontal=16.dp)) {Text(action)}
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun PairScreen(p: Prefs, link: LinkState, home: () -> Unit, back: () -> Unit, offerPair:(String)->Unit,
                                   lastAttemptedInvitation:String?=null,cameraPreview:@Composable (Modifier,(String)->Unit)->Unit={modifier,onCode->PairCamera(modifier,onCode)}) {
    val context=LocalContext.current; val scope=rememberCoroutineScope();val actions=LocalUiActions.current; val dark=dynamicDarkColorScheme(context)
    var camera by remember { mutableStateOf(context.granted(Manifest.permission.CAMERA)) }
    var cameraAsked by remember { mutableStateOf(PermissionRequests.asked(context,Manifest.permission.CAMERA)) }
    var lastCode by remember(p.peer?.pin,p.deviceId) { mutableStateOf("") }; var lastRead by remember { mutableLongStateOf(0) }
    var dialog by remember(p.peer?.pin,p.deviceId) { mutableStateOf(false) }
    // A persisted successful install survives loss of READY, even if the UI
    // never observed pending or its transient success marker. Save the floor
    // across recreation; reopening this scanner starts a fresh floor.
    var pairingBaseline by rememberSaveable { mutableStateOf<Long?>(if(p.deviceId.isNotEmpty())p.pairingRevision else null) }
    LaunchedEffect(p.deviceId) {if(pairingBaseline==null && p.deviceId.isNotEmpty())pairingBaseline=p.pairingRevision}
    val installed=pairingBaseline?.let {p.pairingRevision>it}==true
    val paired=installed && p.pending==null && DeliveryBinding.from(p)!=null
    val success=paired
    val disconnected=link.status !in listOf("Connected","Connecting…")
    val snackbar=remember { SnackbarHostState() }
    val permission=rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { camera=it;cameraAsked=true }
    LaunchedEffect(Unit) { if(!camera && !PermissionRequests.settings(context as Activity,Manifest.permission.CAMERA))actions.run("Couldn't request camera access") {PermissionRequests.mark(context,Manifest.permission.CAMERA);permission.launch(Manifest.permission.CAMERA)} }
    val lifecycle=androidx.lifecycle.compose.LocalLifecycleOwner.current
    DisposableEffect(lifecycle) {
        val observer=androidx.lifecycle.LifecycleEventObserver { _,e -> if(e==androidx.lifecycle.Lifecycle.Event.ON_RESUME) camera=context.granted(Manifest.permission.CAMERA) }
        lifecycle.lifecycle.addObserver(observer);onDispose { lifecycle.lifecycle.removeObserver(observer) }
    }
    var feedbackTimedOut by remember(p.pending) { mutableStateOf(false) }
    val busy=p.pending != null && !feedbackTimedOut && link.pairing != "Can't reach your PC"
    val scanBlocked=busy || (installed && p.peer!=null && !p.pairingLost)
    BackHandler(busy) {}
    LaunchedEffect(busy) {
        if(busy) { delay(15000);if(context.graph.prefs.value.pending==p.pending) {feedbackTimedOut=true;dialog=true} }
    }
    LaunchedEffect(link.pairing,p.pending,p.peer) {
        when {
            link.pairing=="Can't reach your PC" -> dialog=true
            link.pairing in listOf("That isn't a Pneu code","This code expired. Scan the new one.") || link.pairing.startsWith("Your PC's clock") -> snackbar.showSnackbar(link.pairing,duration=SnackbarDuration.Short)
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
    MaterialTheme(colorScheme=dark) {
        Box(Modifier.fillMaxSize().background(Color.Black)) {
            if(camera) {
                cameraPreview(Modifier.fillMaxSize()) { raw ->
                    if(!scanBlocked && (raw != lastCode || android.os.SystemClock.elapsedRealtime()-lastRead>3000)) {
                        lastCode=raw;lastRead=android.os.SystemClock.elapsedRealtime()
                        offerPair(raw)
                    }
                }
                Canvas(Modifier.fillMaxSize()) {
                    val cut=256.dp.toPx();val x=(size.width-cut)/2;val y=(size.height-cut)/2-32.dp.toPx()
                    val scrim=androidx.compose.ui.graphics.Path().apply {
                        fillType=androidx.compose.ui.graphics.PathFillType.EvenOdd
                        addRect(androidx.compose.ui.geometry.Rect(0f,0f,size.width,size.height))
                        addRoundRect(androidx.compose.ui.geometry.RoundRect(x,y,x+cut,y+cut,24.dp.toPx(),24.dp.toPx()))
                    }
                    drawPath(scrim,Color.Black.copy(alpha=.55f))
                    drawRoundRect(if(busy) dark.primary else Color.White,topLeft=androidx.compose.ui.geometry.Offset(x,y),size=androidx.compose.ui.geometry.Size(cut,cut),cornerRadius=androidx.compose.ui.geometry.CornerRadius(24.dp.toPx()),style=Stroke(if(busy)4.dp.toPx() else 2.dp.toPx()))
                }
                Column(Modifier.align(Alignment.Center).offset(y=(-8).dp),horizontalAlignment=Alignment.CenterHorizontally) {
                    Box(Modifier.size(256.dp),contentAlignment=Alignment.Center) {
                        if(busy) CircularProgressIndicator(Modifier.size(48.dp))
                        if(success) Surface(Modifier.size(64.dp),shape=RoundedCornerShape(32.dp),color=dark.primary) {Box(contentAlignment=Alignment.Center) {Symbol(R.drawable.ic_check,tint=dark.onPrimary)}}
                    }
                    Text(if(busy) "Pairing…" else if(success) "Paired with ${p.peer?.name.orEmpty()}" else "Scan the QR code on your PC",Modifier.padding(top=24.dp),color=Color.White,style=MaterialTheme.typography.bodyLarge,textAlign=TextAlign.Center)
                }
            } else Column(Modifier.align(Alignment.Center),horizontalAlignment=Alignment.CenterHorizontally,verticalArrangement=Arrangement.spacedBy(24.dp)) {
                Symbol(R.drawable.ic_photo_camera,modifier=Modifier.size(48.dp),tint=Color.White.copy(alpha=.7f))
                if(cameraAsked) Text("Camera access is off",color=Color.White)
                Button(onClick={
                    if(PermissionRequests.settings(context as Activity,Manifest.permission.CAMERA)) PermissionRequests.openSettings(context)
                    else {PermissionRequests.mark(context,Manifest.permission.CAMERA);permission.launch(Manifest.permission.CAMERA)}
                }) {Text(PermissionPolicy.cameraAction(cameraAsked,(context as Activity).shouldShowRequestPermissionRationale(Manifest.permission.CAMERA)))}
            }
            TopAppBar(title={Text("Pair with PC",color=Color.White)},navigationIcon={IconButton(onClick=back,enabled=!busy){Symbol(R.drawable.ic_arrow_back,"Back",tint=Color.White)}},colors=TopAppBarDefaults.topAppBarColors(containerColor=Color.Transparent))
            SnackbarHost(snackbar,Modifier.align(Alignment.BottomCenter).navigationBarsPadding())
            if(dialog) AlertDialog(onDismissRequest={},title={Text("Can't reach your PC")},text={Text("Make sure both are on the same Wi-Fi, or that Tailscale is connected.")},confirmButton={TextButton(onClick={val confirmed=ConfirmedBinding.from(p);val pending=p.pending;val code=lastAttemptedInvitation ?: lastCode.takeIf(String::isNotEmpty);dialog=false;feedbackTimedOut=false;scope.launch {if(code!=null)offerPair(code) else actions.run("Couldn't retry connection",{dialog=true}) {check(context.graph.retryConnection(confirmed,pending,feedback=pending==null)) {"Pairing changed during retry"}}}}) {Text("Try again")}},dismissButton={TextButton(onClick={val confirmed=ConfirmedBinding.from(p);val pending=p.pending;dialog=false;scope.launch {actions.run("Couldn't cancel pairing",{dialog=true}) {check(context.graph.cancelPairing(pending,confirmed)) {"Pairing changed during cancellation"};context.graph.connection.state.update {it.copy(pairing="")};context.graph.connection.retry();if(p.peer==null) context.stopService(Intent(context,PneuService::class.java))}}}){Text("Cancel")}})
        }
    }
}

@Composable private fun HomeBanner(text:String,action:String,click:()->Unit) {
    Surface(shape=RoundedCornerShape(16.dp),color=MaterialTheme.colorScheme.errorContainer,modifier=Modifier.fillMaxWidth().padding(bottom=16.dp)) {
        Row(Modifier.padding(start=16.dp,end=4.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(12.dp)) {
            Symbol(R.drawable.ic_warning,tint=MaterialTheme.colorScheme.onErrorContainer)
            Text(text,Modifier.weight(1f),style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onErrorContainer,maxLines=1,overflow=TextOverflow.Ellipsis)
            TextButton(click) {Text(action,color=MaterialTheme.colorScheme.onErrorContainer)}
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable private fun HomeScreen(p: Prefs,link: LinkState,recent: List<Outgoing>,health: Health,modifier: Modifier,permissions: () -> Unit,pair: () -> Unit) {
    val context=LocalContext.current; val app=context.graph; val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    var queueMenu by remember { mutableStateOf(false) }
    val waiting by app.waitingCount.collectAsState()
    val title=if(p.pairingLost) "Pairing lost" else if(p.paused) "Paused" else link.status
    val network=RoutePolicy.active(context.getSystemService(android.net.ConnectivityManager::class.java))
    val tailscaleOff=title=="PC unreachable" && p.tailscale && !network.vpn && p.peer?.addresses?.any {it.tailscale}==true
    val detail=when(title) {
        "Pairing lost" -> "${p.lostPcName.ifEmpty {p.peer?.name.orEmpty()}} doesn't recognize this phone"
        "Paused" -> "Automatic sending is off"
        "Connected" -> "${p.peer?.name.orEmpty()} · ${link.route}"
        "Connecting…" -> p.peer?.name.orEmpty()
        "Waiting for Wi-Fi" -> "Mobile data is off in settings"
        else -> if(tailscaleOff) "Tailscale isn't connected" else if(p.lastConnected>0) "Last connected ${lastConnectedLabel(context,p.lastConnected)}" else ""
    }
    Box(modifier.fillMaxSize(),contentAlignment=Alignment.TopCenter) {
        LazyColumn(Modifier.widthIn(max=600.dp).fillMaxWidth().padding(horizontal=16.dp),contentPadding=PaddingValues(bottom=24.dp)) {
            if(p.localCapacity) item {
                HomeBanner("No room for more waiting items","Don't send any") {
                    val selected=ConfirmedQueue.from(p,app.work.value)
                    scope.launch {actions.run("Couldn't clear queue") {check(app.discardAll(selected)) {"Queue changed"}}}
                }
            }
            else if(health.issues.isNotEmpty()) item {
                HomeBanner(if(health.issues.size==1) health.issues.single() else context.resources.getQuantityString(R.plurals.banner_many,health.issues.size,health.issues.size),"Fix",permissions)
            }
            item {
                Surface(shape=RoundedCornerShape(28.dp),color=groupColor(),modifier=Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(start=20.dp,end=20.dp,top=24.dp,bottom=20.dp),horizontalAlignment=Alignment.CenterHorizontally) {
                        Crossfade(title,animationSpec=androidx.compose.animation.core.tween(200),label="Connection status") { status ->
                            val c=MaterialTheme.colorScheme
                            val container=when(status) {"Connected"->c.primaryContainer;"Paused"->c.tertiaryContainer;"Pairing lost"->c.errorContainer;else->c.surfaceContainerHighest}
                            val ink=when(status) {"Connected"->c.onPrimaryContainer;"Paused"->c.onTertiaryContainer;"Pairing lost"->c.onErrorContainer;else->c.onSurfaceVariant}
                            Box(Modifier.size(80.dp),contentAlignment=Alignment.Center) {
                                Surface(Modifier.size(72.dp),shape=RoundedCornerShape(36.dp),color=container) {Box(contentAlignment=Alignment.Center) {Symbol(when(status) {"Paused"->R.drawable.ic_pause;"Pairing lost"->R.drawable.ic_error;"Waiting for Wi-Fi"->R.drawable.ic_wifi_off;"Connected","Connecting…"->R.drawable.ic_stat_pneu;else->R.drawable.ic_cloud_off},modifier=Modifier.size(36.dp),tint=ink)}}
                                if(status=="Connecting…") CircularProgressIndicator(Modifier.size(80.dp),strokeWidth=3.dp)
                            }
                        }
                        Text(title,Modifier.padding(top=12.dp),style=MaterialTheme.typography.headlineSmall)
                        if(detail.isNotEmpty()) Text(detail,Modifier.padding(top=4.dp),style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onSurfaceVariant,maxLines=2,overflow=TextOverflow.Ellipsis,textAlign=TextAlign.Center)
                        FlowRow(Modifier.fillMaxWidth().padding(top=20.dp),horizontalArrangement=Arrangement.spacedBy(8.dp),verticalArrangement=Arrangement.spacedBy(8.dp),maxItemsInEachRow=if(androidx.compose.ui.platform.LocalDensity.current.fontScale>1.3f)1 else 2) {
                            if(p.pairingLost) Button(pair,Modifier.fillMaxWidth().height(48.dp)) {Symbol(R.drawable.ic_qr_code_scanner);Spacer(Modifier.width(8.dp));Text("Pair again")}
                            else {
                                FilledTonalButton(onClick={if(tailscaleOff) context.packageManager.getLaunchIntentForPackage("com.tailscale.ipn")?.let { context.startActivity(it) } else context.startActivity(ManualEntries.clipboardIntent(context,p))},modifier=Modifier.weight(1f).height(48.dp),enabled=link.status=="Connected" || tailscaleOff,contentPadding=PaddingValues(horizontal=12.dp)) {Symbol(if(tailscaleOff)R.drawable.ic_open_in_new else R.drawable.ic_content_paste_go);Spacer(Modifier.width(8.dp));Text(if(tailscaleOff) "Open Tailscale" else "Send clipboard")}
                                if(p.paused) Button(onClick={scope.launch {actions.run("Couldn't resume") {app.preferences.toggle("paused",false)}}},modifier=Modifier.weight(1f).height(48.dp)) {Symbol(R.drawable.ic_play_arrow);Spacer(Modifier.width(8.dp));Text("Resume")}
                                else OutlinedButton(onClick={scope.launch {actions.run("Couldn't pause") {app.preferences.toggle("paused",true)}}},modifier=Modifier.weight(1f).height(48.dp)) {Symbol(R.drawable.ic_pause);Spacer(Modifier.width(8.dp));Text("Pause")}
                            }
                        }
                    }
                }
            }
            item {
                Row(Modifier.fillMaxWidth().padding(start=16.dp,top=16.dp,bottom=8.dp),verticalAlignment=Alignment.CenterVertically) {
                    Text("Recent",Modifier.weight(1f),style=MaterialTheme.typography.titleSmall,color=MaterialTheme.colorScheme.primary)
                    if(waiting>0) Box {
                        TextButton(onClick={queueMenu=true}) {Text(context.resources.getQuantityString(R.plurals.recent_waiting,waiting,waiting));Symbol(R.drawable.ic_arrow_drop_down)}
                        DropdownMenu(queueMenu,{queueMenu=false}) {
                            DropdownMenuItem(text={Text("Send now")},onClick={val confirmed=ConfirmedBinding.from(p);val pending=p.pending;queueMenu=false;scope.launch {actions.run("Couldn't retry connection") {check(app.retryConnection(confirmed,pending)) {"Pairing changed during retry"}}}})
                            DropdownMenuItem(text={Text("Don't send any")},onClick={val selected=ConfirmedQueue.from(p,app.work.value);queueMenu=false;scope.launch {actions.run("Couldn't clear queue") {check(app.discardAll(selected)) {"Queue changed"}}}})
                        }
                    }
                }
            }
            if(recent.isEmpty()) item {Group {Column(Modifier.fillMaxWidth().height(120.dp),horizontalAlignment=Alignment.CenterHorizontally,verticalArrangement=Arrangement.Center) {Symbol(R.drawable.ic_image,modifier=Modifier.size(32.dp),tint=MaterialTheme.colorScheme.onSurfaceVariant);Text("Nothing sent yet",Modifier.padding(top=8.dp),style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.onSurfaceVariant)}}}
            else items(recent,key={it.mediaId}) {row ->
                Surface(color=groupColor(),modifier=Modifier.animateItem().fillMaxWidth(),shape=RoundedCornerShape(topStart=if(row==recent.first())24.dp else 0.dp,topEnd=if(row==recent.first())24.dp else 0.dp,bottomStart=if(row==recent.last())24.dp else 0.dp,bottomEnd=if(row==recent.last())24.dp else 0.dp)) {
                    val progress=link.transferMediaId==row.mediaId
                    Row(Modifier.fillMaxWidth().heightIn(min=72.dp).clickable(enabled=row.state=="sent" && (if(row.kind=="text")row.text.matches(Regex("""https?://\S+""")) else row.uri.isNotEmpty())) {scope.launch {if(row.kind=="text")runCatching {context.startActivity(Intent(Intent.ACTION_VIEW,Uri.parse(row.text)))} else RecentItems.openOriginal(context,row)}}.padding(horizontal=16.dp,vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
                        MediaThumb(row)
                        Column(Modifier.weight(1f)) {
                            Text(when(row.kind) {"photo"->"Photo";"recording"->"Screen recording";"file"->row.fileName;"text"->row.text.lineSequence().firstOrNull().orEmpty().removePrefix("https://");"clipboard_image"->"Image";else->"Screenshot"},style=MaterialTheme.typography.bodyLarge,maxLines=1,overflow=if(row.kind=="file")TextOverflow.MiddleEllipsis else TextOverflow.Ellipsis)
                            val supporting=when {
                                progress -> "Sending · ${android.text.format.Formatter.formatShortFileSize(context,link.sent)} of ${android.text.format.Formatter.formatShortFileSize(context,link.total)}"
                                row.state=="sent" -> (if(row.kind=="clipboard_image" || row.kind=="text" && row.origin=="clipboard")"Clipboard · " else "")+lastConnectedLabel(context,row.createdAt)
                                row.state=="failed" -> UiPresentation.failedItem(row.error)
                                DeliveryPolicy.hold(row.kind,row.automatic,false,network.mobile,p.originals,row.attempted,row.optimized) -> "Waiting for Wi-Fi"
                                else -> "Waiting"
                            }
                            Text(supporting,style=MaterialTheme.typography.bodyMedium,color=if(row.state=="failed") MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,maxLines=1,overflow=TextOverflow.Ellipsis)
                        }
                        if(row.state=="failed") IconButton(onClick={val selected=ConfirmedQueue(ConfirmedBinding.from(p),listOf(ConfirmedItem.from(row)));scope.launch {actions.run("Couldn't retry item") {check(app.retry(row.mediaId,selected)) {"Queue item changed"}}}}) {Symbol(R.drawable.ic_refresh,"Retry")}
                        if(row.state!="sent") Box(contentAlignment=Alignment.Center) {
                            if(progress) CircularProgressIndicator(progress={if(link.total>0)(link.sent.toFloat()/link.total).coerceIn(0f,1f) else 0f},modifier=Modifier.size(40.dp),strokeWidth=2.dp)
                            IconButton(onClick={val selected=ConfirmedQueue(ConfirmedBinding.from(p),listOf(ConfirmedItem.from(row)));scope.launch {actions.run("Couldn't remove item") {check(app.discard(selected)) {"Queue item changed"}}}}) {Symbol(R.drawable.ic_close,if(progress)"Cancel" else if(row.state=="failed")"Remove" else "Don't send")}
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
    Surface(Modifier.size(48.dp),shape=RoundedCornerShape(12.dp),color=MaterialTheme.colorScheme.secondaryContainer) {
        val b=bitmap
        if(b!=null) androidx.compose.foundation.Image(b.asImageBitmap(),null,contentScale=androidx.compose.ui.layout.ContentScale.Crop)
        else Box(contentAlignment=Alignment.Center) {Symbol(when {row.kind=="text" && row.text.matches(Regex("""https?://\S+"""))->R.drawable.ic_link;row.kind=="text"->R.drawable.ic_content_paste;row.kind=="file"->R.drawable.ic_description;else->R.drawable.ic_image})}
        if(row.mime.startsWith("video/")) Box(Modifier.fillMaxSize().padding(4.dp),contentAlignment=Alignment.BottomStart) {
            Box(Modifier.size(20.dp).background(Color.Black.copy(alpha=.5f),CircleShape),contentAlignment=Alignment.Center) {Symbol(R.drawable.ic_play_arrow,modifier=Modifier.size(16.dp),tint=Color.White)}
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable private fun SettingsScreen(p: Prefs,health: Health,refresh:Int,back: () -> Unit,permissions: () -> Unit,unpair: () -> Unit) {
    val context=LocalContext.current;val app=context.graph;val scope=rememberCoroutineScope();val actions=LocalUiActions.current;val snackbar=remember {SnackbarHostState()}
    var pendingMedia by rememberSaveable {mutableStateOf("")}
    fun mediaPermissionFor(key:String)=if(key=="recordings")Manifest.permission.READ_MEDIA_VIDEO else Manifest.permission.READ_MEDIA_IMAGES
    LaunchedEffect(refresh,pendingMedia) {if(pendingMedia.isNotEmpty() && context.granted(mediaPermissionFor(pendingMedia))) {val key=pendingMedia;if(actions.run("Couldn't save setting") {app.preferences.toggle(key,true)})pendingMedia=""}}
    val mediaPermission=rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {result ->
        val key=pendingMedia
        if(key.isNotEmpty() && result[mediaPermissionFor(key)]==true) {scope.launch {if(actions.run("Couldn't save setting") {app.preferences.toggle(key,true)})pendingMedia=""}}
    }
    val tailscaleInstalled=remember {runCatching {context.packageManager.getPackageInfo("com.tailscale.ipn",0)}.isSuccess}
    fun toggle(key: String,value: Boolean) {
        if((key=="wifi" && !value && !p.tailscale)||(key=="tailscale" && !value && !p.wifi)) {scope.launch {snackbar.showSnackbar("Pneu needs Wi-Fi or Tailscale")};return}
        if(!value && pendingMedia==key)pendingMedia=""
        if(value && key in listOf("screenshots","photos","recordings") && !context.granted(mediaPermissionFor(key))) {
            pendingMedia=key
            if(PermissionRequests.settings(context as Activity,mediaPermissionFor(key)))PermissionRequests.openSettings(context)
            else {PermissionRequests.mark(context,mediaPermissionFor(key));mediaPermission.launch(arrayOf(mediaPermissionFor(key)))}
        } else scope.launch {actions.run("Couldn't save setting",{toggle(key,value)}) {try {app.preferences.toggle(key,value)}catch(_:IllegalArgumentException) {snackbar.showSnackbar("Pneu needs Wi-Fi or Tailscale")}}}
    }
    Scaffold(containerColor=Color.Transparent,topBar={LargeTopAppBar(title={Text("Settings")},navigationIcon={NavIcon(back)},colors=TopAppBarDefaults.topAppBarColors(containerColor=Color.Transparent))},snackbarHost={SnackbarHost(snackbar)}) {padding ->
        Box(Modifier.padding(padding).fillMaxSize(),contentAlignment=Alignment.TopCenter) {
            Column(Modifier.widthIn(max=600.dp).fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal=16.dp)) {
                Header("Send automatically")
                Group {
                    ToggleRow("Screenshots",p.screenshots) {toggle("screenshots",it)}
                    ToggleRow("Camera photos",p.photos) {toggle("photos",it)}
                    ToggleRow("Screen recordings",p.recordings) {toggle("recordings",it)}
                }
                Header("Connection")
                Group {
                    ToggleRow("Direct on home Wi-Fi",p.wifi) {toggle("wifi",it)}
                    ToggleRow("Tailscale when away",p.tailscale,detail=if(!tailscaleInstalled)"Tailscale app isn't installed" else null) {toggle("tailscale",it)}
                    ToggleRow("Use mobile data",p.mobile) {toggle("mobile",it)}
                    ToggleRow("Send originals on mobile data",p.originals,enabled=p.mobile) {toggle("originals",it)}
                    ToggleRow("Queue while PC is unreachable",p.queue) {toggle("queue",it)}
                }
                Header("Clipboard")
                Group {
                    ToggleRow("Send button in notification",p.clipboardButton) {toggle("clipboardButton",it)}
                    ToggleRow("Receive clipboard from PC",p.clipboardFromPc) {toggle("clipboardFromPc",it)}
                }
                Header("Paired PC")
                Group {
                    SettingRow(if(p.pairingLost)"Pairing lost" else p.peer?.name ?: "Not paired",if(p.pairingLost)p.lostPcName else p.peer?.addresses?.let {UiPresentation.pairedAddresses(it).joinToString(" · ").ifEmpty {null}})
                    SettingRow("Unpair",error=true,click=unpair)
                }
                Header("This phone")
                Group {
                    ToggleRow("Show in share sheet",p.shareSheet) {toggle("shareSheet",it)}
                    ToggleRow("Quick Settings tiles",p.quickSettings) {toggle("quickSettings",it)}
                    SettingRow("Permissions and battery",if(health.issues.isEmpty())"All set" else context.resources.getQuantityString(R.plurals.pref_permissions_issues,health.issues.size,health.issues.size),error=health.issues.isNotEmpty(),click=permissions)}
                Text("Version ${BuildConfig.VERSION_NAME}",Modifier.fillMaxWidth().padding(24.dp),style=MaterialTheme.typography.bodySmall,color=MaterialTheme.colorScheme.onSurfaceVariant,textAlign=TextAlign.Center)
            }
        }
    }
}
@Composable private fun ToggleRow(title: String,checked: Boolean,enabled: Boolean=true,detail: String?=null,change: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().clickable(enabled=enabled){change(!checked)}.heightIn(min=if(detail==null)56.dp else 72.dp).padding(horizontal=16.dp,vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
        Column(Modifier.weight(1f)) {Text(title,style=MaterialTheme.typography.bodyLarge,color=MaterialTheme.colorScheme.onSurface.copy(alpha=if(enabled)1f else .38f));if(detail!=null) Text(detail,style=MaterialTheme.typography.bodyMedium,color=MaterialTheme.colorScheme.error)}
        Switch(checked=checked,onCheckedChange=change,enabled=enabled,thumbContent=if(checked)({Symbol(R.drawable.ic_check,modifier=Modifier.size(16.dp))})else null)
    }
}
@Composable private fun SettingRow(title: String,detail: String?=null,error: Boolean=false,click: (() -> Unit)?=null) {
    Column(Modifier.fillMaxWidth().then(if(click!=null)Modifier.clickable(onClick=click)else Modifier).heightIn(min=if(detail==null)56.dp else 72.dp).padding(horizontal=16.dp,vertical=12.dp),verticalArrangement=Arrangement.Center) {
        Text(title,style=MaterialTheme.typography.bodyLarge,color=if(error&&detail==null)MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurface,maxLines=1,overflow=TextOverflow.Ellipsis)
        if(detail!=null) Text(detail,style=MaterialTheme.typography.bodyMedium,color=if(error)MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,maxLines=1,overflow=TextOverflow.Ellipsis)
    }
}
