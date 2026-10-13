package io.github.stantonmatt.pneu.features

internal class BindingChangedException:IllegalStateException("Binding changed")

/** Owns unpublished capture bytes. Automatic publication rejection is ordinary
 * policy invalidation; manual rejection still reports a changed recipient.
 * Cleanup precedes the rescan so observers never see abandoned staging bytes. */
internal object StagedAdmission {
    suspend fun publish(automatic:Boolean,discard:()->Unit,rescan:()->Unit,body:suspend ()->Boolean):Boolean {
        var admitted=false
        try {admitted=body()} finally {if(!admitted)discard()}
        if(!admitted) {
            if(!automatic)throw BindingChangedException()
            rescan()
        }
        return admitted
    }
}
