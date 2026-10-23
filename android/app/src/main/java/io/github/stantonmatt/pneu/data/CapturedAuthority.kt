package io.github.stantonmatt.pneu.data

/** Authority captured before a user/provider wait. A null connection generation
 * deliberately scopes durable file admission to the pairing; a non-null token
 * scopes a deferred effect to its originating connection, even after disconnect.
 * Checking current state can only remove authority, never replace the binding. */
data class CapturedAuthority(val binding:DeliveryBinding,val connectionGeneration:String?=binding.sessionId) {
    fun stillValid(p:Prefs,currentConnectionGeneration:String?)=binding.matches(p) &&
        (connectionGeneration==null || connectionGeneration==currentConnectionGeneration)
    fun saved()=listOf(binding.phoneId,binding.pin,binding.pcId,binding.generation.toString(),binding.sessionId.orEmpty(),connectionGeneration.orEmpty())
    companion object {
        fun restore(values:List<String>):CapturedAuthority?=runCatching {
            require(values.size==6)
            CapturedAuthority(DeliveryBinding(values[0],values[1],values[2],values[3].toLong(),values[4].ifEmpty {null}),values[5].ifEmpty {null})
        }.getOrNull()
    }
}
