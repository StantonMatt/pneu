package io.github.stantonmatt.pneu.ui

import io.github.stantonmatt.pneu.connection.ConnectionStatus
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.*
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.*
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.geometry.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.SemanticsPropertyKey
import androidx.compose.ui.semantics.SemanticsPropertyReceiver
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.Symbol
import io.github.stantonmatt.pneu.connection.LinkState

/** One device on the far end of the Link, coloured by its own state. */
enum class LinkTone {Neutral,On,Warn,Danger}
internal val ReceivingTransfer=SemanticsPropertyKey<Boolean>("ReceivingTransfer")
internal var SemanticsPropertyReceiver.receivingTransfer by ReceivingTransfer

/** This phone is always on the left. With several devices the right end is a
 * stack in list order: up to three tiles, or two and "+n". The tube shows the
 * overall state; the capsule runs towards whichever device receives. */
@Composable fun LinkView(state:LinkState,sending:Boolean,others:List<LinkTone> = emptyList(),receiving:Boolean=false) {
    val c=LocalPneuColors.current;val status=state.connectionStatus;val moving=LocalPneuMotion.current
    val connected=status==ConnectionStatus.CONNECTED;val paused=status==ConnectionStatus.PAUSED;val lost=status==ConnectionStatus.PAIRING_LOST;val connecting=status==ConnectionStatus.CONNECTING
    val offline=status==ConnectionStatus.UNREACHABLE || status==ConnectionStatus.WAITING_WIFI
    val busy=sending || receiving
    val duration=if(moving)PneuMotion.medium else PneuMotion.short
    val phone by animateColorAsState(when {paused->c.warnSoft;connected || offline->c.accentSoft;else->c.fill},tween(duration),label="Phone tile")
    val pc by animateColorAsState(when {lost->c.dangerSoft;connected->c.accentSoft;else->c.fill},tween(duration),label="PC tile")
    val phoneInk=when {paused->c.warn;connected || offline->c.onAccentSoft;else->c.text2}
    val pcInk=when {lost->c.danger;connected->c.onAccentSoft;else->c.text2}
    var phase=0f;var travel=.5f
    if(moving && (connecting || connected && busy)) {
        val transition=rememberInfiniteTransition(label="Link motion")
        if(connecting) {val v by transition.animateFloat(0f,1f,infiniteRepeatable(tween(800,easing=LinearEasing)),label="Dashes");phase=v}
        if(connected && busy) {val v by transition.animateFloat(0f,1f,infiniteRepeatable(tween(1400,easing=PneuMotion.transit)),label="Capsule");travel=v}
    }
    Box(Modifier.fillMaxWidth().height(56.dp).clearAndSetSemantics {receivingTransfer=connected && receiving}) {
    Row(Modifier.fillMaxWidth().height(56.dp).padding(horizontal=6.dp).clearAndSetSemantics {},verticalAlignment=Alignment.CenterVertically) {
        Box(Modifier.size(52.dp).background(phone,RoundedCornerShape(17.dp)),contentAlignment=Alignment.Center){Symbol(R.drawable.ic_smartphone,tint=phoneInk)}
        Canvas(Modifier.weight(1f).height(56.dp).padding(horizontal=12.dp)) {
            val y=size.height/2;val stroke=4.dp.toPx();val start=Offset(0f,y);val end=Offset(size.width,y)
            when {
                connected && !busy->drawLine(Brush.horizontalGradient(listOf(c.linkStart,c.accent)),start,end,stroke,StrokeCap.Round)
                connected->drawLine(c.accentSoft,start,end,stroke,StrokeCap.Round)
                paused->drawLine(c.fill2,start,end,stroke,StrokeCap.Round)
                lost->{drawLine(c.dangerSoft,start,Offset(size.width/3,y),stroke,StrokeCap.Round);drawLine(c.dangerSoft,Offset(size.width*2/3,y),end,stroke,StrokeCap.Round)}
                else->drawLine(if(connecting)c.text3 else c.fill2,start,end,stroke,StrokeCap.Butt,pathEffect=PathEffect.dashPathEffect(floatArrayOf(6.dp.toPx(),7.dp.toPx()),-phase*13.dp.toPx()))
            }
            if(connected && busy) {
                // Receiving runs right to left, nose (spark) first.
                val w=24.dp.toPx();val h=12.dp.toPx();val x=(size.width-w)*(if(receiving && !sending)1-travel else travel)
                val nose=if(receiving && !sending)x else x+w-6.dp.toPx();val seam=if(receiving && !sending)x+7.dp.toPx() else x+w-7.dp.toPx()
                val alpha=if(!moving)1f else (minOf(travel/.12f,(1-travel)/.12f)).coerceIn(0f,1f)
                drawRoundRect(c.surface.copy(alpha=alpha),Offset(x-3.dp.toPx(),y-h/2-3.dp.toPx()),Size(w+6.dp.toPx(),h+6.dp.toPx()),CornerRadius(9.dp.toPx()))
                drawRoundRect(c.accent.copy(alpha=alpha),Offset(x,y-h/2),Size(w,h),CornerRadius(h/2))
                drawRoundRect(c.spark.copy(alpha=alpha),Offset(nose,y-h/2),Size(6.dp.toPx(),h),CornerRadius(3.dp.toPx()))
                drawLine(c.surface.copy(alpha=alpha),Offset(seam,y-h/2),Offset(seam,y+h/2),1.5.dp.toPx())
            }
        }
        if(others.size>1)LinkStack(others)
        else Box(Modifier.size(52.dp).background(pc,RoundedCornerShape(17.dp)),contentAlignment=Alignment.Center){Symbol(R.drawable.ic_computer,tint=pcInk)}
    }
    // The badge cuts through the tube without changing its layout.
    if(!connected && !connecting)Box(Modifier.align(Alignment.Center).clearAndSetSemantics {},contentAlignment=Alignment.Center) {
        val fill=when {paused->c.warnSoft;lost->c.dangerSoft;else->c.surface}
        Box(Modifier.size(36.dp).background(c.surface,CircleShape).padding(4.dp).border(if(paused || lost)0.dp else 1.5.dp,c.fill2,CircleShape).background(fill,CircleShape),contentAlignment=Alignment.Center) {
            Symbol(when {paused->R.drawable.ic_pause;lost->R.drawable.ic_link_off;status==ConnectionStatus.WAITING_WIFI->R.drawable.ic_wifi_off;else->R.drawable.ic_cloud_off},modifier=Modifier.size(18.dp),tint=when {paused->c.warn;lost->c.danger;else->c.text2})
        }
    }
}
}

