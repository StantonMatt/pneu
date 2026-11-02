package io.github.stantonmatt.pneu

import io.github.stantonmatt.pneu.connection.ConnectionStatus
import android.provider.Settings
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.*
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.semantics.*
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.connection.LinkState
import io.github.stantonmatt.pneu.data.*
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.protocol.utf8Prefix
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.withContext
import kotlinx.coroutines.launch

/** A device's status line and tile colour, shared by Home, its page and the picker. */
internal data class DeviceLine(val text:String,val tone:LinkTone,val color:Color)
@Composable internal fun deviceLine(p:Prefs,state:DeviceState):DeviceLine {
    val context=LocalContext.current;val resources=LocalResources.current;val c=LocalPneuColors.current;val link=state.connection
    return when {
        state.peer.pairingLost->DeviceLine(resources.getString(R.string.status_pairing_lost),LinkTone.Danger,c.danger)
        state.peer.setupRefused->DeviceLine(resources.getString(R.string.setup_refused,state.peer.name),LinkTone.Warn,c.warn)
        link.connectionStatus==ConnectionStatus.FINGERPRINT_MISMATCH->DeviceLine(resources.getString(R.string.status_fingerprint_mismatch),LinkTone.Danger,c.danger)
        link.connectionStatus==ConnectionStatus.UPDATE_REQUIRED->DeviceLine(resources.getString(R.string.device_update_required),LinkTone.Warn,c.warn)
        connected(p,state)->DeviceLine(if(link.route.isEmpty())resources.getString(R.string.status_connected) else resources.getString(R.string.device_connected,UiPresentation.status(context,link.route)),LinkTone.On,c.text2)
        link.connectionStatus==ConnectionStatus.CONNECTING->DeviceLine(resources.getString(R.string.status_connecting),LinkTone.Neutral,c.text2)
        link.connectionStatus==ConnectionStatus.WAITING_WIFI || link.connectionStatus==ConnectionStatus.NO_NETWORK->DeviceLine(UiPresentation.status(context,link.connectionStatus),LinkTone.Neutral,c.text2)
        // Added via another device and not reached yet: it keeps trying, never an error.
        SetupPresentation.introducedStatus(state.peer)==IntroducedStatus.SILENT->DeviceLine(resources.getString(R.string.device_not_connected_yet),LinkTone.Neutral,c.text2)
        else->DeviceLine(lastSeen(context,state.lastSeen),LinkTone.Neutral,c.text2)
    }
}
/** A reset binding never counts as connected, as in the aggregate Link. */
internal fun connected(p:Prefs,state:DeviceState)=state.connection.connectionStatus==ConnectionStatus.CONNECTED && !state.peer.pairingLost && state.peer.deviceId !in p.resetPeers
private fun lastSeen(context:android.content.Context,timestamp:Long):String {
    if(timestamp==0L)return context.getString(R.string.device_not_connected)
    val label=lastConnectedLabel(context,timestamp)
    return if(label==context.getString(R.string.time_yesterday))context.getString(R.string.device_last_seen_yesterday) else context.getString(R.string.device_last_seen,label)
}
/** Stack tiles follow the Devices card order. Pause greys every computer, as the tube does. */
@Composable internal fun linkTones(p:Prefs,devices:List<DeviceState>)=devices.map {state->
    when {state.peer.pairingLost->LinkTone.Danger;p.paused->LinkTone.Neutral;else->deviceLine(p,state).tone}
}

@Composable private fun DeviceTile(tone:LinkTone,ghost:Boolean=false,icon:Int=R.drawable.ic_computer) {
    val c=LocalPneuColors.current
    val (fill,ink)=when(tone) {LinkTone.On->c.accentSoft to c.onAccentSoft;LinkTone.Warn->c.warnSoft to c.warn;LinkTone.Danger->c.dangerSoft to c.danger;else->c.fill to c.text2}
    if(ghost)Box(Modifier.size(36.dp).border(1.5.dp,c.text3,RoundedCornerShape(11.dp)),contentAlignment=Alignment.Center) {Symbol(R.drawable.ic_add,modifier=Modifier.size(20.dp),tint=c.accentText)}
    else Box(Modifier.size(36.dp).background(fill,RoundedCornerShape(11.dp)),contentAlignment=Alignment.Center) {Symbol(icon,modifier=Modifier.size(20.dp),tint=ink)}
}

