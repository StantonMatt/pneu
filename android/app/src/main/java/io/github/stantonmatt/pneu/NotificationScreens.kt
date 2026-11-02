package io.github.stantonmatt.pneu

import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.drawable.Drawable
import android.net.Uri
import android.os.Process
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.*
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.data.historySearchKey
import io.github.stantonmatt.pneu.data.Prefs
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.CAP_NOTIFICATIONS
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*

@Composable internal fun NotificationSettingsRows(p:Prefs,refresh:Int,open:()->Unit) {
    val context=LocalContext.current
    val access=remember(refresh) {context.notificationAccess()}
    val consent=p.notificationConsent
    val detail=when {
        !consent.enabled->stringResource(R.string.notification_off)
        !access->stringResource(R.string.notification_needs_access)
        consent.apps.isEmpty()->stringResource(R.string.notification_no_apps)
        else->context.resources.getQuantityString(R.plurals.notification_app_count,consent.apps.size,consent.apps.size)
    }
    SettingRow(stringResource(R.string.notification_show),detail,error=consent.enabled && !access,divider=false,click=open)
}

internal fun restrictedNotificationSource(source:Int)=source==android.content.pm.PackageInstaller.PACKAGE_SOURCE_LOCAL_FILE ||
    source==android.content.pm.PackageInstaller.PACKAGE_SOURCE_DOWNLOADED_FILE

internal data class NotificationPickerApp(val key:String,val label:String,val icon:Drawable?=null)
internal fun notificationAppMatches(app:NotificationPickerApp,query:String):Boolean {
    val key=historySearchKey(query)
    return key.isEmpty() || historySearchKey(app.label).contains(key) || historySearchKey(app.key.substringAfter('/')).contains(key)
}
private fun android.content.Context.notificationPickerApps():List<NotificationPickerApp> =packageManager.queryIntentActivities(
    Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER),PackageManager.ResolveInfoFlags.of(0))
    .filter {it.activityInfo.packageName!=packageName}.distinctBy {it.activityInfo.packageName}
    .map {NotificationPickerApp(NotificationConsent.app(Process.myUserHandle().hashCode(),it.activityInfo.packageName),it.loadLabel(packageManager).toString(),it.loadIcon(packageManager))}
    .sortedWith(compareBy(String.CASE_INSENSITIVE_ORDER) {it.label})

@Composable internal fun NotificationsScreen(p:Prefs,back:()->Unit) {
    val context=LocalContext.current;val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    var access by remember {mutableStateOf(context.notificationAccess())}
    var dialog by rememberSaveable {mutableStateOf(false)}
    var awaiting by rememberSaveable {mutableStateOf(false)}
    var blocked by rememberSaveable {mutableStateOf(false)}
    var apps by remember {mutableStateOf(emptyList<NotificationPickerApp>())}
    LaunchedEffect(Unit) {apps=withContext(Dispatchers.IO) {context.notificationPickerApps()}}
    val settingError=stringResource(R.string.ui_error_setting)
    fun change(body:suspend ()->Unit) {scope.launch {actions.run(settingError) {body()}}}
    val launcher=rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) {
        access=context.notificationAccess()
        if(awaiting) {
            awaiting=false
            if(access) {blocked=false;change {context.graph.preferences.notificationMaster(true)}}
            else blocked=runCatching {restrictedNotificationSource(context.packageManager.getInstallSourceInfo(context.packageName).packageSource)}.getOrDefault(false)
        }
    }
    val lifecycle by androidx.lifecycle.compose.LocalLifecycleOwner.current.lifecycle.currentStateFlow.collectAsState()
    LaunchedEffect(lifecycle) {access=context.notificationAccess();if(access)blocked=false}
    NotificationsContent(p,access,blocked,apps,back,
        toggle={enabled->if(!enabled || access)change {context.graph.preferences.notificationMaster(enabled)} else dialog=true},
        names={enabled->change {context.graph.preferences.notificationNamesOnly(enabled)}},
        select={computer,key,enabled->change {context.graph.preferences.notificationSelection(computer,key,enabled)}},
        appInfo={launcher.launch(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,Uri.parse("package:${context.packageName}")))})
    if(dialog)NotificationAccessDialog({dialog=false}) {
        dialog=false;awaiting=true;launcher.launch(Intent(Settings.ACTION_NOTIFICATION_LISTENER_DETAIL_SETTINGS)
            .putExtra(Settings.EXTRA_NOTIFICATION_LISTENER_COMPONENT_NAME,ComponentName(context,PhoneNotificationListener::class.java).flattenToString()))
    }
}

@Composable internal fun NotificationAccessDialog(cancel:()->Unit,proceed:()->Unit) {
    PneuDialog(cancel,title={Text(stringResource(R.string.notification_allow_title))},
        text={Text(stringResource(R.string.notification_allow_text))},
        dismissButton={PneuTextButton(onClick=cancel) {Text(stringResource(R.string.cd_cancel))}},
        confirmButton={PneuTextButton(onClick=proceed) {Text(stringResource(R.string.notification_continue))}})

}

