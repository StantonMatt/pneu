package io.github.stantonmatt.pneu

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.features.FolderPreparing
import io.github.stantonmatt.pneu.ui.*

@Composable internal fun FolderPreparingRow(preparation:FolderPreparing,destination:String?=null,first:Boolean=true,last:Boolean=true,cancel:()->Unit) {
    val c=LocalPneuColors.current;val context=LocalContext.current;val resources=LocalResources.current
    Surface(color=c.surface,modifier=Modifier.fillMaxWidth().pneuCardRowHairline(first,last),shape=RoundedCornerShape(topStart=if(first)24.dp else 0.dp,topEnd=if(first)24.dp else 0.dp,bottomStart=if(last)24.dp else 0.dp,bottomEnd=if(last)24.dp else 0.dp)) {
        Row(Modifier.fillMaxWidth().pneuRow(!first,76.dp).heightIn(min=68.dp).padding(horizontal=16.dp,vertical=12.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(16.dp)) {
            Surface(Modifier.size(44.dp),shape=RoundedCornerShape(10.dp),color=c.fill) {
                Box(contentAlignment=Alignment.Center) {Symbol(R.drawable.ic_folder,modifier=Modifier.size(22.dp),tint=c.text2)}
            }
            Column(Modifier.weight(1f)) {
                Text(preparation.name,style=MaterialTheme.typography.bodyLarge,maxLines=1,overflow=TextOverflow.MiddleEllipsis)
                val prefix=destination?.let {resources.getString(R.string.folder_to,it)}.orEmpty()
                Text(prefix+if(preparation.total==null)resources.getString(R.string.folder_preparing_count,context.resources.getQuantityString(R.plurals.folder_items,preparation.count,java.text.NumberFormat.getIntegerInstance().format(preparation.count))) else resources.getString(R.string.folder_preparing_bytes,io.github.stantonmatt.pneu.features.UiPresentation.sizePair(preparation.done,preparation.total)),style=MaterialTheme.typography.bodyMedium,color=c.text2,maxLines=1,overflow=TextOverflow.Ellipsis)
                if(preparation.total!=null)LinearProgressIndicator(progress={if(preparation.total>0)(preparation.done.toFloat()/preparation.total).coerceIn(0f,1f) else 0f},modifier=Modifier.fillMaxWidth().padding(top=7.dp).height(3.dp).clip(RoundedCornerShape(2.dp)),color=c.accent,trackColor=c.fill2,gapSize=0.dp,drawStopIndicator={})
            }
            IconButton(onClick=cancel,modifier=Modifier.size(40.dp)) {Symbol(R.drawable.ic_close,resources.getString(R.string.cd_cancel),modifier=Modifier.size(22.dp),tint=c.text2)}
        }
    }
}
