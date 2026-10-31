package io.github.stantonmatt.pneu

import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.semantics.*
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.ui.*

/** Device page actions for linking and Remove from all devices. */
internal data class SetupActions(val link:()->Unit={},val allow:()->Unit={},val remove:()->Unit={})
/** Home's part of linking: the one-time banner and other phones in your devices. */
internal data class HomeSetup(val banner:LinkBanner?=null,val phones:List<PhoneMember> = emptyList(),
    val link:(String)->Unit={},val dismissBanner:()->Unit={},val openPhone:(String)->Unit={})

/** The pairing confirmation's switch, on by default. At the limit it is off and can't be turned on. */
@Composable internal fun LinkSwitch(checked:Boolean,atLimit:Boolean,change:(Boolean)->Unit) {
    val c=LocalPneuColors.current;val resources=LocalResources.current
    Row(Modifier.padding(top=8.dp).fillMaxWidth().clip(RoundedCornerShape(14.dp)).background(c.fill)
        .pneuToggle(checked && !atLimit,!atLimit,onChange=change).padding(horizontal=14.dp,vertical=12.dp),
        verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(12.dp)) {
        Column(Modifier.weight(1f)) {
            Text(resources.getString(R.string.setup_link_switch),style=PneuType.bodyStrong,color=c.text)
            if(atLimit)Text(resources.getString(R.string.setup_link_limit,SetupPresentation.LIMIT),Modifier.padding(top=2.dp),style=PneuType.subhead,color=c.text2)
        }
        PneuSwitch(checked && !atLimit,null,enabled=!atLimit)
    }
}

/** Pair with {device}? Every pairing confirmation offers the switch, except a device already paired (alsoLink null). */
@Composable internal fun PairConfirmDialog(title:String,action:String,alsoLink:Boolean?,atLimit:Boolean,change:(Boolean)->Unit,cancel:()->Unit,confirm:()->Unit) {
    val resources=LocalResources.current
    PneuDialog(pairingHeader=true,onDismissRequest=cancel,title={Text(title)},text=alsoLink?.let {{LinkSwitch(it,atLimit,change)}},
        confirmButton={PneuTextButton(onClick=confirm) {Text(action)}},dismissButton={PneuTextButton(onClick=cancel) {Text(resources.getString(R.string.cd_cancel))}})
}

/** Another device asked to link. Back or tapping outside leaves it waiting. */
@Composable internal fun LinkRequestDialog(title:String,dismiss:()->Unit,deny:()->Unit,allow:()->Unit) {
    val resources=LocalResources.current
    PneuDialog(onDismissRequest=dismiss,pairingHeader=true,title={Text(title)},text={Text(resources.getString(R.string.setup_request_text))},
        confirmButton={PneuTextButton(onClick=allow) {Text(resources.getString(R.string.action_allow))}},
        dismissButton={PneuTextButton(onClick=deny) {Text(resources.getString(R.string.action_dont_allow))}})
}

/** Says what's true: removed here now, and from the others as they connect. */
@Composable internal fun RemoveFromAllDialog(name:String,waiting:Int,cancel:()->Unit,remove:()->Unit) {
    val resources=LocalResources.current
    val text=resources.getString(R.string.setup_remove_text)+if(waiting>0)" "+resources.getQuantityString(R.plurals.unpair_text,waiting,waiting) else ""
    PneuDialog(destructive=true,onDismissRequest=cancel,title={Text(resources.getString(R.string.setup_remove_title,name))},text={Text(text)},
        confirmButton={PneuTextButton(onClick=remove) {Text(resources.getString(R.string.setup_action_remove))}},
        dismissButton={PneuTextButton(onClick=cancel) {Text(resources.getString(R.string.cd_cancel))}})
}
/** One OK, primary as in the approved design. */
@Composable internal fun SetupNoticeDialog(title:String,text:String?,ok:()->Unit) {
    val resources=LocalResources.current
    PneuDialog(onDismissRequest=ok,title={Text(title)},text=text?.let {{Text(it)}},
        confirmButton={CompositionLocalProvider(LocalButtonKind provides ButtonKind.Primary) {PneuTextButton(onClick=ok) {Text(resources.getString(R.string.action_ok))}}})
}

