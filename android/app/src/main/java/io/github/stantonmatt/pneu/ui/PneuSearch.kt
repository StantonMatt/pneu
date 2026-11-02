package io.github.stantonmatt.pneu.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsFocusedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.Symbol

@Composable internal fun PneuSearch(query:String,change:(String)->Unit) {
    val c=LocalPneuColors.current;val keyboard=LocalSoftwareKeyboardController.current
    val searchLabel=stringResource(R.string.history_search)
    val interaction=remember {MutableInteractionSource()};val focused by interaction.collectIsFocusedAsState()
    Surface(shape=RoundedCornerShape(16.dp),color=c.surface,border=if(focused)BorderStroke(2.dp,c.accent)else if(c.dark)BorderStroke(1.dp,c.hairline)else null) {
        Row(Modifier.fillMaxWidth().height(48.dp).padding(start=16.dp,end=4.dp),verticalAlignment=Alignment.CenterVertically,horizontalArrangement=Arrangement.spacedBy(12.dp)) {
            Symbol(R.drawable.ic_search,modifier=Modifier.size(22.dp),tint=c.text2)
            BasicTextField(query,change,interactionSource=interaction,modifier=Modifier.weight(1f).semantics {contentDescription=searchLabel},singleLine=true,
                textStyle=PneuType.body.copy(color=c.text),cursorBrush=SolidColor(c.accent),
                keyboardOptions=KeyboardOptions(imeAction=ImeAction.Search),keyboardActions=KeyboardActions(onSearch={keyboard?.hide()}),
                decorationBox={field->Box {if(query.isEmpty())Text(stringResource(R.string.history_search),style=PneuType.body,color=c.text3);field()}})
            if(query.isNotEmpty())IconButton(onClick={change("")}) {Symbol(R.drawable.ic_close,stringResource(R.string.history_clear_search),Modifier.size(22.dp),c.text2)}
        }
    }
}

@Composable internal fun PneuSearchNoMatches() {
    val c=LocalPneuColors.current
    PneuCard {Column(Modifier.fillMaxWidth().height(132.dp),horizontalAlignment=Alignment.CenterHorizontally,verticalArrangement=Arrangement.Center) {
        Symbol(R.drawable.ic_search,modifier=Modifier.size(28.dp),tint=c.text3)
        Text(stringResource(R.string.history_no_matches),Modifier.padding(top=8.dp),style=PneuType.subhead,color=c.text2)
    }}
}
