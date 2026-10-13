package io.github.stantonmatt.pneu.ui

import androidx.compose.animation.core.*
import androidx.compose.foundation.*
import androidx.compose.foundation.interaction.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.dp

// The wrappers keep the existing screen callbacks and semantics intact.
enum class ButtonKind { Primary,PrimaryLarge,Tonal,Neutral,Destructive,SmallTonal,Text }
internal val LocalButtonKind=staticCompositionLocalOf {ButtonKind.Text}
@Composable fun PneuButton(onClick:()->Unit,modifier:Modifier=Modifier,enabled:Boolean=true,
    kind:ButtonKind=ButtonKind.Primary,contentPadding:PaddingValues=PaddingValues(horizontal=12.dp),content:@Composable RowScope.()->Unit) {
    val c=LocalPneuColors.current;val interaction=remember {MutableInteractionSource()};val pressed by interaction.collectIsPressedAsState()
    val shape=RoundedCornerShape(when(kind){ButtonKind.SmallTonal->10.dp;ButtonKind.PrimaryLarge->16.dp;else->14.dp})
    val scale by animateFloatAsState(if(pressed).97f else 1f,tween(if(LocalPneuMotion.current)PneuMotion.fast else 0),label="Button press")
    val (fill,ink)=when(kind) {
        ButtonKind.Primary,ButtonKind.PrimaryLarge->c.accent to c.onAccent;ButtonKind.Tonal,ButtonKind.SmallTonal->c.accentSoft to c.onAccentSoft
        ButtonKind.Neutral->c.fill to c.text;ButtonKind.Destructive->c.dangerFill to c.onAccent;ButtonKind.Text->androidx.compose.ui.graphics.Color.Transparent to c.accentText
    }
    Surface(modifier.graphicsLayer {scaleX=scale;scaleY=scale;alpha=if(enabled)1f else .4f}
        .clip(shape).pneuFocusRing(interaction,shape,enabled).clickable(interactionSource=interaction,indication=null,enabled=enabled,role=androidx.compose.ui.semantics.Role.Button,onClick=onClick),
        shape=shape,color=fill,contentColor=ink) {
        ProvideTextStyle(if(kind==ButtonKind.SmallTonal)PneuType.smallButton else PneuType.button) {
            Row(Modifier.heightIn(min=when(kind){ButtonKind.SmallTonal->34.dp;ButtonKind.PrimaryLarge->52.dp;else->48.dp}).padding(contentPadding),horizontalArrangement=Arrangement.Center,verticalAlignment=Alignment.CenterVertically,content=content)
        }
    }
}
@Composable fun PneuTextButton(onClick:()->Unit,modifier:Modifier=Modifier,content:@Composable RowScope.()->Unit) {
    val kind=LocalButtonKind.current
    PneuButton(onClick,if(kind==ButtonKind.Text)modifier else modifier.fillMaxWidth(),kind=kind,content=content)
}
@Composable fun PneuTonalButton(onClick:()->Unit,modifier:Modifier=Modifier,enabled:Boolean=true,contentPadding:PaddingValues=PaddingValues(horizontal=12.dp),content:@Composable RowScope.()->Unit)=PneuButton(onClick,modifier,enabled,ButtonKind.Tonal,contentPadding,content)
@Composable fun PneuNeutralButton(onClick:()->Unit,modifier:Modifier=Modifier,content:@Composable RowScope.()->Unit)=PneuButton(onClick,modifier,kind=ButtonKind.Neutral,content=content)
