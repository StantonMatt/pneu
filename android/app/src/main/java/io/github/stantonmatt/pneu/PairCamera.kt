package io.github.stantonmatt.pneu

import androidx.camera.core.*
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import java.util.concurrent.Executors

@Composable
fun PairCamera(modifier: Modifier, onCode: (String) -> Unit) {
    val context=LocalContext.current; val lifecycle=LocalLifecycleOwner.current
    val preview=remember { PreviewView(context) }
    val currentCallback by rememberUpdatedState(onCode)
    AndroidView(factory={ preview },modifier=modifier)
    DisposableEffect(lifecycle) {
        val executor=Executors.newSingleThreadExecutor()
        val mainExecutor=ContextCompat.getMainExecutor(context)
        var disposed=false
        val provider=ProcessCameraProvider.getInstance(context)
        val cameraPreview=Preview.Builder().build().apply { surfaceProvider=preview.surfaceProvider }
        val analysis=ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build()
        analysis.setAnalyzer(executor) { proxy ->
            val code=try {
                val plane=proxy.planes[0]; val buffer=plane.buffer.duplicate()
                val luminance=ByteArray(buffer.remaining()).also { buffer.get(it) }
                decodePairQr(luminance,proxy.width,proxy.height,plane.rowStride)
            } finally { proxy.close() }
            code?.let { mainExecutor.execute { if(!disposed)currentCallback(it) } }
        }
        provider.addListener({
            if(!disposed) runCatching { provider.get().bindToLifecycle(lifecycle,CameraSelector.DEFAULT_BACK_CAMERA,cameraPreview,analysis) }
        },mainExecutor)
        onDispose {
            disposed=true; analysis.clearAnalyzer()
            if(provider.isDone) provider.get().unbind(cameraPreview,analysis)
            executor.shutdown()
        }
    }
}
