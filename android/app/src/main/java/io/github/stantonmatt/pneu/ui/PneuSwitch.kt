package io.github.stantonmatt.pneu.ui

import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.*
import androidx.compose.foundation.*
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.runtime.*
import androidx.compose.ui.*
import androidx.compose.ui.draw.*
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp

@Composable fun PneuSwitch(checked:Boolean,onCheckedChange:((Boolean)->Unit)?,enabled:Boolean=true,modifier:Modifier=Modifier) {
    val c=LocalPneuColors.current;val duration=if(LocalPneuMotion.current)PneuMotion.short else 0
    val interaction=remember {MutableInteractionSource()}
    val x by animateDpAsState(if(checked)23.dp else 3.dp,tween(duration,easing=PneuMotion.ease),label="Switch thumb")
    val track by animateColorAsState(if(checked)c.accent else c.switchOff,tween(duration),label="Switch track")
    Box(modifier.size(52.dp,32.dp).alpha(if(enabled)1f else .4f).clip(CircleShape)
        .then(if(onCheckedChange!=null)Modifier.pneuFocusRing(interaction,CircleShape,enabled).toggleable(checked,interactionSource=interaction,indication=null,enabled=enabled,role=Role.Switch,onValueChange=onCheckedChange) else Modifier).background(track)) {
        Box(Modifier.offset(x=x,y=3.dp).size(26.dp).shadow(3.dp,CircleShape).background(Color.White,CircleShape))
    }
}
