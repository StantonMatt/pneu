package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.data.Outgoing

object ClipboardFeedback {
    fun result(row:Outgoing?)=when(row?.state) {
        "sent"->"Clipboard sent"
        "superseded"->"Replaced by newer clipboard"
        "waiting","sending","failed"->if(row.nextAttempt==Long.MAX_VALUE) "Couldn't send clipboard" else "Still sending clipboard"
        else->"Clipboard send cancelled"
    }
}
