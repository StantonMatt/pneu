package io.github.stantonmatt.pneu.ui

import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.*
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.*
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.geometry.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.Symbol
import io.github.stantonmatt.pneu.connection.LinkState

@Composable fun LinkView(state:LinkState,sending:Boolean) {
    val c=LocalPneuColors.current;val status=state.status;val moving=LocalPneuMotion.current
    val connected=status=="Connected";val paused=status=="Paused";val lost=status=="Pairing lost";val connecting=status=="Connecting…"
    val offline=status=="PC unreachable" || status=="Waiting for Wi-Fi"
    val duration=if(moving)PneuMotion.medium else PneuMotion.short
    val phone by animateColorAsState(when {paused->c.warnSoft;connected || offline->c.accentSoft;else->c.fill},tween(duration),label="Phone tile")
    val pc by animateColorAsState(when {lost->c.dangerSoft;connected->c.accentSoft;else->c.fill},tween(duration),label="PC tile")
    val phoneInk=when {paused->c.warn;connected || offline->c.onAccentSoft;else->c.text2}
    val pcInk=when {lost->c.danger;connected->c.onAccentSoft;else->c.text2}
    var phase=0f;var travel=.5f
    if(moving && (connecting || connected && sending)) {
        val transition=rememberInfiniteTransition(label="Link motion")
        if(connecting) {val v by transition.animateFloat(0f,1f,infiniteRepeatable(tween(800,easing=LinearEasing)),label="Dashes");phase=v}
        if(connected && sending) {val v by transition.animateFloat(0f,1f,infiniteRepeatable(tween(1400,easing=PneuMotion.transit)),label="Capsule");travel=v}
    }
    Box(Modifier.fillMaxWidth().height(56.dp).clearAndSetSemantics {}) {
    Row(Modifier.fillMaxWidth().height(56.dp).padding(horizontal=6.dp).clearAndSetSemantics {},verticalAlignment=Alignment.CenterVertically) {
        Box(Modifier.size(52.dp).background(phone,RoundedCornerShape(17.dp)),contentAlignment=Alignment.Center){Symbol(R.drawable.ic_smartphone,tint=phoneInk)}
        Canvas(Modifier.weight(1f).height(56.dp).padding(horizontal=12.dp)) {
            val y=size.height/2;val stroke=4.dp.toPx();val start=Offset(0f,y);val end=Offset(size.width,y)
            when {
                connected && !sending->drawLine(Brush.horizontalGradient(listOf(c.linkStart,c.accent)),start,end,stroke,StrokeCap.Round)
                connected->drawLine(c.accentSoft,start,end,stroke,StrokeCap.Round)
                paused->drawLine(c.fill2,start,end,stroke,StrokeCap.Round)
                lost->{drawLine(c.dangerSoft,start,Offset(size.width/3,y),stroke,StrokeCap.Round);drawLine(c.dangerSoft,Offset(size.width*2/3,y),end,stroke,StrokeCap.Round)}
                else->drawLine(if(connecting)c.text3 else c.fill2,start,end,stroke,StrokeCap.Butt,pathEffect=PathEffect.dashPathEffect(floatArrayOf(6.dp.toPx(),7.dp.toPx()),-phase*13.dp.toPx()))
            }
            if(connected && sending) {
                val w=24.dp.toPx();val h=12.dp.toPx();val x=(size.width-w)*travel
                val alpha=if(!moving)1f else (minOf(travel/.12f,(1-travel)/.12f)).coerceIn(0f,1f)
                drawRoundRect(c.surface.copy(alpha=alpha),Offset(x-3.dp.toPx(),y-h/2-3.dp.toPx()),Size(w+6.dp.toPx(),h+6.dp.toPx()),CornerRadius(9.dp.toPx()))
                drawRoundRect(c.accent.copy(alpha=alpha),Offset(x,y-h/2),Size(w,h),CornerRadius(h/2))
                drawRoundRect(c.spark.copy(alpha=alpha),Offset(x+w-6.dp.toPx(),y-h/2),Size(6.dp.toPx(),h),CornerRadius(3.dp.toPx()))
                drawLine(c.surface.copy(alpha=alpha),Offset(x+w-7.dp.toPx(),y-h/2),Offset(x+w-7.dp.toPx(),y+h/2),1.5.dp.toPx())
            }
        }
        Box(Modifier.size(52.dp).background(pc,RoundedCornerShape(17.dp)),contentAlignment=Alignment.Center){Symbol(R.drawable.ic_computer,tint=pcInk)}
    }
    // The badge cuts through the tube without changing its layout.
    if(!connected && !connecting)Box(Modifier.align(Alignment.Center).clearAndSetSemantics {},contentAlignment=Alignment.Center) {
        val fill=when {paused->c.warnSoft;lost->c.dangerSoft;else->c.surface}
        Box(Modifier.size(36.dp).background(c.surface,CircleShape).padding(4.dp).border(if(paused || lost)0.dp else 1.5.dp,c.fill2,CircleShape).background(fill,CircleShape),contentAlignment=Alignment.Center) {
            Symbol(when {paused->R.drawable.ic_pause;lost->R.drawable.ic_link_off;status=="Waiting for Wi-Fi"->R.drawable.ic_wifi_off;else->R.drawable.ic_cloud_off},modifier=Modifier.size(18.dp),tint=when {paused->c.warn;lost->c.danger;else->c.text2})
        }
    }
}
}
