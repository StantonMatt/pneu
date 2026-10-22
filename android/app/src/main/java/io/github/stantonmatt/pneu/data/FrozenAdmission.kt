package io.github.stantonmatt.pneu.data

/** Captured before focus, clipboard access, target selection or provider calls. */
internal data class FrozenAdmission(val binding:DeliveryBinding?,val text:List<DeliveryBinding>,val images:List<DeliveryBinding>,val files:List<DeliveryBinding>) {
    fun targets(kind:String)=when(kind) {"text"->text;"clipboard_image"->images;else->files}
}
