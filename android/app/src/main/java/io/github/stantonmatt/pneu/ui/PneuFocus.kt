package io.github.stantonmatt.pneu.ui

import androidx.compose.foundation.interaction.InteractionSource
import androidx.compose.foundation.interaction.collectIsFocusedAsState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.drawOutline
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.inset
import androidx.compose.ui.input.InputMode
import androidx.compose.ui.platform.LocalInputModeManager
import androidx.compose.ui.unit.dp

/** A single indication for custom controls, without adding another focus target.
 * Draw inside the bounds so menus/cards cannot clip the ring. The narrow raised
 * halo keeps the accent ring visible on an accent-filled button or switch. */
@Composable internal fun Modifier.pneuFocusRing(interaction:InteractionSource,
    shape:Shape=RoundedCornerShape(10.dp),enabled:Boolean=true):Modifier {
    val focused by interaction.collectIsFocusedAsState()
    val input=LocalInputModeManager.current
    val c=LocalPneuColors.current
    // clickable/toggleable(enabled=false) remove only their own focus target.
    // Do not attach inheritable focus properties here: disabled container rows
    // can still contain enabled Retry/Remove buttons with independent targets.
    return drawWithContent {
        drawContent()
        if(enabled && focused && input.inputMode==InputMode.Keyboard) {
            inset(2.dp.toPx()) {
                val outline=shape.createOutline(size,layoutDirection,this)
                drawOutline(outline,c.raised,style=Stroke(4.dp.toPx()))
                drawOutline(outline,c.accent,style=Stroke(2.dp.toPx()))
            }
        }
    }
}
