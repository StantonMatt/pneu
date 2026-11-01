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


/** The one-time Link offer on Home. With no action it says where to finish; ✕ hides it for good. */
@Composable fun PneuLinkBanner(text:String,action:String?,onAction:()->Unit,onDismiss:()->Unit,dismissLabel:String) {
    val c=LocalPneuColors.current
    Surface(shape=RoundedCornerShape(20.dp),color=c.surface,border=if(c.dark)BorderStroke(1.dp,c.hairline)else null,
        modifier=Modifier.fillMaxWidth().padding(bottom=12.dp).then(if(action!=null)Modifier.pneuClick(onClick=onAction) else Modifier)) {
        Row(Modifier.padding(start=12.dp,end=2.dp,top=6.dp,bottom=6.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(12.dp)) {
            Box(Modifier.size(32.dp).background(if(action!=null)c.accentSoft else c.fill,RoundedCornerShape(10.dp)),contentAlignment=Alignment.Center) {
                Symbol(R.drawable.ic_link,modifier=Modifier.size(20.dp),tint=if(action!=null)c.onAccentSoft else c.text2)
            }
            Text(text,Modifier.weight(1f).padding(vertical=8.dp),style=PneuType.bodyStrong,maxLines=2,overflow=TextOverflow.Ellipsis)
            if(action!=null)Text(action,Modifier.heightIn(min=48.dp).padding(horizontal=6.dp).wrapContentHeight(),style=PneuType.button,color=c.accentText)
            IconButton(onClick=onDismiss,modifier=Modifier.size(48.dp)) {Symbol(R.drawable.ic_close,dismissLabel,modifier=Modifier.size(20.dp),tint=c.text2)}
        }
    }
}
