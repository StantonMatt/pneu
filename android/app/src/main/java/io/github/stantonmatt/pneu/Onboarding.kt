package io.github.stantonmatt.pneu

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.*
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.data.Peer
import io.github.stantonmatt.pneu.features.*
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.launch

/** On the scanner, always dark: the way to Pneu for someone without it on the computer yet. */
@Composable internal fun GetPneuButton(onClick:()->Unit) {
    val interaction=remember {MutableInteractionSource()};val pressed by interaction.collectIsPressedAsState();val shape=RoundedCornerShape(14.dp)
    val scale by animateFloatAsState(if(pressed).97f else 1f,tween(if(LocalPneuMotion.current)PneuMotion.fast else 0),label="Get Pneu press")
    Row(Modifier.graphicsLayer {scaleX=scale;scaleY=scale}.clip(shape).background(Color.White.copy(alpha=.14f)).pneuFocusRing(interaction,shape)
        .clickable(interactionSource=interaction,indication=null,role=Role.Button,onClick=onClick).heightIn(min=44.dp).padding(horizontal=18.dp,vertical=8.dp),
        verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(8.dp)) {
        Symbol(R.drawable.ic_computer,modifier=Modifier.size(20.dp),tint=Color.White)
        Text(LocalResources.current.getString(R.string.pair_get_pneu),color=Color.White,style=PneuType.button)
    }
}

/** The download page's address: tap it to copy, or share it to open on the computer. */
@Composable internal fun GetPneuSheet(close:()->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val c=LocalPneuColors.current;val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    val url=resources.getString(R.string.download_url)
    PneuSheet(close) {
        Text(resources.getString(R.string.pair_get_pneu),style=PneuType.title2)
        Text(resources.getString(R.string.get_pneu_platforms),Modifier.padding(top=8.dp),style=PneuType.dialog,color=c.text2)
        Row(Modifier.padding(top=18.dp).fillMaxWidth().clip(RoundedCornerShape(14.dp)).background(c.fill).pneuClick {
            // Android shows its own copied confirmation.
            context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText(resources.getString(R.string.app_name),url))
        }.padding(horizontal=16.dp,vertical=14.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(10.dp)) {
            Symbol(R.drawable.ic_link,modifier=Modifier.size(20.dp),tint=c.accentText)
            Text(resources.getString(R.string.download_url_label),Modifier.weight(1f),style=PneuType.headline,color=c.accentText,maxLines=1,overflow=TextOverflow.Ellipsis)
            Symbol(R.drawable.ic_content_copy,resources.getString(R.string.cd_copy_link),Modifier.size(20.dp),tint=c.text2)
        }
        PneuButton({scope.launch {actions.run(resources.getString(R.string.ui_error_share)) {
            // Pneu's own share target only reaches computers that already have Pneu.
            context.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT,url),null)
                .putExtra(Intent.EXTRA_EXCLUDE_COMPONENTS,arrayOf(android.content.ComponentName(context,ShareActivity::class.java))))
        }}},Modifier.padding(top=18.dp).fillMaxWidth()) {
            Symbol(R.drawable.ic_share,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_share_link))
        }
    }
}

/** null when the device isn't unreachable. Shared by the Link card and a device page. */
@Composable internal fun unreachableHint(unreachable:Boolean,peers:List<Peer>,tailscaleAllowed:Boolean):UnreachableHint? {
    val network=rememberPhoneNetwork();val installed=rememberTailscaleInstalled()
    if(!unreachable)return null
    return ReachabilityHints.unreachable(network,installed,tailscaleAllowed,peers.any {peer->!peer.pairingLost && peer.addresses.any {it.tailscale}})
}
/** The detail line for a hint; LAST_CONNECTED keeps the caller's own line. */
internal fun unreachableDetail(context:android.content.Context,hint:UnreachableHint,peers:List<Peer>):String? = when(hint) {
    UnreachableHint.SAME_WIFI -> context.getString(R.string.detail_same_wifi)
    UnreachableHint.OFF_OR_OTHER_WIFI -> peers.singleOrNull()?.let {context.getString(R.string.detail_off_or_other_wifi,it.name)} ?: context.getString(R.string.detail_devices_off_or_other_wifi)
    UnreachableHint.TAILSCALE_OFF -> context.getString(R.string.detail_tailscale_off)
    UnreachableHint.LAST_CONNECTED -> null
}

