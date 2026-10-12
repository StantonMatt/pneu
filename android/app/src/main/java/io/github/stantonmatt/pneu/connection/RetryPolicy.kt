package io.github.stantonmatt.pneu.connection

/** Persisted receipt retry policy; metadata and IDs never change across retries. */
object RetryPolicy {
    const val MAX_BACKOFF_MILLIS=72_000L // 60 seconds with the maximum 1.2 jitter
    data class Decision(val mismatches: Int, val waitMillis: Long)
    fun nack(code: String, previousMismatches: Int, jitter: Double): Decision {
        require(jitter in .8..1.2)
        return when(code) {
            "hash_mismatch","size_mismatch" -> {
                val count=previousMismatches+1
                Decision(count,if(count>3)Long.MAX_VALUE else 2000)
            }
            "unsupported_kind","id_conflict" -> Decision(previousMismatches,Long.MAX_VALUE)
            "storage_failed","disk_full","not_writable","clipboard_unavailable" -> Decision(previousMismatches,(60000*jitter).toLong())
            else -> error("Unknown NACK")
        }
    }
}
