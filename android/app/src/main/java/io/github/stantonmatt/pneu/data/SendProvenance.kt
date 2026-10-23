package io.github.stantonmatt.pneu.data

/** Admission-only provenance, carried through the picker and staging. A URI
 * without a journal/clipboard identity needs its actual staged hash first. */
data class SendProvenance(
    val received:Boolean=false,
    val source:String="",
    val sensitive:Boolean=false,
    val needsHash:Boolean=false,
)