/** A device page's last card: the linking row, Added via, then Remove from all devices or Unpair. */
@Composable internal fun SetupPageCard(name:String,page:SetupPage,actions:SetupActions,unpair:()->Unit) {
    val c=LocalPneuColors.current;val resources=LocalResources.current
    var rows=0
    @Composable fun line(title:String,detail:String?=null,titleColor:Color=c.text,click:(()->Unit)?=null,trailing:(@Composable ()->Unit)?=null) {
        Row(Modifier.fillMaxWidth().pneuRow(rows++>0).then(if(click!=null)Modifier.pneuClick(onClick=click) else Modifier)
            .heightIn(min=if(detail==null)56.dp else 68.dp).padding(horizontal=16.dp,vertical=10.dp),verticalAlignment=Alignment.CenterVertically) {
            Column(Modifier.weight(1f).semantics(mergeDescendants=true) {}) {
                Text(title,style=PneuType.body,color=titleColor)
                if(detail!=null)Text(detail,style=PneuType.subhead,color=c.text2)
            }
            trailing?.invoke()
        }
    }
    PneuCard {
        val link=resources.getString(R.string.setup_row_link)
        when(page.row) {
            LinkRow.LINK->line(link,titleColor=c.accentText,click=actions.link)
            LinkRow.PENDING->line(link,resources.getString(R.string.setup_row_pending,name))
            LinkRow.THEIRS->line(link,resources.getString(R.string.setup_row_theirs,name))
            LinkRow.ASKED->line(resources.getString(R.string.setup_row_asked)) {
                Text(resources.getString(R.string.action_allow),Modifier.clip(RoundedCornerShape(10.dp)).pneuClick(onClick=actions.allow)
                    .heightIn(min=48.dp).padding(horizontal=10.dp).wrapContentHeight().semantics {role=Role.Button},style=PneuType.button,color=c.accentText)
            }
            LinkRow.UPDATE->line(link,resources.getString(R.string.setup_row_update,name),titleColor=c.text3)
            LinkRow.SEPARATE->line(resources.getString(R.string.setup_row_separate),resources.getString(R.string.setup_row_separate_detail))
            LinkRow.NONE->{}
        }
        page.addedVia?.let {
            Text(resources.getString(R.string.setup_added_via,it),Modifier.fillMaxWidth().pneuRow(rows++>0).heightIn(min=48.dp).padding(horizontal=16.dp,vertical=14.dp),style=PneuType.dialog,color=c.text2)
        }
        line(resources.getString(if(page.member)R.string.setup_remove_all else R.string.action_unpair),titleColor=c.danger,click=if(page.member)actions.remove else unpair)
    }
}

/** Another phone in your devices: no status or sharing, since phones never connect to each other. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable internal fun PhoneMemberScreen(member:PhoneMember,back:()->Unit,remove:()->Unit) {
    val c=LocalPneuColors.current;val resources=LocalResources.current
    val scroll=rememberScrollState()
    var titleHeight by remember {mutableIntStateOf(0)}
    val collapsed by remember {derivedStateOf {titleHeight>0 && scroll.value>=titleHeight}}
    Scaffold(containerColor=Color.Transparent,contentWindowInsets=WindowInsets.statusBars,topBar={
        TopAppBar(title={if(collapsed)Text(member.name,Modifier.padding(start=4.dp),style=PneuType.title2,maxLines=1,overflow=TextOverflow.Ellipsis)},
            navigationIcon={IconButton(onClick=back) {Symbol(R.drawable.ic_arrow_back,resources.getString(R.string.cd_back))}},
            colors=TopAppBarDefaults.topAppBarColors(containerColor=c.bg,scrolledContainerColor=c.bg))
    }) {padding->
        Box(Modifier.padding(padding).fillMaxSize(),contentAlignment=Alignment.TopCenter) {
            Column(Modifier.widthIn(max=600.dp).fillMaxWidth().verticalScroll(scroll).navigationBarsPadding().padding(horizontal=16.dp)) {
                Text(member.name,Modifier.onSizeChanged {titleHeight=it.height}.padding(start=4.dp,end=4.dp,top=6.dp).semantics {heading()},style=PneuType.largeTitle,maxLines=2,overflow=TextOverflow.Ellipsis)
                Text(resources.getString(R.string.setup_phones_dont_connect_page),Modifier.padding(start=4.dp,end=4.dp,top=6.dp,bottom=16.dp),style=PneuType.subhead,color=c.text2)
                SetupPageCard(member.name,SetupPage(member.key,member.addedVia,LinkRow.NONE),SetupActions(remove=remove)) {}
                Spacer(Modifier.height(24.dp))
            }
        }
    }
}