/** Home → Devices: one row per device, other phones in your devices (grey), then Add device. */
@Composable internal fun DevicesCard(p:Prefs,devices:List<DeviceState>,open:(String)->Unit,add:()->Unit,phones:List<PhoneMember> = emptyList(),openPhone:(String)->Unit={}) {
    val c=LocalPneuColors.current;val resources=LocalResources.current
    Text(resources.getString(R.string.group_devices),Modifier.padding(start=16.dp,top=20.dp,bottom=8.dp),style=PneuType.section,color=c.text2)
    PneuCard {
        devices.forEachIndexed {i,state->
            val line=deviceLine(p,state)
            Row(Modifier.fillMaxWidth().pneuRow(i>0,66.dp).pneuClick {open(state.peer.deviceId)}.heightIn(min=68.dp).padding(horizontal=16.dp,vertical=10.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(14.dp)) {
                DeviceTile(line.tone)
                Column(Modifier.weight(1f)) {
                    Text(state.peer.name,style=PneuType.body,maxLines=1,overflow=TextOverflow.Ellipsis)
                    Text(line.text,style=PneuType.subhead,color=line.color,maxLines=if(state.peer.setupRefused)Int.MAX_VALUE else 1,overflow=TextOverflow.Ellipsis)
                }
                Symbol(R.drawable.ic_chevron_right,modifier=Modifier.size(20.dp),tint=c.text3)
            }
        }
        phones.forEachIndexed {i,phone->
            Row(Modifier.fillMaxWidth().pneuRow(devices.isNotEmpty() || i>0,66.dp).pneuClick {openPhone(phone.key)}.heightIn(min=68.dp).padding(horizontal=16.dp,vertical=10.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(14.dp)) {
                DeviceTile(LinkTone.Neutral,icon=R.drawable.ic_smartphone)
                Column(Modifier.weight(1f)) {
                    Text(phone.name,style=PneuType.body,maxLines=1,overflow=TextOverflow.Ellipsis)
                    Text(resources.getString(R.string.setup_phones_dont_connect),style=PneuType.subhead,color=c.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
                }
                Symbol(R.drawable.ic_chevron_right,modifier=Modifier.size(20.dp),tint=c.text3)
            }
        }
        Row(Modifier.fillMaxWidth().pneuRow(devices.isNotEmpty() || phones.isNotEmpty(),66.dp).pneuClick(onClick=add).heightIn(min=56.dp).padding(horizontal=16.dp,vertical=10.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(14.dp)) {
            DeviceTile(LinkTone.Neutral,ghost=true)
            Text(resources.getString(R.string.action_add_device),Modifier.weight(1f),style=PneuType.body.copy(fontWeight=androidx.compose.ui.text.font.FontWeight.Medium),color=c.accentText)
        }
    }
}

/** The kinds Automatic sending covers, from this phone's Settings. */
private fun automaticKinds(context:android.content.Context,p:Prefs):String? {
    val kinds=listOfNotNull(R.string.auto_kind_screenshots.takeIf {p.screenshots},R.string.auto_kind_photos.takeIf {p.photos},R.string.auto_kind_recordings.takeIf {p.recordings}).map(context::getString)
    val text=when(kinds.size) {0->return null;1->kinds[0];2->context.getString(R.string.list_two,kinds[0],kinds[1]);else->context.getString(R.string.list_three,kinds[0],kinds[1],kinds[2])}
    return text.replaceFirstChar {it.uppercase()}
}

/** One pair: status card, the pair's sharing switches, then linking and Remove from all devices or Unpair. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun DeviceScreen(p:Prefs,state:DeviceState,back:()->Unit,pair:()->Unit,unpair:()->Unit,sendFiles:()->Unit,setup:SetupActions=SetupActions(),sendFolder:()->Unit={},setSwitch:(String,Boolean)->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val c=LocalPneuColors.current
    val peer=state.peer;val link=state.connection;val online=connected(p,state)
    val scroll=rememberScrollState()
    var titleHeight by remember {mutableIntStateOf(0)}
    val collapsed by remember {derivedStateOf {titleHeight>0 && scroll.value>=titleHeight}}
    Scaffold(containerColor=Color.Transparent,contentWindowInsets=WindowInsets.statusBars,topBar={
        TopAppBar(modifier=Modifier.drawWithSeparator(collapsed),title={if(collapsed)Text(peer.name,Modifier.padding(start=4.dp),style=PneuType.title2,maxLines=1,overflow=TextOverflow.Ellipsis)},
            navigationIcon={IconButton(onClick=back) {Symbol(R.drawable.ic_arrow_back,resources.getString(R.string.cd_back))}},
            colors=TopAppBarDefaults.topAppBarColors(containerColor=c.bg,scrolledContainerColor=c.bg))
    }) {padding->
        Box(Modifier.padding(padding).fillMaxSize(),contentAlignment=Alignment.TopCenter) {
            Column(Modifier.widthIn(max=600.dp).fillMaxWidth().verticalScroll(scroll).navigationBarsPadding().padding(horizontal=16.dp)) {
                Text(peer.name,Modifier.onSizeChanged {titleHeight=it.height}.padding(start=4.dp,end=4.dp,top=6.dp,bottom=14.dp).semantics {heading()},style=PneuType.largeTitle,maxLines=2,overflow=TextOverflow.Ellipsis)
                val status=when {peer.pairingLost->ConnectionStatus.PAIRING_LOST;online->ConnectionStatus.CONNECTED;else->link.connectionStatus}
                val title=if(peer.setupRefused)resources.getString(R.string.setup_refused,peer.name) else if(peer.addedViaId!=null && !peer.established && status==ConnectionStatus.UNREACHABLE)resources.getString(R.string.device_not_connected_yet) else when(status) {ConnectionStatus.UNREACHABLE->resources.getString(R.string.device_not_connected);ConnectionStatus.UPDATE_REQUIRED->resources.getString(R.string.device_update_required);else->UiPresentation.status(context,status)}
                val address=UiPresentation.pairedAddress(peer.addresses,link.route)
                val hint=unreachableHint(status==ConnectionStatus.UNREACHABLE,listOf(peer),p.tailscale)
                var anywhere by rememberSaveable {mutableStateOf(false)}
                val detail=if(peer.setupRefused)resources.getString(R.string.setup_refused_hint,peer.name) else if(SetupPresentation.introducedStatus(peer)==IntroducedStatus.SILENT && status==ConnectionStatus.UNREACHABLE)
                    resources.getString(R.string.setup_silent_hint,peer.name) else when(status) {
                    ConnectionStatus.PAIRING_LOST->resources.getString(R.string.detail_pairing_lost,peer.name)
                    ConnectionStatus.UPDATE_REQUIRED->resources.getString(R.string.device_update_detail,peer.name)
                    ConnectionStatus.CONNECTED->listOfNotNull(link.route.takeIf(String::isNotEmpty)?.let {UiPresentation.status(context,it)},address).joinToString(" · ")
                    ConnectionStatus.WAITING_WIFI->resources.getString(R.string.detail_mobile_data_off)
                    else->hint?.let {unreachableDetail(context,it,listOf(peer))} ?: if(state.lastSeen>0)resources.getString(R.string.detail_last_connected,lastConnectedLabel(context,state.lastSeen)) else ""
                }
                Surface(shape=RoundedCornerShape(24.dp),border=if(c.dark)BorderStroke(1.dp,c.hairline)else null,color=c.surface,modifier=Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(start=20.dp,end=20.dp,top=22.dp,bottom=20.dp),horizontalAlignment=Alignment.CenterHorizontally) {
                        LinkView(link.copy(status=status.key),sending=online && link.transferMediaId.isNotEmpty(),receiving=online && link.incoming!=null)
                        Column(Modifier.fillMaxWidth().semantics(mergeDescendants=true) {},horizontalAlignment=Alignment.CenterHorizontally) {
                            Text(title,Modifier.fillMaxWidth().padding(top=14.dp),style=PneuType.title2,textAlign=TextAlign.Center)
                            if(detail.isNotEmpty())Text(detail,Modifier.padding(top=4.dp),style=PneuType.subhead,color=c.text2,maxLines=2,overflow=TextOverflow.Ellipsis,textAlign=TextAlign.Center)
                        }
                        Spacer(Modifier.height(20.dp))
                        if(peer.pairingLost || peer.setupRefused || SetupPresentation.introducedStatus(peer)==IntroducedStatus.SILENT && status==ConnectionStatus.UNREACHABLE)PneuButton(pair,Modifier.fillMaxWidth().height(48.dp)) {Symbol(R.drawable.ic_qr_code_scanner,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_pair_again))}
                        // Unreachable: the Link card's action first, then Send files (files wait for the device).
                        else {
                            if(hint?.hasAction()==true)UnreachableAction(hint,Modifier.fillMaxWidth().padding(bottom=8.dp).height(48.dp)) {anywhere=true}
                            PneuButtonPair(Modifier.fillMaxWidth()) {
                                PneuTonalButton(sendFiles,Modifier.height(48.dp)) {Symbol(R.drawable.ic_upload,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_send_files),maxLines=1,overflow=TextOverflow.Ellipsis)}
                                PneuTonalButton(sendFolder,Modifier.height(48.dp)) {Symbol(R.drawable.ic_folder,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_send_folder),maxLines=1,overflow=TextOverflow.Ellipsis)}
                            }
                        }
                        if(anywhere)ConnectAnywhereSheet(p.tailscale,listOf(peer)) {anywhere=false}
                    }
                }
                if(!peer.pairingLost) {
                    Text(resources.getString(R.string.group_sharing),Modifier.padding(start=16.dp,top=22.dp,bottom=8.dp),style=PneuType.section,color=c.text2)
                    val pending=resources.getString(R.string.switch_pending,peer.name)
                    fun note(key:String)=if(!online && key in state.pendingSwitches && state.settingsSupported!=false)pending else null
                    val kinds=automaticKinds(context,p)
                    PneuCard {
                        SwitchRow(resources.getString(R.string.pref_send_clipboard),peer.clipboardSend,note("sendClipboard"),divider=false) {setSwitch("sendClipboard",it)}
                        SwitchRow(resources.getString(R.string.pref_receive_clipboard),peer.clipboardReceive,note("receiveClipboard")) {setSwitch("receiveClipboard",it)}
                        if(kinds==null)SwitchRow(resources.getString(R.string.pref_automatic_sending),false,resources.getString(R.string.auto_off_all),enabled=false) {}
                        else SwitchRow(resources.getString(R.string.pref_automatic_sending),peer.automaticMedia,note("automaticMedia") ?: kinds) {setSwitch("automaticMedia",it)}
                    }
                    if(peer.helloSeen && state.settingsSupported==false)Text(resources.getString(R.string.sharing_needs_update,peer.name),Modifier.padding(start=16.dp,end=16.dp,top=8.dp),style=PneuType.subhead,color=c.text2)
                }
                Spacer(Modifier.height(12.dp))
                SetupPageCard(peer.name,SetupPresentation.page(p,peer),setup,unpair)
                Spacer(Modifier.height(24.dp))
            }
        }
    }
}
@Composable private fun Modifier.drawWithSeparator(show:Boolean):Modifier {
    val c=LocalPneuColors.current
    return drawWithContent {drawContent();if(show)drawLine(c.separator,Offset(0f,size.height-1f),Offset(size.width,size.height-1f),1f)}
}

/** A switch row whose optional second line is information, not an error. */
@Composable internal fun SwitchRow(title:String,checked:Boolean,detail:String?=null,enabled:Boolean=true,divider:Boolean=true,change:(Boolean)->Unit) {
    val c=LocalPneuColors.current
    Row(Modifier.fillMaxWidth().pneuRow(divider).pneuToggle(checked,enabled,onChange=change).heightIn(min=if(detail==null)56.dp else 68.dp).padding(horizontal=16.dp,vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
        Column(Modifier.weight(1f).alpha(if(enabled)1f else .4f)) {
            Text(title,style=PneuType.body)
            if(detail!=null)Text(detail,style=PneuType.subhead,color=c.text2)
        }
        PneuSwitch(checked=checked,onCheckedChange=null,enabled=enabled)
    }
}

/** Tap a sending, waiting or failed Recent row: one line per device. Close leaves everything running. */
@Composable internal fun RecipientsSheet(title:String,item:RecentItem,devices:Map<String,Boolean>,close:()->Unit,stop:(RecipientOutcome)->Unit,retry:(RecipientOutcome)->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val c=LocalPneuColors.current
    PneuDialog(onDismissRequest=close,title={Text(title,maxLines=2,overflow=TextOverflow.Ellipsis)},
        confirmButton={PneuTextButton(onClick=close) {Text(resources.getString(R.string.action_close))}},
        text={Column(Modifier.padding(top=2.dp)) {
            item.recipients.forEachIndexed {i,recipient->
                val online=devices[recipient.peerId]==true
                val sending=recipient.state=="sending" && recipient.active
                val connectionLost=UiPresentation.connectionLostFailure(context,recipient.state,recipient.error,recipient.resumable,recipient.sent,recipient.total)
                val (line,color)=when {
                    recipient.error==ReinstallReplacement.CANCELLED->resources.getString(R.string.replacement_recipient_not_sent) to c.warn
                    recipient.state=="sent"->resources.getString(R.string.delivery_sent,lastConnectedLabel(context,item.row.createdAt)) to c.text2
                    recipient.state in setOf("discarded","superseded","skipped")->resources.getString(when(recipient.state) {
                        "discarded"->R.string.delivery_stopped;"superseded"->R.string.delivery_superseded;else->R.string.delivery_skipped
                    }) to c.text2
                    sending->resources.getString(R.string.item_sending,android.text.format.Formatter.formatShortFileSize(context,recipient.sent),android.text.format.Formatter.formatShortFileSize(context,recipient.total)) to c.accentText
                    connectionLost!=null->connectionLost to c.danger
                    recipient.state=="failed"->(UiPresentation.failureReason(recipient.error)?.let {resources.getString(R.string.delivery_failed,resources.getString(it))} ?: resources.getString(R.string.item_failed)) to c.danger
                    !online->resources.getString(R.string.delivery_waiting_offline) to c.text2
                    else->resources.getString(R.string.item_waiting) to c.text2
                }
                Row(Modifier.fillMaxWidth().pneuRow(i>0,50.dp).heightIn(min=60.dp).padding(vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(14.dp)) {
                    DeviceTile(if(online)LinkTone.On else LinkTone.Neutral)
                    Column(Modifier.weight(1f)) {
                        Text(recipient.name.ifEmpty {resources.getString(R.string.computer)},style=PneuType.body,color=c.text,maxLines=1,overflow=TextOverflow.Ellipsis)
                        Text(line,style=PneuType.subhead,color=color,maxLines=1,overflow=TextOverflow.Ellipsis)
                        if(sending)LinearProgressIndicator(progress={if(recipient.total>0)(recipient.sent.toFloat()/recipient.total).coerceIn(0f,1f) else 0f},modifier=Modifier.fillMaxWidth().padding(top=7.dp).height(3.dp).clip(RoundedCornerShape(2.dp)),color=c.accent,trackColor=c.fill2,gapSize=0.dp,drawStopIndicator={})
                    }
                    if(recipient.error==ReinstallReplacement.CANCELLED && recipient.item!=null)IconButton(modifier=Modifier.size(40.dp),onClick={stop(recipient)}) {Symbol(R.drawable.ic_close,resources.getString(R.string.cd_remove),modifier=Modifier.size(22.dp),tint=c.text2)}
                    when {
                        recipient.state=="sent"->Box(Modifier.size(40.dp),contentAlignment=Alignment.Center) {Symbol(R.drawable.ic_check_circle,tint=c.accent)}
                        recipient.retryable && recipient.item!=null->IconButton(modifier=Modifier.size(40.dp),onClick={retry(recipient)}) {Symbol(R.drawable.ic_refresh,resources.getString(R.string.cd_retry),modifier=Modifier.size(22.dp),tint=c.text2)}
                        recipient.pending && recipient.item!=null->IconButton(modifier=Modifier.size(40.dp),onClick={stop(recipient)}) {Symbol(R.drawable.ic_close,if(sending)resources.getString(R.string.cd_stop_sending) else resources.getString(R.string.cd_dont_send),modifier=Modifier.size(22.dp),tint=c.text2)}
                    }
                }
            }
        }})
}

/** What other devices call this phone. Save needs a name. */
@Composable internal fun RenameDialog(current:String,dismiss:()->Unit,save:(String)->Unit) {
    val resources=LocalResources.current;val c=LocalPneuColors.current
    var value by rememberSaveable(stateSaver=androidx.compose.ui.text.input.TextFieldValue.Saver) {mutableStateOf(TextFieldValue(current,TextRange(current.length)))}
    val name=value.text.trim()
    val focus=remember {FocusRequester()}
    LaunchedEffect(Unit) {runCatching {focus.requestFocus()}}
    PneuDialog(onDismissRequest=dismiss,title={Text(resources.getString(R.string.rename_title))},
        text={OutlinedTextField(value,{next->value=if(utf8Prefix(next.text,128)==next.text && '\u0000' !in next.text)next else value},
            Modifier.fillMaxWidth().padding(top=12.dp).focusRequester(focus),singleLine=true,label={Text(resources.getString(R.string.pref_name))},
            textStyle=PneuType.body.copy(color=c.text),shape=RoundedCornerShape(8.dp),
            keyboardOptions=KeyboardOptions(capitalization=KeyboardCapitalization.Words,imeAction=ImeAction.Done),
            keyboardActions=KeyboardActions(onDone={if(name.isNotEmpty())save(name)}),
            colors=OutlinedTextFieldDefaults.colors(focusedBorderColor=c.accent,unfocusedBorderColor=c.fill2,focusedLabelColor=c.accentText,unfocusedLabelColor=c.text2,cursorColor=c.accent,focusedContainerColor=Color.Transparent,unfocusedContainerColor=Color.Transparent))},
        confirmButton={PneuTextButton(onClick={save(name)},enabled=name.isNotEmpty()) {Text(resources.getString(R.string.action_save))}},
        dismissButton={PneuTextButton(onClick=dismiss) {Text(resources.getString(R.string.cd_cancel))}})
}
/** Android's own device name until this phone is renamed, as the connection uses. */
internal fun selfName(context:android.content.Context,p:Prefs)=p.selfName.ifEmpty {Settings.Global.getString(context.contentResolver,Settings.Global.DEVICE_NAME) ?: android.os.Build.MODEL}

/** Choose devices each time: Share opens this over the app you shared from.
 * Connected devices start selected; an offline one can be chosen and waits. */
@Composable internal fun TargetPicker(peers:List<Peer>,links:Map<String,LinkState>,send:(Set<String>)->Unit,cancel:()->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val c=LocalPneuColors.current
    val online:(Peer)->Boolean={links[it.deviceId]?.connectionStatus==ConnectionStatus.CONNECTED}
    val ordered=remember(peers) {peers.sortedWith(compareByDescending<Peer> {online(it)}.thenByDescending {it.lastConnected})}
    var selected by rememberSaveable(peers.map {it.deviceId}) {mutableStateOf(peers.filter(online).map {it.deviceId})}
    PneuDialog(onDismissRequest=cancel,title={Text(resources.getString(R.string.picker_title))},
        text={Column(Modifier.padding(top=2.dp)) {
            ordered.forEachIndexed {i,peer->
                val chosen=peer.deviceId in selected;val connected=online(peer);val route=links[peer.deviceId]?.route.orEmpty()
                val line=when {
                    connected->if(route.isEmpty())resources.getString(R.string.status_connected) else resources.getString(R.string.device_connected,UiPresentation.status(context,route))
                    chosen->resources.getString(R.string.picker_sends_when_connected)
                    else->resources.getString(R.string.device_not_connected)
                }
                Row(Modifier.fillMaxWidth().pneuRow(i>0,50.dp).pneuToggle(chosen,true,androidx.compose.ui.semantics.Role.Checkbox) {
                    selected=if(it)selected+peer.deviceId else selected-peer.deviceId
                }.heightIn(min=60.dp).padding(vertical=8.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(14.dp)) {
                    DeviceTile(if(connected)LinkTone.On else LinkTone.Neutral)
                    Column(Modifier.weight(1f)) {
                        Text(peer.name,style=PneuType.body,color=c.text,maxLines=1,overflow=TextOverflow.Ellipsis)
                        Text(line,style=PneuType.subhead,color=c.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
                    }
                    Box(Modifier.size(24.dp),contentAlignment=Alignment.Center) {
                        if(chosen)Symbol(R.drawable.ic_check_circle,modifier=Modifier.size(24.dp),tint=c.accent)
                        else Box(Modifier.size(20.dp).border(2.dp,c.text3,CircleShape))
                    }
                }
            }
        }},
        confirmButton={PneuTextButton(onClick={send(selected.toSet())},enabled=selected.isNotEmpty()) {Text(resources.getString(R.string.action_send))}},
        dismissButton={PneuTextButton(onClick=cancel) {Text(resources.getString(R.string.cd_cancel))}})
}

internal val CapturedAuthoritySaver=androidx.compose.runtime.saveable.Saver<CapturedAuthority?,List<String>>(
    save={it?.saved()},restore={CapturedAuthority.restore(it)})

/** Transfer grants synchronously before any provider work or Activity teardown.
 * The selected pairing was captured before opening the system picker. */
internal fun sendFilesTo(activity:android.app.Activity,authority:CapturedAuthority,uris:List<android.net.Uri>):ManualEntryViewModel {
    val app=activity.graph
    check(authority.stillValid(app.prefs.value,app.connection.generation(authority.binding))) {"Binding changed during file selection"}
    val operation=ManualEntryViewModel(activity.application)
    ShareAdmissionService.start(activity,operation,ShareRequest(kotlinx.coroutines.CompletableDeferred(authority.binding),uris,emptyList(),explicitTargets=listOf(authority)))
    // Admission outlives the picker Activity. Keep its feedback in the same
    // process scope, using the Share flow's result presentation exactly once.
    app.scope.launch(kotlinx.coroutines.Dispatchers.Main.immediate) {operation.showResult(app.context)}
    return operation
}

/** Shown by ShareActivity while its operation waits for a choice. */
@Composable internal fun SharePicker(operation:ManualEntryViewModel) {
    val peers by operation.targetSelection.collectAsState()
    val links by LocalContext.current.graph.connection.devices.collectAsState()
    val list=peers ?: return
    PneuTheme {TargetPicker(list,links,{operation.selectTargets(it)}) {operation.cancelTargetSelection()}}
}

internal val ConfirmedSetupMemberSaver=androidx.compose.runtime.saveable.Saver<ConfirmedSetupMember?,String>(
    save={it?.let {confirmed->com.google.gson.Gson().toJson(confirmed)}},
    restore={runCatching {com.google.gson.Gson().fromJson(it,ConfirmedSetupMember::class.java)}.getOrNull()})
