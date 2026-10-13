package io.github.stantonmatt.pneu.ui

import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.saveable.Saver
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.features.UiFeedback

// Retain feedback identity while the scanner enters or the Activity is recreated.
internal val UiFeedbackSaver=Saver<UiFeedback?,List<String>>(
    save={it?.let {event->listOf(event.message,event.id)} ?: emptyList()},
    restore={if(it.size==2)UiFeedback(it[0],it[1]) else null})

@Composable fun PneuSnackbarHost(state:SnackbarHostState,modifier:Modifier=Modifier) {
    val c=LocalPneuColors.current
    SnackbarHost(state,modifier.padding(horizontal=12.dp,vertical=16.dp)) {Snackbar(it,shape=RoundedCornerShape(14.dp),containerColor=c.inverse,contentColor=c.onInverse,actionColor=c.onInverse)}
}
