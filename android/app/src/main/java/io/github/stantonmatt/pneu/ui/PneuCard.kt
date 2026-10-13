package io.github.stantonmatt.pneu.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

@Composable fun PneuCard(content:@Composable ColumnScope.()->Unit) {
    val c=LocalPneuColors.current
    Surface(Modifier.fillMaxWidth(),shape=RoundedCornerShape(24.dp),color=c.surface,border=if(c.dark)BorderStroke(1.dp,c.hairline)else null) {Column(content=content)}
}
