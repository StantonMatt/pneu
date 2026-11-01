package io.github.stantonmatt.pneu

import androidx.compose.runtime.Composable
import androidx.compose.material3.Text
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import io.github.stantonmatt.pneu.data.ReplacementChoice
import io.github.stantonmatt.pneu.ui.PneuDialog
import io.github.stantonmatt.pneu.ui.PneuTextButton
import java.util.Calendar

@Composable internal fun ReplaceDeviceDialog(choice:ReplacementChoice,keepBoth:()->Unit,replace:()->Unit) {
    val context=LocalContext.current;val resources=LocalResources.current
    val seen=choice.lastConnected
    val today=Calendar.getInstance();val date=Calendar.getInstance().apply {timeInMillis=seen}
    val whenLabel=if(today.get(Calendar.YEAR)==date.get(Calendar.YEAR) && today.get(Calendar.DAY_OF_YEAR)==date.get(Calendar.DAY_OF_YEAR))
        resources.getString(R.string.replacement_today,lastConnectedLabel(context,seen)) else lastConnectedLabel(context,seen)
    PneuDialog(onDismissRequest=keepBoth,title={Text(resources.getString(R.string.replacement_title,choice.name))},
        text={Text(if(seen==0L)resources.getString(R.string.replacement_never) else resources.getString(R.string.replacement_seen,whenLabel))},
        confirmButton={PneuTextButton(onClick=replace) {Text(resources.getString(R.string.replacement_action))}},
        dismissButton={PneuTextButton(onClick=keepBoth) {Text(resources.getString(R.string.replacement_keep_both))}})
}