/** Connect anywhere, or Open Tailscale when it's installed but off. Nothing when Tailscale is on. */
@Composable internal fun UnreachableAction(hint:UnreachableHint,modifier:Modifier,anywhere:()->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    when(hint) {
        UnreachableHint.TAILSCALE_OFF -> PneuTonalButton(onClick={scope.launch {actions.run(resources.getString(R.string.ui_error_tailscale)) {ReachabilityHints.openTailscale(context)}}},modifier=modifier) {
            Symbol(R.drawable.ic_open_in_new,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(resources.getString(R.string.action_open_tailscale),maxLines=1,overflow=TextOverflow.Ellipsis)
        }
        UnreachableHint.SAME_WIFI,UnreachableHint.OFF_OR_OTHER_WIFI -> PneuTonalButton(onClick=anywhere,modifier=modifier) {Text(resources.getString(R.string.action_connect_anywhere),maxLines=1,overflow=TextOverflow.Ellipsis)}
        UnreachableHint.LAST_CONNECTED -> {}
    }
}
internal fun UnreachableHint.hasAction()=this!=UnreachableHint.LAST_CONNECTED

/** One row per way to connect away from Wi-Fi. Today that's Tailscale. */
@Composable internal fun ConnectAnywhereSheet(tailscaleAllowed:Boolean,close:()->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current;val c=LocalPneuColors.current;val scope=rememberCoroutineScope();val actions=LocalUiActions.current
    val installed=rememberTailscaleInstalled()
    PneuSheet(close) {
        Text(resources.getString(R.string.anywhere_title),style=PneuType.title2)
        Text(resources.getString(R.string.anywhere_text),Modifier.padding(top=8.dp),style=PneuType.dialog,color=c.text2)
        Surface(Modifier.padding(top=16.dp).fillMaxWidth(),shape=RoundedCornerShape(24.dp),color=c.fill) {
            Row(Modifier.heightIn(min=64.dp).padding(start=16.dp,end=10.dp,top=10.dp,bottom=10.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(14.dp)) {
                Box(Modifier.size(36.dp).background(c.accentSoft,RoundedCornerShape(11.dp)),contentAlignment=Alignment.Center) {Symbol(R.drawable.ic_public,modifier=Modifier.size(20.dp),tint=c.onAccentSoft)}
                Column(Modifier.weight(1f)) {
                    Text(resources.getString(R.string.tile_tailscale),style=PneuType.body)
                    Text(resources.getString(R.string.anywhere_tailscale_detail),style=PneuType.subhead,color=c.text2)
                }
                when {
                    installed && tailscaleAllowed -> Text(resources.getString(R.string.status_on),Modifier.padding(horizontal=6.dp),style=PneuType.button,color=c.text2)
                    // Installed, but Settings → Tailscale when away is off.
                    installed -> PneuButton({scope.launch {actions.run(resources.getString(R.string.ui_error_setting)) {context.graph.preferences.toggle("tailscale",true)}}},kind=ButtonKind.Text,contentPadding=PaddingValues(horizontal=6.dp)) {Text(resources.getString(R.string.action_turn_on))}
                    else -> PneuButton({scope.launch {actions.run(resources.getString(R.string.ui_error_play)) {ReachabilityHints.openTailscaleListing(context)}}},kind=ButtonKind.Text,contentPadding=PaddingValues(horizontal=6.dp)) {Text(resources.getString(R.string.action_get))}
                }
            }
        }
    }
}
