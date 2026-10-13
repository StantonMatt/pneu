package io.github.stantonmatt.pneu.ui

import androidx.compose.foundation.*
import androidx.compose.foundation.interaction.*
import androidx.compose.foundation.selection.toggleable
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

@Composable fun Modifier.pneuRow(divider:Boolean=false,inset:Dp=16.dp):Modifier {
    val c=LocalPneuColors.current
    return drawWithContent {
        drawContent()
        if(divider)drawLine(c.separator,Offset(inset.toPx(),0f),Offset(size.width,0f),1f)
    }
}
@Composable fun Modifier.pneuClick(enabled:Boolean=true,onClick:()->Unit):Modifier {
    val interaction=remember {MutableInteractionSource()};val pressed by interaction.collectIsPressedAsState();val c=LocalPneuColors.current
    return pneuFocusRing(interaction,enabled=enabled).clickable(interactionSource=interaction,indication=null,enabled=enabled,onClick=onClick).drawWithContent {drawContent();if(pressed)drawRect(c.text.copy(alpha=.06f))}
}
@Composable fun Modifier.pneuToggle(checked:Boolean,enabled:Boolean,onChange:(Boolean)->Unit):Modifier {
    val interaction=remember {MutableInteractionSource()};val pressed by interaction.collectIsPressedAsState();val c=LocalPneuColors.current
    return pneuFocusRing(interaction,enabled=enabled).toggleable(checked,interactionSource=interaction,indication=null,enabled=enabled,role=Role.Switch,onValueChange=onChange).drawWithContent {drawContent();if(pressed)drawRect(c.text.copy(alpha=.06f))}
}

/** Join separately animated Recent rows into one card without internal borders. */
@Composable fun Modifier.pneuCardRowHairline(first:Boolean,last:Boolean):Modifier {
    val c=LocalPneuColors.current
    if(!c.dark)return this
    return drawWithContent {
        drawContent()
        val half=.5f
        val radius=minOf(24.dp.toPx(),size.width/2,size.height/2)
        val diameter=(2*radius-1f).coerceAtLeast(0f)
        val start=if(first)radius else 0f;val end=if(last)size.height-radius else size.height
        drawLine(c.hairline,Offset(half,start),Offset(half,end),1f)
        drawLine(c.hairline,Offset(size.width-half,start),Offset(size.width-half,end),1f)
        if(first) {
            drawLine(c.hairline,Offset(radius,half),Offset(size.width-radius,half),1f)
            drawArc(c.hairline,180f,90f,false,Offset(half,half),Size(diameter,diameter),style=Stroke(1f))
            drawArc(c.hairline,270f,90f,false,Offset(size.width-2*radius+half,half),Size(diameter,diameter),style=Stroke(1f))
        }
        if(last) {
            drawLine(c.hairline,Offset(radius,size.height-half),Offset(size.width-radius,size.height-half),1f)
            drawArc(c.hairline,90f,90f,false,Offset(half,size.height-2*radius+half),Size(diameter,diameter),style=Stroke(1f))
            drawArc(c.hairline,0f,90f,false,Offset(size.width-2*radius+half,size.height-2*radius+half),Size(diameter,diameter),style=Stroke(1f))
        }
    }
}
