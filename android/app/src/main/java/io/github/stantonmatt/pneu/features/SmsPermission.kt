package io.github.stantonmatt.pneu.features

import android.Manifest
import android.app.Activity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import io.github.stantonmatt.pneu.*
import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.ui.*
import kotlinx.coroutines.launch

/** Every system request (including repair) carries the prominent disclosure. */
@Composable internal fun smsPermissionRequest(refresh:Int,onGranted:()->Unit):()->Unit {
    val context=LocalContext.current
    val scope=rememberCoroutineScope()
    val currentGranted by rememberUpdatedState(onGranted)
    var dialog by rememberSaveable {mutableStateOf(false)}
    var settings by rememberSaveable {mutableStateOf(false)}
    var requesting by rememberSaveable {mutableStateOf(false)}
    val permission=rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {granted->
        requesting=false;dialog=false
        if(granted)currentGranted()
    }
    LaunchedEffect(refresh) {
        if(settings) {settings=false;dialog=false;if(context.granted(Manifest.permission.RECEIVE_SMS))currentGranted()}
    }
    if(dialog)PneuDialog(
        onDismissRequest={dialog=false},dismissible=!requesting,
        title={Text(stringResource(R.string.sms_disclosure_title))},
        text={Text(stringResource(R.string.sms_disclosure_text))},
        confirmButton={PneuTextButton(onClick={
            if(PermissionRequests.settings(context as Activity,Manifest.permission.RECEIVE_SMS)) {
                settings=true;PermissionRequests.openSettings(context)
            } else if(!requesting) {
                requesting=true;PermissionRequests.mark(context,Manifest.permission.RECEIVE_SMS)
                permission.launch(Manifest.permission.RECEIVE_SMS)
            }
        }) {Text(stringResource(if(PermissionRequests.settings(context as Activity,Manifest.permission.RECEIVE_SMS))R.string.action_open_settings else R.string.action_continue))}},
        dismissButton={PneuTextButton(onClick={if(!requesting)dialog=false}) {Text(stringResource(R.string.cd_cancel))}}
    )
    return {if(context.granted(Manifest.permission.RECEIVE_SMS))scope.launch {currentGranted()} else dialog=true}
}
