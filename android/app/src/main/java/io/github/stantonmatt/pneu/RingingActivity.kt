package io.github.stantonmatt.pneu

import android.os.Bundle
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.github.stantonmatt.pneu.features.RingStopSource
import io.github.stantonmatt.pneu.ui.*

class RingingActivity:ComponentActivity() {
    override fun onCreate(savedInstanceState:Bundle?) {
        super.onCreate(savedInstanceState)
        setShowWhenLocked(true);setTurnScreenOn(true);enableEdgeToEdge()
        setContent {
            val state by graph.ringing.state.collectAsState()
            LaunchedEffect(state.ringing) {if(!state.ringing)finish()}
            PneuTheme {RingingScreen(state.computer) {graph.ringing.stop(RingStopSource.SURFACE)}}
        }
    }
    override fun dispatchTouchEvent(event:MotionEvent):Boolean {
        if(event.action==MotionEvent.ACTION_DOWN) {graph.ringing.stop(RingStopSource.SURFACE);finish();return true}
        return super.dispatchTouchEvent(event)
    }
}
@Composable internal fun RingingScreen(computer:String,stop:()->Unit) {
    val c=LocalPneuColors.current
    Box(Modifier.fillMaxSize().background(c.bg).padding(horizontal=24.dp)) {
        Column(Modifier.align(Alignment.TopCenter).padding(top=114.dp),horizontalAlignment=Alignment.CenterHorizontally) {
            Box(Modifier.size(272.dp).border(2.dp,c.accent.copy(alpha=.10f),CircleShape),contentAlignment=Alignment.Center) {
                Box(Modifier.size(200.dp).border(2.dp,c.accent.copy(alpha=.22f),CircleShape),contentAlignment=Alignment.Center) {
                    Box(Modifier.size(96.dp).shadow(18.dp,RoundedCornerShape(30.dp),ambientColor=c.accent,spotColor=c.accent).background(c.accent,RoundedCornerShape(30.dp)),contentAlignment=Alignment.Center) {
                        Symbol(R.drawable.ic_ring,modifier=Modifier.size(52.dp),tint=c.onAccent)
                    }
                }
            }
            Spacer(Modifier.height(8.dp))
            Text(androidx.compose.ui.res.stringResource(R.string.ring_title),style=PneuType.title1.copy(fontSize=30.sp,lineHeight=36.sp,fontWeight=FontWeight.SemiBold),color=c.text)
            Text(androidx.compose.ui.res.stringResource(R.string.ring_from,computer),Modifier.padding(top=8.dp),style=PneuType.body,color=c.text2)
        }
        PneuButton(stop,Modifier.align(Alignment.BottomCenter).padding(bottom=72.dp).fillMaxWidth().height(64.dp),kind=ButtonKind.PrimaryLarge) {
            Symbol(R.drawable.ic_stop,modifier=Modifier.size(20.dp));Spacer(Modifier.width(8.dp));Text(androidx.compose.ui.res.stringResource(R.string.ring_stop),fontSize=18.sp)
        }
    }
}
