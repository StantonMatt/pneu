package io.github.stantonmatt.pneu

import android.annotation.SuppressLint
import androidx.camera.core.*
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage
import java.util.concurrent.Executors

@SuppressLint("UnsafeOptInUsageError")
@Composable
fun PairCamera(modifier: Modifier, onCode: (String) -> Unit) {
    val context=LocalContext.current; val lifecycle=LocalLifecycleOwner.current
    val preview=remember { PreviewView(context) }
    val currentCallback by rememberUpdatedState(onCode)
    AndroidView(factory={ preview },modifier=modifier)
    DisposableEffect(lifecycle) {
        val executor=Executors.newSingleThreadExecutor()
        val scanner=BarcodeScanning.getClient(BarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build())
        val provider=ProcessCameraProvider.getInstance(context)
        val cameraPreview=Preview.Builder().build().apply { surfaceProvider=preview.surfaceProvider }
        val analysis=ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build()
        analysis.setAnalyzer(executor) { proxy ->
            val media=proxy.image
            if(media == null) proxy.close()
            else scanner.process(InputImage.fromMediaImage(media,proxy.imageInfo.rotationDegrees))
                .addOnSuccessListener { codes -> codes.firstOrNull()?.rawValue?.let(currentCallback) }
                .addOnCompleteListener { proxy.close() }
        }
        var disposed=false
        provider.addListener({
            if(!disposed) runCatching { provider.get().bindToLifecycle(lifecycle,CameraSelector.DEFAULT_BACK_CAMERA,cameraPreview,analysis) }
        },ContextCompat.getMainExecutor(context))
        onDispose {
            disposed=true; analysis.clearAnalyzer()
            if(provider.isDone) provider.get().unbind(cameraPreview,analysis)
            scanner.close(); executor.shutdown()
        }
    }
}
