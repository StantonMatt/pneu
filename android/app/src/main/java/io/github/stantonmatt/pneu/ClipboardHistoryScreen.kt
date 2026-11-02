package io.github.stantonmatt.pneu

import android.content.ClipboardManager
import android.graphics.BitmapFactory
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.*
import androidx.compose.ui.unit.sp
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.Alignment
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.data.SavedClip
import io.github.stantonmatt.pneu.features.LocalUiActions
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.*

/** Recent rows keep their place after a copy. Pin order is independent of use. */
internal fun historyDisplayOrder(previous:List<String>,items:List<SavedClip>,
    previousArrivals:Map<String,Long> = emptyMap(),arrivals:Map<String,Long> = emptyMap(),
    previousPins:Map<String,Boolean> = emptyMap()):List<String> {
    val pins=items.filter {it.pinned}.sortedByDescending {it.pinPosition}.map {it.id}
    val recent=items.filterNot {it.pinned};val ids=recent.map {it.id}
    val unpinned=recent.filter {previousPins[it.id]==true}.map {it.id}
    val incoming=ids.filter {it !in unpinned && (it !in previous || arrivals[it]!=previousArrivals[it])}
        .sortedByDescending {arrivals[it] ?: 0L}
    val result=(incoming+previous.filter {it in ids && it !in incoming && it !in unpinned}).toMutableList()
    val positions=recent.associate {it.id to it.position}
    for(id in unpinned) {
        val index=result.indexOfFirst {positions.getValue(it)<positions.getValue(id)}
        result.add(if(index<0)result.size else index,id)
    }
    return pins+result
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun ClipboardHistoryScreen(back:()->Unit) {
    val context=LocalContext.current;val app=context.graph;val c=LocalPneuColors.current
    val resources=LocalResources.current;val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    val state by app.savedClips.state.collectAsState()
    var order by remember {mutableStateOf(state.items.map {it.id})}
    var observedArrivals by remember {mutableStateOf(state.arrivals)}
    var observedPins by remember {mutableStateOf(state.items.associate {it.id to it.pinned})}
    val displayed=historyDisplayOrder(order,state.items,observedArrivals,state.arrivals,observedPins)
    SideEffect {order=displayed;observedArrivals=state.arrivals;observedPins=state.items.associate {it.id to it.pinned}}
    val byId=state.items.associateBy {it.id}
    var query by remember {mutableStateOf("")}
    val searching=io.github.stantonmatt.pneu.data.historySearchKey(query).isNotEmpty()
    val rows=displayed.mapNotNull(byId::get).filter {io.github.stantonmatt.pneu.data.historyMatches(it,query)}
    val pinned=rows.filter {it.pinned};val recent=rows.filterNot {it.pinned}
    val hasPins=state.items.any {it.pinned}
    var copied by remember {mutableStateOf<Pair<String,Long>?>(null)}
    var clear by remember {mutableStateOf(false)}
    val snackbar=remember {SnackbarHostState()}
    LaunchedEffect(copied) {if(copied!=null) {delay(2000);copied=null}}
    val scroll=TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    Scaffold(modifier=Modifier.nestedScroll(scroll.nestedScrollConnection),containerColor=Color.Transparent,contentWindowInsets=WindowInsets.statusBars,
        snackbarHost={SnackbarHost(snackbar)},
        topBar={LargeTopAppBar(expandedHeight=114.dp,collapsedHeight=56.dp,scrollBehavior=scroll,
            title={Text(stringResource(R.string.history_title),Modifier.padding(start=4.dp),style=if(scroll.state.collapsedFraction>.5f)PneuType.title2 else PneuType.largeTitle)},
            navigationIcon={IconButton(onClick=back) {Symbol(R.drawable.ic_arrow_back,stringResource(R.string.cd_back))}},
            actions={if(query.isEmpty() && state.items.any {!it.pinned})PneuTextButton({clear=true}) {Text(stringResource(R.string.history_clear_all))}},
            colors=TopAppBarDefaults.topAppBarColors(containerColor=c.bg,scrolledContainerColor=c.bg))}) {padding->
        Box(Modifier.padding(padding).fillMaxSize(),contentAlignment=Alignment.TopCenter) {
            LazyColumn(Modifier.widthIn(max=600.dp).fillMaxWidth().imePadding(),contentPadding=PaddingValues(start=16.dp,end=16.dp,bottom=24.dp+WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding())) {
                if(state.items.isNotEmpty())item(key="search") {PneuSearch(query) {query=it};Spacer(Modifier.height(12.dp))}
                if(rows.isEmpty())item(key="empty") {
                    if(searching)PneuSearchNoMatches() else PneuCard {Column(Modifier.fillMaxWidth().height(132.dp),horizontalAlignment=Alignment.CenterHorizontally,verticalArrangement=Arrangement.Center) {
                        Symbol(R.drawable.ic_content_paste,modifier=Modifier.size(28.dp),tint=c.text3)
                        Text(stringResource(R.string.history_empty),Modifier.padding(top=8.dp),style=PneuType.subhead,color=c.text2)
                    }}
                }
                for((isPinned,group) in listOf(true to pinned,false to recent)) {
                    if(group.isNotEmpty() && hasPins)item(key=if(isPinned)"pinned"else "recent") {
                        Text(stringResource(if(isPinned)R.string.history_pinned else R.string.recent_title),
                            Modifier.padding(start=16.dp,top=if(isPinned || pinned.isEmpty())6.dp else 22.dp,bottom=8.dp),style=PneuType.section,color=c.text2)
                    }
                    itemsIndexed(group,key={_,item->item.id}) {index,item->
                        HistoryRow(item,index==0,index==group.lastIndex,copied?.first==item.id,
                            copy={scope.launch {actions.run(resources.getString(R.string.toast_cant_read)) {
                                if(app.savedClips.copy(item.id) {clip->withContext(Dispatchers.Main) {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)}})
                                    copied=item.id to System.nanoTime()
                            }}},
                            pin={scope.launch {actions.run(resources.getString(R.string.ui_error_setting)) {
                                if(!app.savedClips.setPinned(item.id,!item.pinned))snackbar.showSnackbar(resources.getString(R.string.history_pin_limit))
                            }}},
                            delete={scope.launch {actions.run(resources.getString(R.string.ui_error_remove_item)) {app.savedClips.delete(item.id)}}})
                    }
                }
            }
        }
    }
    if(clear)HistoryDeleteDialog(state.items.count {!it.pinned},false,{clear=false},pinned=state.items.count {it.pinned}) {
        scope.launch {if(actions.run(resources.getString(R.string.ui_error_clear_queue)) {app.savedClips.clear()})clear=false}
    }
}

