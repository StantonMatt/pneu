package io.github.stantonmatt.pneu.features

import io.github.stantonmatt.pneu.R
import io.github.stantonmatt.pneu.data.Outgoing

object ClipboardFeedback {
    fun result(row:Outgoing?)=when(row?.state) {
        "sent"->R.string.toast_clipboard_sent
        "superseded"->R.string.toast_clipboard_replaced
        "waiting","sending","failed"->if(row.nextAttempt==Long.MAX_VALUE) R.string.toast_clipboard_failed else R.string.toast_clipboard_sending
        else->R.string.toast_clipboard_cancelled
    }
}
