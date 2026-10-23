package io.github.stantonmatt.pneu.features

import java.util.UUID
import io.github.stantonmatt.pneu.connection.PairingIssue
import io.github.stantonmatt.pneu.connection.ConnectionStatus

/** Copy is payload, never identity: two identical rejections are two events. */
data class UiFeedback(val message:String,val id:String=UUID.randomUUID().toString(),val status:ConnectionStatus?=null) {
    val issue:PairingIssue=when(status) {null->PairingIssue.fromMessage(message);ConnectionStatus.UNREACHABLE->PairingIssue.UNREACHABLE;else->PairingIssue.FEEDBACK}
}
