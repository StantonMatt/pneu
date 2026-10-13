package io.github.stantonmatt.pneu.ui

import androidx.compose.animation.*
import androidx.compose.animation.core.tween
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.*
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.*
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.Symbol

@Composable fun PneuDialog(onDismissRequest:()->Unit,title:@Composable ()->Unit,
    confirmButton:@Composable ()->Unit,dismissButton:(@Composable ()->Unit)?=null,
    text:(@Composable ()->Unit)?=null,destructive:Boolean=false,pairingHeader:Boolean=false,dismissible:Boolean=true) {
    val c=LocalPneuColors.current;val animate=LocalPneuMotion.current;val rise=with(androidx.compose.ui.platform.LocalDensity.current){24.dp.roundToPx()}
    var entered by remember {mutableStateOf(false)}
    // The caller owns visibility. A confirmation may open a permission prompt or
    // report an error while retaining this dialog; an action is not a dismissal.
    val dim by animateFloatAsState(if(entered)1f else 0f,tween(if(animate)160 else 0),label="Dialog scrim")
    Dialog({if(dismissible)onDismissRequest()},properties=DialogProperties(usePlatformDefaultWidth=false,decorFitsSystemWindows=false,dismissOnBackPress=dismissible,dismissOnClickOutside=dismissible)) {
        // Supply the precise Pneu scrim rather than the platform's additional dim.
        val view=androidx.compose.ui.platform.LocalView.current
        SideEffect {(view.parent as? DialogWindowProvider)?.window?.setDimAmount(0f)}
        LaunchedEffect(Unit) {entered=true}
        Box(Modifier.fillMaxSize().background(c.scrim.copy(alpha=c.scrim.alpha*dim)).pointerInput(onDismissRequest,dismissible){detectTapGestures(onTap={if(dismissible)onDismissRequest()})},contentAlignment=Alignment.BottomCenter) {
            AnimatedVisibility(entered,enter=if(animate)slideInVertically(tween(PneuMotion.medium,easing=PneuMotion.easeOut)){rise}+fadeIn(tween(PneuMotion.medium)) else fadeIn(tween(PneuMotion.short))) {
                Surface(Modifier.statusBarsPadding().navigationBarsPadding().padding(12.dp).widthIn(max=600.dp).fillMaxWidth().pointerInput(Unit){detectTapGestures(onTap={})},
                    shape=RoundedCornerShape(28.dp),color=c.raised,border=if(c.dark)BorderStroke(1.dp,c.hairline)else null,shadowElevation=PneuElevation.dialog) {
                    PneuDialogContent(title,confirmButton,dismissButton,text,destructive,pairingHeader)
                }
            }
        }
    }
}

/** Measure the actions first, then give the scrollable copy the remaining height.
 * fill=false keeps ordinary dialogs compact instead of stretching to the window. */
@Composable internal fun PneuDialogContent(title:@Composable ()->Unit,confirmButton:@Composable ()->Unit,
    dismissButton:(@Composable ()->Unit)?=null,text:(@Composable ()->Unit)?=null,
    destructive:Boolean=false,pairingHeader:Boolean=false,modifier:Modifier=Modifier) {
    val c=LocalPneuColors.current
    Column(modifier.padding(start=20.dp,end=20.dp,top=24.dp,bottom=20.dp)) {
        Column(Modifier.weight(1f,fill=false).verticalScroll(rememberScrollState())) {
            if(pairingHeader) {
                Box(Modifier.size(44.dp).background(c.accentSoft,RoundedCornerShape(14.dp)),contentAlignment=Alignment.Center) {
                    Symbol(R.drawable.ic_computer,tint=c.onAccentSoft)
                }
                Spacer(Modifier.height(16.dp))
            }
            ProvideTextStyle(PneuType.title2,title)
            text?.let {
                Spacer(Modifier.height(8.dp))
                CompositionLocalProvider(LocalContentColor provides c.text2) {ProvideTextStyle(PneuType.dialog,it)}
            }
        }
        Spacer(Modifier.height(24.dp))
        val confirmKind=if(dismissButton==null)ButtonKind.Neutral else if(destructive)ButtonKind.Destructive else ButtonKind.Primary
        if(androidx.compose.ui.platform.LocalDensity.current.fontScale>1.3f) {
            Column(verticalArrangement=Arrangement.spacedBy(10.dp)) {
                dismissButton?.let {CompositionLocalProvider(LocalButtonKind provides ButtonKind.Neutral) {it()}}
                CompositionLocalProvider(LocalButtonKind provides confirmKind) {confirmButton()}
            }
        } else {
            Row(horizontalArrangement=Arrangement.spacedBy(10.dp)) {
                dismissButton?.let {Box(Modifier.weight(1f)) {CompositionLocalProvider(LocalButtonKind provides ButtonKind.Neutral) {it()}}}
                Box(Modifier.weight(1f)) {CompositionLocalProvider(LocalButtonKind provides confirmKind) {confirmButton()}}
            }
        }
    }
}
