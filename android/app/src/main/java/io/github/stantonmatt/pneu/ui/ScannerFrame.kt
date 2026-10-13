package io.github.stantonmatt.pneu.ui

import androidx.compose.animation.core.*
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.graphics.drawscope.*
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.Symbol
import kotlin.math.roundToInt

internal data class ScannerPlacement(val frameSize:Int,val top:Int,val gap:Int)

/** Coordinates within the safe drawing area; reserve the measured instruction first. */
internal fun scannerPlacement(width:Int,height:Int,instructionHeight:Int,density:Float):ScannerPlacement {
    fun px(dp:Int)=(dp*density).roundToInt()
    val remaining=(height-instructionHeight).coerceAtLeast(0)
    val bottom=minOf(px(16),remaining)
    val reserve=minOf(px(56),remaining-bottom)
    val gap=minOf(px(28),remaining-bottom-reserve)
    val frame=minOf(px(264),(width-px(32)).coerceAtLeast(0),remaining-bottom-reserve-gap)
    val top=minOf(px(200),remaining-bottom-gap-frame)
    return ScannerPlacement(frame,top,gap)
}

@Composable fun ScannerFrame(busy:Boolean,success:Boolean,instruction:String) {
    val animate=LocalPneuMotion.current
    val density=LocalDensity.current;val direction=LocalLayoutDirection.current
    val insets=WindowInsets.safeDrawing
    val left=insets.getLeft(density,direction);val right=insets.getRight(density,direction)
    val topInset=insets.getTop(density);val bottom=insets.getBottom(density)
    val textGutter=with(density){20.dp.roundToPx()}
    val scale by animateFloatAsState(if(busy).96f else 1f,tween(if(animate)PneuMotion.short else 0),label="Scanner frame")
    val fill by animateFloatAsState(if(success)1f else 0f,tween(if(animate)PneuMotion.long else 0,easing=PneuMotion.easeOut),label="Pair success")
    var frame by remember {mutableStateOf(Rect.Zero)}
    Layout(modifier=Modifier.fillMaxSize().drawBehind {
        val cut=frame.width;val x=frame.left;val y=frame.top
        val ratio=cut/264.dp.toPx();val r=36.dp.toPx()*ratio
        val scrim=Path().apply {
            fillType=PathFillType.EvenOdd
            addRect(Rect(Offset.Zero,size))
            addRoundRect(RoundRect(frame,r,r))
        }
        drawPath(scrim,Color.Black.copy(alpha=.6f))
        if(cut>0f) {
            val color=if(busy)Color(0xFF8C95FF)else Color.White
            val arm=46.dp.toPx()*ratio;val stroke=4.dp.toPx()*ratio
            withTransform({scale(scale,scale,frame.center)}) {
                if(!success)for(i in 0..3)withTransform({rotate(i*90f,frame.center)}) {
                    drawLine(color,Offset(x+arm,y),Offset(x+r,y),stroke,StrokeCap.Round)
                    drawArc(color,180f,90f,false,Offset(x,y),Size(r*2,r*2),style=Stroke(stroke,cap=StrokeCap.Round))
                    drawLine(color,Offset(x,y+r),Offset(x,y+arm),stroke,StrokeCap.Round)
                }
            }
            if(fill>0f)withTransform({scale(fill,fill,frame.center)}) {
                drawRoundRect(Color(0xFF4F5BFF),frame.topLeft,frame.size,CornerRadius(r))
            }
        }
    },content={
        Box(contentAlignment=Alignment.Center) {
            if(busy)Box(Modifier.size(72.dp).background(Color.Black.copy(alpha=.72f),CircleShape),contentAlignment=Alignment.Center){CircularProgressIndicator(Modifier.size(36.dp),color=Color.White,strokeWidth=3.dp)}
            if(success)Symbol(R.drawable.ic_check,modifier=Modifier.size(56.dp),tint=Color.White)
        }
        Text(instruction,color=Color.White,style=PneuType.headline,textAlign=androidx.compose.ui.text.style.TextAlign.Center)
    }) {children,constraints->
        val width=constraints.maxWidth;val height=constraints.maxHeight
        val availableWidth=(width-left-right).coerceAtLeast(0)
        val availableHeight=(height-topInset-bottom).coerceAtLeast(0)
        // Measure wrapping/font scale before choosing the frame's size or offset.
        val instructionText=children[1].measure(Constraints(maxWidth=(availableWidth-2*textGutter).coerceAtLeast(0),maxHeight=availableHeight))
        val placement=scannerPlacement(availableWidth,availableHeight,instructionText.height,density.density)
        val cut=placement.frameSize
        val x=left+(availableWidth-cut)/2;val y=topInset+placement.top
        frame=Rect(x.toFloat(),y.toFloat(),(x+cut).toFloat(),(y+cut).toFloat())
        val indicator=children[0].measure(Constraints.fixed(cut,cut))
        layout(width,height) {
            indicator.place(x,y)
            instructionText.place(left+(availableWidth-instructionText.width)/2,y+cut+placement.gap)
        }
    }
}
