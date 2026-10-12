package io.github.stantonmatt.pneu.features

import androidx.compose.runtime.staticCompositionLocalOf
import kotlinx.coroutines.CancellationException

/** UI mutations report persistence failures without optimistic navigation/state. */
class UiActions(private val failure:(String,(() -> Unit)?)->Unit) {
    companion object {
        fun feedback(context:android.content.Context)=UiActions {message,_->
            android.os.Handler(android.os.Looper.getMainLooper()).post {android.widget.Toast.makeText(context,message,android.widget.Toast.LENGTH_LONG).show()}
        }
    }
    suspend fun run(message:String,retry:(()->Unit)?=null,body:suspend ()->Unit):Boolean = try {
        body();true
    } catch(e:Exception) {
        if(e is CancellationException)throw e
        // Class only: provider/database messages can contain user content or paths.
        runCatching {android.util.Log.w("PneuUiAction",e.javaClass.simpleName)}
        failure(message,retry);false
    }
}
val LocalUiActions=staticCompositionLocalOf<UiActions> {error("Missing UI action boundary")}
