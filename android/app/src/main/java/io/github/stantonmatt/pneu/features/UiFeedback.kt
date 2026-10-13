package io.github.stantonmatt.pneu.features

import java.util.UUID

/** Copy is payload, never identity: two identical rejections are two events. */
data class UiFeedback(val message:String,val id:String=UUID.randomUUID().toString())
