package io.github.stantonmatt.pneu

import android.content.ClipboardManager
import android.graphics.BitmapFactory
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
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

/** A history selection leaves rows in place; a sent/received arrival, including
 * a duplicate, is inserted at the top. Storage order wins on the next visit. */
internal fun historyDisplayOrder(previous:List<String>,items:List<SavedClip>,
    previousArrivals:Map<String,Long> = emptyMap(),arrivals:Map<String,Long> = emptyMap()):List<String> {
    val ids=items.map {it.id}
    val incoming=ids.filter {it !in previous || arrivals[it]!=previousArrivals[it]}
        .sortedByDescending {arrivals[it] ?: 0L}
    return incoming+previous.filter {it in ids && it !in incoming}
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun ClipboardHistoryScreen(back:()->Unit) {
    val context=LocalContext.current;val app=context.graph;val c=LocalPneuColors.current
    val resources=LocalResources.current;val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    val state by app.savedClips.state.collectAsState()
    var order by remember {mutableStateOf(state.items.map {it.id})}
    var observedArrivals by remember {mutableStateOf(state.arrivals)}
    // Derive immediately as well, so removal has no frame of stale content.
    val displayed=historyDisplayOrder(order,state.items,observedArrivals,state.arrivals)
    SideEffect {if(order!=displayed)order=displayed;if(observedArrivals!=state.arrivals)observedArrivals=state.arrivals}
    val byId=state.items.associateBy {it.id};val rows=displayed.mapNotNull(byId::get)
    var copied by remember {mutableStateOf<Pair<String,Long>?>(null)}
    var clear by remember {mutableStateOf(false)}
    LaunchedEffect(copied) {if(copied!=null) {delay(2000);copied=null}}
    val scroll=TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    Scaffold(modifier=Modifier.nestedScroll(scroll.nestedScrollConnection),containerColor=Color.Transparent,contentWindowInsets=WindowInsets.statusBars,
        topBar={LargeTopAppBar(expandedHeight=114.dp,collapsedHeight=56.dp,scrollBehavior=scroll,
            title={Text(stringResource(R.string.history_title),Modifier.padding(start=4.dp),style=if(scroll.state.collapsedFraction>.5f)PneuType.title2 else PneuType.largeTitle)},
            navigationIcon={IconButton(onClick=back) {Symbol(R.drawable.ic_arrow_back,stringResource(R.string.cd_back))}},
            actions={if(rows.isNotEmpty())PneuTextButton({clear=true}) {Text(stringResource(R.string.history_clear_all))}},
            colors=TopAppBarDefaults.topAppBarColors(containerColor=c.bg,scrolledContainerColor=c.bg))}) {padding->
        Box(Modifier.padding(padding).fillMaxSize(),contentAlignment=Alignment.TopCenter) {
            LazyColumn(Modifier.widthIn(max=600.dp).fillMaxWidth(),contentPadding=PaddingValues(start=16.dp,end=16.dp,bottom=24.dp+WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding())) {
                if(rows.isEmpty())item {
                    PneuCard {Column(Modifier.fillMaxWidth().height(132.dp),horizontalAlignment=Alignment.CenterHorizontally,verticalArrangement=Arrangement.Center) {
                        Symbol(R.drawable.ic_content_paste,modifier=Modifier.size(28.dp),tint=c.text3)
                        Text(stringResource(R.string.history_empty),Modifier.padding(top=8.dp),style=PneuType.subhead,color=c.text2)
                    }}
                }
                itemsIndexed(rows,key={_,item->item.id}) {index,item->
                    val first=index==0;val last=index==rows.lastIndex
                    Surface(Modifier.fillMaxWidth().pneuCardRowHairline(first,last),color=c.surface,
                        shape=RoundedCornerShape(topStart=if(first)24.dp else 0.dp,topEnd=if(first)24.dp else 0.dp,bottomStart=if(last)24.dp else 0.dp,bottomEnd=if(last)24.dp else 0.dp)) {
                        Row(Modifier.fillMaxWidth().pneuRow(!first,76.dp).pneuClick {
                            scope.launch {actions.run(resources.getString(R.string.toast_cant_read)) {
                                if(app.savedClips.copy(item.id) {clip->withContext(Dispatchers.Main) {context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)}})
                                    copied=item.id to System.nanoTime()
                            }}
                        }.heightIn(min=68.dp).padding(start=16.dp,end=8.dp,top=12.dp,bottom=12.dp),
                            verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
                            HistoryThumbnail(item)
                            Column(Modifier.weight(1f)) {
                                val text=item.text.replace(Regex("\\s+")," ").trim()
                                val url=text.matches(Regex("https?://\\S+"))
                                Text(if(item.image.isNotEmpty())stringResource(R.string.kind_image) else if(url)text.removePrefix("https://").removePrefix("http://")else text,
                                    style=PneuType.body,maxLines=if(url)1 else 2,overflow=TextOverflow.Ellipsis)
                                if(copied?.first==item.id)Row(verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(4.dp)) {
                                    Symbol(R.drawable.ic_check,modifier=Modifier.size(16.dp),tint=c.accentText)
                                    Text(stringResource(R.string.history_copied),style=PneuType.subhead,color=c.accentText)
                                } else {
                                    val origin=if(item.sender.isEmpty())stringResource(R.string.history_this_phone)else stringResource(R.string.history_from,item.sender)
                                    Text(stringResource(R.string.history_origin_time,origin,lastConnectedLabel(context,item.copiedAt)),style=PneuType.subhead,color=c.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
                                }
                            }
                            IconButton(onClick={scope.launch {actions.run(resources.getString(R.string.ui_error_remove_item)) {app.savedClips.delete(item.id)}}}) {
                                Symbol(R.drawable.ic_close,stringResource(R.string.history_delete),Modifier.size(22.dp),c.text2)
                            }
                        }
                    }
                }
            }
        }
    }
    if(clear)HistoryDeleteDialog(rows.size,false,{clear=false}) {
        scope.launch {if(actions.run(resources.getString(R.string.ui_error_clear_queue)) {app.savedClips.clear()})clear=false}
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

@Composable internal fun HistoryDeleteDialog(count:Int,turnOff:Boolean,cancel:()->Unit,confirm:()->Unit) {
    val resources=LocalResources.current
    PneuDialog(destructive=true,onDismissRequest=cancel,
        title={Text(stringResource(if(turnOff)R.string.history_off_title else R.string.history_clear_title))},
        text={Text(resources.getQuantityString(R.plurals.history_delete_count,count,count))},
        dismissButton={PneuTextButton(cancel) {Text(stringResource(R.string.action_cancel))}},
        confirmButton={PneuTextButton(confirm) {Text(stringResource(if(turnOff)R.string.history_turn_off else R.string.history_clear))}})
}