/** Front tile first; the others peek out behind it, each ringed in the card colour.
 * A "+n" tile sits on top at the far end so its count stays readable. */
@Composable private fun LinkStack(others:List<LinkTone>) {
    val c=LocalPneuColors.current
    val visible:List<LinkTone?> =if(others.size>3)others.take(2)+null else others
    Box(Modifier.width(52.dp+16.dp*(visible.size-1)).height(52.dp)) {
        visible.forEachIndexed {i,tone->
            val (fill,ink)=when(tone) {LinkTone.On->c.accentSoft to c.onAccentSoft;LinkTone.Warn->c.warnSoft to c.warn;LinkTone.Danger->c.dangerSoft to c.danger;else->c.fill to c.text2}
            val animated by animateColorAsState(fill,tween(if(LocalPneuMotion.current)PneuMotion.medium else PneuMotion.short),label="Stack tile")
            Box(Modifier.zIndex(if(tone==null)visible.size+1f else (visible.size-i).toFloat()).offset(x=16.dp*i-3.dp,y=(-3).dp).size(58.dp).background(c.surface,RoundedCornerShape(20.dp)).padding(3.dp).background(animated,RoundedCornerShape(17.dp)),contentAlignment=Alignment.Center) {
                if(tone==null)Text("+${others.size-2}",style=PneuType.bodyStrong.copy(fontSize=15.sp),color=c.text2)
                else Symbol(R.drawable.ic_computer,tint=ink)
            }
        }
    }
}