@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun NotificationsContent(p:Prefs,access:Boolean,blocked:Boolean,apps:List<NotificationPickerApp>,back:()->Unit,
    toggle:(Boolean)->Unit,names:(Boolean)->Unit,select:(Boolean,String,Boolean)->Unit,appInfo:()->Unit) {
    val context=LocalContext.current;val consent=p.notificationConsent;val on=consent.enabled && access
    var query by rememberSaveable {mutableStateOf("")}
    val visibleApps=remember(apps,query) {apps.filter {notificationAppMatches(it,query)}}
    Scaffold(containerColor=Color.Transparent,contentWindowInsets=WindowInsets.statusBars,topBar={LargeTopAppBar(
        title={Text(stringResource(R.string.notification_title),style=PneuType.largeTitle,maxLines=2)},navigationIcon={NavIcon(back)},
        colors=TopAppBarDefaults.topAppBarColors(containerColor=LocalPneuColors.current.bg))}) {padding->
        LazyColumn(Modifier.padding(padding).fillMaxSize().navigationBarsPadding().imePadding(),contentPadding=PaddingValues(horizontal=16.dp,vertical=8.dp)) {
            item {PneuCard {
                ToggleRow(stringResource(R.string.notification_show_on),on,divider=false,change=toggle)
                if(on)ToggleRow(stringResource(R.string.notification_names_only),consent.namesOnly,change=names)
            }}
            if(blocked && !access)item {Spacer(Modifier.height(12.dp));NotificationRestrictedCard(appInfo)}
            if(on) {
                item {Header(stringResource(R.string.notification_computers))}
                item {PneuCard {p.devices.filter {it.role=="computer"}.forEachIndexed {i,peer->
                    val links by context.graph.connection.devices.collectAsState()
                    val supported=links[peer.deviceId]?.takeIf {it.status=="Connected"}?.capabilities?.contains(CAP_NOTIFICATIONS)
                    NotificationChoiceRow(peer.name,NotificationConsent.computer(peer) in consent.computers,null,R.drawable.ic_computer,
                        if(supported==false)stringResource(R.string.notification_update) else null,i>0) {select(true,NotificationConsent.computer(peer),it)}
                }}}
                item {Header(stringResource(R.string.notification_apps))}
                item {PneuSearch(query) {query=it};Spacer(Modifier.height(12.dp))}
                item {if(visibleApps.isEmpty() && historySearchKey(query).isNotEmpty())PneuSearchNoMatches()
                    else PneuCard {visibleApps.forEachIndexed {i,app->NotificationChoiceRow(app.label,app.key in consent.apps,app.icon,null,null,i>0) {select(false,app.key,it)}}}}
            }
        }
    }
}

@Composable internal fun NotificationRestrictedCard(open:()->Unit) {
    PneuCard {Row(Modifier.padding(16.dp),horizontalArrangement=Arrangement.spacedBy(12.dp)) {
        Box(Modifier.size(36.dp).background(LocalPneuColors.current.warnSoft,RoundedCornerShape(10.dp)),contentAlignment=Alignment.Center) {Symbol(R.drawable.ic_warning,tint=LocalPneuColors.current.warn)}
        Column(verticalArrangement=Arrangement.spacedBy(8.dp)) {
            Text(stringResource(R.string.notification_access_off),style=PneuType.headline)
            Text(stringResource(R.string.notification_restricted_hint),style=PneuType.subhead,color=LocalPneuColors.current.text2)
            PneuButton(open,kind=ButtonKind.SmallTonal) {Text(stringResource(R.string.notification_app_info))}
        }
    }}
}

/** Rasterize launcher drawables into the same tile, including adaptive/transparent icons. */
internal fun notificationIconBitmap(icon:Drawable,pixels:Int,fill:Int):Bitmap {
    val bitmap=Bitmap.createBitmap(pixels,pixels,Bitmap.Config.ARGB_8888)
    val canvas=Canvas(bitmap);val scale=pixels/36f
    val outline=android.graphics.Path().apply {addRoundRect(0f,0f,pixels.toFloat(),pixels.toFloat(),10*scale,10*scale,android.graphics.Path.Direction.CW)}
    canvas.clipPath(outline);canvas.drawColor(fill)
    val inset=kotlin.math.round(4*scale).toInt()
    val original=android.graphics.Rect(icon.bounds)
    try {icon.setBounds(inset,inset,pixels-inset,pixels-inset);icon.draw(canvas)} finally {icon.bounds=original}
    return bitmap
}

@Composable private fun NotificationChoiceRow(label:String,checked:Boolean,icon:Drawable?,symbol:Int?,detail:String?,divider:Boolean,change:(Boolean)->Unit) {
    Row(Modifier.fillMaxWidth().pneuRow(divider,64.dp).pneuToggle(checked,true,onChange=change).heightIn(min=64.dp).padding(horizontal=16.dp,vertical=8.dp),
        verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(12.dp)) {
        if(icon!=null) {
            val fill=LocalPneuColors.current.fill.toArgb()
            val bitmap=remember(icon,fill) {notificationIconBitmap(icon,128,fill).asImageBitmap()}
            Image(bitmap,null,Modifier.size(36.dp))
        } else Box(Modifier.size(36.dp).background(if(checked)LocalPneuColors.current.accentSoft else LocalPneuColors.current.fill,RoundedCornerShape(10.dp)),contentAlignment=Alignment.Center) {Symbol(symbol ?: R.drawable.ic_notifications,modifier=Modifier.size(20.dp),tint=if(checked)LocalPneuColors.current.accentText else LocalPneuColors.current.text2)}
        Column(Modifier.weight(1f)) {Text(label,style=PneuType.body);if(detail!=null)Text(detail,style=PneuType.subhead,color=LocalPneuColors.current.text2)}
        PneuSwitch(checked,onCheckedChange=null)
    }
}
