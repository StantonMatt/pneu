package io.github.stantonmatt.pneu.features

object ClipboardFeedback {
    fun result(state:String?)=when(state) {
        "sent"->"Clipboard sent"
        "superseded"->"Replaced by newer clipboard"
        "discarded",null->"Clipboard send cancelled"
        else->"Delivery unconfirmed. Still pending."
    }
}
