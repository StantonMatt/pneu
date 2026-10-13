package io.github.stantonmatt.pneu.ui

import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.*
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.Symbol

@Composable fun PneuBanner(text:String,action:String,click:()->Unit,capacity:Boolean=false) {
    val c=LocalPneuColors.current
    Surface(shape=RoundedCornerShape(20.dp),color=c.surface,border=if(c.dark)BorderStroke(1.dp,c.hairline)else null,modifier=Modifier.fillMaxWidth().padding(bottom=12.dp).pneuClick(onClick=click)) {
        Row(Modifier.padding(start=12.dp,end=4.dp,top=10.dp,bottom=10.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(12.dp)) {
            Box(Modifier.size(32.dp).background(if(capacity)c.dangerSoft else c.warnSoft,RoundedCornerShape(10.dp)),contentAlignment=Alignment.Center){Symbol(if(capacity)R.drawable.ic_error else R.drawable.ic_warning,modifier=Modifier.size(20.dp),tint=if(capacity)c.danger else c.warn)}
            Text(text,Modifier.weight(1f),style=PneuType.bodyStrong,maxLines=2,overflow=TextOverflow.Ellipsis)
            Text(action,Modifier.heightIn(min=48.dp).padding(horizontal=10.dp).wrapContentHeight(),style=PneuType.button,color=c.accentText)
        }
    }
}

