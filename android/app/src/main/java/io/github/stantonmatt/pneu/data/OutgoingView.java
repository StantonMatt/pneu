package io.github.stantonmatt.pneu.data;

/** Shared by Room and the view migration; keep constants off reflected row classes. */
final class OutgoingView {
    static final String SQL="SELECT a.mediaId AS mediaId, a.transferId AS transferId, a.uri AS uri, a.path AS path, a.fileName AS fileName, a.mime AS mime, a.hash AS hash, a.kind AS kind, a.origin AS origin, a.text AS text, a.sensitive AS sensitive, a.automatic AS automatic, a.optimized AS optimized, a.orderId AS orderId, a.sourceHash AS sourceHash, a.size AS size, a.createdAt AS createdAt, a.hlc AS hlc, a.sourceDevice AS sourceDevice, EXISTS(SELECT 1 FROM deliveries used WHERE used.mediaId=a.mediaId AND used.attempted=1) AS payloadAttempted, d.peerId AS peerId, d.bindingGeneration AS bindingGeneration, d.sessionId AS sessionId, d.state AS state, d.error AS error, d.attempted AS attempted, d.revision AS revision, d.rejectedCapabilities AS rejectedCapabilities, d.sentBytes AS sentBytes, d.resumable AS resumable, d.nextAttempt AS nextAttempt, d.mismatches AS mismatches FROM artifacts a JOIN deliveries d ON a.mediaId=d.mediaId";
    private OutgoingView() {}
}
