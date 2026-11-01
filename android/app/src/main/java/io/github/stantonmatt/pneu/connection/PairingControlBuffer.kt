package io.github.stantonmatt.pneu.connection

import io.github.stantonmatt.pneu.protocol.*

/** A known acceptor may already be READY while the dialer's frozen attempt is
 * recovering PAIR_RESULT. Retain only bounded control state, never apply it. */
internal class PairingControlBuffer(private val setup:Boolean) {
    private val frames=ArrayDeque<Frame.Control>()
    fun hold(frame:Frame.Control) {
        if(frame.type==Type.MEMBERS) {
            require(setup && frames.none {it.type==Type.MEMBERS})
            require(frame.body.toString().toByteArray().size<=65536)
        } else {
            require(frame.type in setOf(Type.CLIPBOARD,Type.STATUS,Type.PEER_SETTINGS))
            require(frames.count {it.type!=Type.MEMBERS}<4)
        }
        frames.add(frame)
    }
    fun afterInstall(bindingPersisted:Boolean):List<Frame.Control> {
        check(bindingPersisted) {"Binding must be persisted before buffered controls"}
        return frames.toList().also {frames.clear()}
    }
}