@Composable private fun HistoryRow(item:SavedClip,first:Boolean,last:Boolean,copied:Boolean,copy:()->Unit,pin:()->Unit,delete:()->Unit) {
    val context=LocalContext.current;val c=LocalPneuColors.current;val haptic=LocalHapticFeedback.current
    var menu by remember(item.id) {mutableStateOf(false)}
    val interaction=remember {MutableInteractionSource()};val pressed by interaction.collectIsPressedAsState()
    val pinLabel=stringResource(if(item.pinned)R.string.history_unpin else R.string.history_pin)
    val deleteLabel=stringResource(R.string.history_delete)
    Box(Modifier.fillMaxWidth(),contentAlignment=Alignment.TopEnd) {
        Surface(Modifier.fillMaxWidth().pneuCardRowHairline(first,last),color=c.surface,
            shape=RoundedCornerShape(topStart=if(first)24.dp else 0.dp,topEnd=if(first)24.dp else 0.dp,bottomStart=if(last)24.dp else 0.dp,bottomEnd=if(last)24.dp else 0.dp)) {
            Row(Modifier.fillMaxWidth().pneuRow(!first,76.dp)
                .pneuFocusRing(interaction).combinedClickable(interactionSource=interaction,indication=null,onClick=copy,onLongClick={haptic.performHapticFeedback(HapticFeedbackType.LongPress);menu=true},hapticFeedbackEnabled=false)
                .drawWithContent {drawContent();if(menu || pressed)drawRect(c.text.copy(alpha=.06f))}
                .semantics {customActions=listOf(CustomAccessibilityAction(pinLabel) {pin();true},CustomAccessibilityAction(deleteLabel) {delete();true})}
                .heightIn(min=68.dp).padding(start=16.dp,end=8.dp,top=12.dp,bottom=12.dp),
                verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
                HistoryThumbnail(item)
                Column(Modifier.weight(1f)) {
                    val text=item.text.replace(Regex("\\s+")," ").trim()
                    val url=text.matches(Regex("https?://\\S+"))
                    Text(if(item.image.isNotEmpty())stringResource(R.string.kind_image) else if(url)text.removePrefix("https://").removePrefix("http://")else text,
                        style=PneuType.body,maxLines=if(url)1 else 2,overflow=TextOverflow.Ellipsis)
                    if(copied)Row(verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(4.dp)) {
                        Symbol(R.drawable.ic_check,modifier=Modifier.size(16.dp),tint=c.accentText)
                        Text(stringResource(R.string.history_copied),style=PneuType.subhead,color=c.accentText)
                    } else {
                        val origin=if(item.sender.isEmpty())stringResource(R.string.history_this_phone)else stringResource(R.string.history_from,item.sender)
                        Text(stringResource(R.string.history_origin_time,origin,lastConnectedLabel(context,item.copiedAt)),style=PneuType.subhead,color=c.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
                    }
                }
                IconButton(onClick=delete) {Symbol(R.drawable.ic_close,deleteLabel,Modifier.size(22.dp),c.text2)}
            }
        }
        // A small end-aligned anchor keeps the menu at the row's trailing edge.
        Box(Modifier.width(8.dp).height(48.dp)) {
            DropdownMenu(menu,{menu=false},modifier=Modifier.widthIn(min=196.dp),shape=RoundedCornerShape(16.dp),containerColor=c.raised,shadowElevation=PneuElevation.menu,border=if(c.dark)BorderStroke(1.dp,c.hairline)else null) {
                DropdownMenuItem(modifier=Modifier.height(44.dp),text={Text(pinLabel,style=PneuType.body.copy(fontSize=15.sp))},onClick={menu=false;pin()})
                DropdownMenuItem(modifier=Modifier.height(44.dp),text={Text(deleteLabel,style=PneuType.body.copy(fontSize=15.sp),color=c.danger)},onClick={menu=false;delete()})
            }
        }
    }
}

@Composable private fun HistoryThumbnail(item:SavedClip) {
    val context=LocalContext.current;val c=LocalPneuColors.current
    val bitmap by produceState<android.graphics.Bitmap?>(null,item.thumbnail) {
        if(item.thumbnail.isNotEmpty())value=withContext(Dispatchers.IO) {BitmapFactory.decodeFile(context.graph.savedClips.thumbnail(item).absolutePath)}
    }
    Surface(Modifier.size(44.dp),shape=RoundedCornerShape(10.dp),color=c.fill) {
        val picture=bitmap
        if(picture!=null)Image(picture.asImageBitmap(),null,contentScale=ContentScale.Crop)
        else Box(contentAlignment=Alignment.Center) {Symbol(when {
            item.image.isNotEmpty()->R.drawable.ic_image
            item.text.trim().matches(Regex("https?://\\S+"))->R.drawable.ic_link
            else->R.drawable.ic_content_paste
        },modifier=Modifier.size(22.dp),tint=c.text2)}
    }
}

@Composable internal fun HistoryDeleteDialog(count:Int,turnOff:Boolean,cancel:()->Unit,pinned:Int=0,confirm:()->Unit) {
    val resources=LocalResources.current
    val message=when {
        !turnOff && pinned>0->resources.getQuantityString(R.plurals.history_delete_count_keep_pins,count,count)
        turnOff && pinned==count && pinned>0->resources.getQuantityString(R.plurals.history_delete_pinned,pinned,pinned)
        turnOff && pinned>0->resources.getQuantityString(R.plurals.history_delete_count_with_pins,count,count,pinned)
        else->resources.getQuantityString(R.plurals.history_delete_count,count,count)
    }
    PneuDialog(destructive=true,onDismissRequest=cancel,
        title={Text(stringResource(if(turnOff)R.string.history_off_title else R.string.history_clear_title))},
        text={Text(message)},
        dismissButton={PneuTextButton(cancel) {Text(stringResource(R.string.action_cancel))}},
        confirmButton={PneuTextButton(confirm) {Text(stringResource(if(turnOff)R.string.history_turn_off else R.string.history_clear))}})
}
