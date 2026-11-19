package io.github.stantonmatt.pneu.ui

import android.view.WindowInsetsController
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.DialogWindowProvider

/** A sheet over the current screen. Swiping down, Back or tapping outside closes it. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable fun PneuSheet(onDismissRequest:()->Unit,content:@Composable ColumnScope.()->Unit) {
    val c=LocalPneuColors.current
    val hostView=LocalView.current
    ModalBottomSheet(onDismissRequest,sheetState=rememberModalBottomSheetState(skipPartiallyExpanded=true),
        shape=RoundedCornerShape(topStart=28.dp,topEnd=28.dp),containerColor=c.raised,contentColor=c.text,tonalElevation=0.dp,scrimColor=c.scrim,
        dragHandle={Box(Modifier.padding(top=10.dp,bottom=18.dp).size(36.dp,4.dp).background(c.text3.copy(alpha=.5f),RoundedCornerShape(2.dp)))}) {
        // Keep the screen's status-bar icon choice, including the dark scanner, as PneuDialog does.
        val view=LocalView.current
        SideEffect {(view.parent as? DialogWindowProvider)?.window?.insetsController?.let {sheet->
            hostView.windowInsetsController?.let {host->sheet.setSystemBarsAppearance(host.systemBarsAppearance,WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS)}
        }}
        Column(Modifier.fillMaxWidth().padding(start=20.dp,end=20.dp,bottom=24.dp),content=content)
    }
}
