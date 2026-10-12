# Pneu wire protocol v1

This document is normative for independent Kotlin and Rust implementations. MUST,
SHOULD and MAY have their usual RFC 2119 meanings. The protocol crate implements
syntax and framing; certificate verification, clocks, retry queues, durable
storage and these state machines belong to the applications.

## Transport and discovery

The Android phone is always the TCP client; the PC listens on TCP port **47821**
by default. A single bidirectional, long-lived TLS connection carries all frames.
Disable Nagle (`TCP_NODELAY`); do not buffer a whole file before transmitting it.
Use bounded buffering, stream hashing and
backpressure. Control frames SHOULD be scheduled between chunks. Concurrent
writers MUST serialize entire frames; never interleave their bytes.

TLS **1.3 only**, mutual authentication, no plaintext fallback. Both endpoints
use self-signed certificates with ECDSA **secp256r1 / P-256** keys (not Ed25519).
Use the TLS 1.3 `ecdsa_secp256r1_sha256` signature scheme and normal TLS 1.3 AEAD
cipher suites. Each endpoint presents one leaf certificate. Its fingerprint is
SHA-256 of the exact DER certificate bytes, represented as 64 lowercase hex
characters without colons. Pin the whole certificate, not SPKI. Renewing or
replacing a certificate requires re-pairing. Keep private keys in platform secure
storage and restrict PC key/config permissions to their owner.

No CA-chain or DNS-hostname validation is performed: addresses are routing hints,
and the certificate fingerprint is the identity. Phones MUST verify the expected
PC pin; PCs check client fingerprint admission immediately after TLS. Both MUST
verify the P-256 public key, certificate self-signature, current certificate
validity dates, and the TLS CertificateVerify signature. A custom Android TrustManager or rustls
verifier MUST NOT bypass CertificateVerify. Disable TLS 0-RTT and session
resumption in v1 so every connection presents and checks both certificates.
An absent certificate or a mismatching PC pin fails TLS before protocol traffic.
The PC completes TLS for any client certificate that passes all cryptographic and
structural checks. Immediately after TLS, before reading any protocol frame, it
checks the client fingerprint. Unless it is paired or admissible under an active
pairing invitation and admission quotas, the PC sends `ERROR unauthorized` with a
short message and closes within the 2 s fatal-close bound. Unknown clients count
toward the same total connection and handshake limits. During pairing only,
admitted unknown clients enter the restricted provisional state specified below.
Phones treat `ERROR unauthorized` from a PC authenticated against the stored pin
as a revocation signal ("Pairing lost"), rather than a network failure.

PC advertises mDNS service `_pneu._tcp.local.` (service type `_pneu._tcp`), SRV
port and TXT keys `v=1`, `fp=<PC fingerprint>`. Instance name is for display only.
Phone tries: (1) last-good LAN endpoint, (2) current mDNS endpoints matching the
stored fingerprint/version, (3) paired Tailscale endpoints. QR LAN candidates
bootstrap the LAN cache. Preserve candidate order within a category, trying each
once before the next category. mDNS TXT is untrusted and cannot change a pin.
Tailscale `100.64.0.0/10` addresses, including the planned `100.67.144.28`, route
through the phone's Tailscale app. LAN addresses can be IPv4, IPv6 or DNS names.
PC selects QR and mDNS addresses from the same prioritized shortlist: Tailscale
IPv4, interfaces carrying a default route, then other physical LAN interfaces.
Exclude Docker/bridge/veth/virbr, loopback, unspecified, multicast and link-local
addresses before applying the eight-address limit. DNS A/AAAA records have no
ordering guarantee; priority governs the selected shortlist. Discovery failure
MUST NOT prevent the TCP daemon from running.

Per endpoint, TCP connect timeout is 5 s and TLS handshake timeout is 10 s. After
all candidates fail, retry after 2, 4, 8, 16, 32, then 60 s, with uniform ±20%
jitter. Network changes and user-initiated sends MAY trigger an immediate retry.
Never run parallel connection races in v1. A PC pairs with one phone at a time;
a successful new pairing atomically forgets the old phone's pin, binding, dedupe
and pending text, then accepts the new phone. Saved files remain. Only one active
connection is allowed: the newest authenticated connection wins, closes the old
one with `ERROR connection_replaced`, and suppresses old connection events and
pending desktop actions.
An ordinary disconnect does not revoke completed-transfer desktop actions or
admitted clipboard text history; these remain allowed unless the pairing is
revoked or a newer connection is admitted.
While unpairing, PC refuses new connection registrations. Work
admitted under an older pairing MUST NOT mutate the replacement pairing's state
or initiate desktop actions after replacement/unpair completes, even if the
device ID is reused. A failed lifecycle transaction MUST NOT announce successful
pairing or unpairing; report the failure, retain the durable binding and permit
reconnect. The old connection may be closed. Completed files remain untouched.

## Pairing QR URI

Canonical example (shown on one line in the vectors):

```text
pneu://pair?v=1&name=Office%20PC&fp=<64-lowercase-hex>&addr=192.168.1.10%3A47821&addr=100.67.144.28%3A47821&token=<64-lowercase-hex>&exp=1800000300000
```

Scheme MUST be `pneu`, authority MUST be `pair`; no userinfo, port, path, fragment
or trailing `/`. URI length is at most 8192 UTF-8 bytes. Query fields:

| Key | Cardinality | Meaning |
| --- | --- | --- |
| `v` | exactly 1 | Decimal integer protocol version, currently `1` |
| `name` | exactly 1 | PC display name, 1–128 UTF-8 bytes, no U+0000 |
| `fp` | exactly 1 | PC DER certificate SHA-256, 64 lowercase hex |
| `addr` | 1–8 | Ordered candidate endpoint, `host:port` or `[IPv6]:port` |
| `token` | exactly 1 | Cryptographically random 256-bit secret, 64 lowercase hex |
| `exp` | exactly 1 | Token expiry, Unix UTC milliseconds |

Encode each key/value with UTF-8 percent encoding; canonical spaces are `%20`.
Parsers also accept `+` as a space for form-query compatibility. Reject malformed
percent escapes, invalid UTF-8, unknown keys and duplicate singleton keys.
Addresses have ASCII IPv4, DNS names (labels 1–63 characters, ASCII alphanumeric
with interior hyphens, total at most 253 characters), or bracketed IPv6 literals;
ports are decimal 1–65535. All-numeric/dotted-numeric hosts must parse as IPv4
literals; they are not treated as DNS names. No zones, paths, schemes, credentials
or whitespace.
Do not resolve DNS while parsing. Repeated identical `addr` values are invalid.
Version/expiry parsing is syntactic; the application MUST reject unsupported
versions and locally expired brand-new invitations before dialing. A persisted
pending pairing attempt is recovery, not a brand-new invitation, and MUST be
retried even after its original `exp` has passed. PC's clock is decisive
for token expiry; explain a clock disagreement to the user.

PC generates a token with an OS CSPRNG, keeps it only in memory, and sets `exp`
to creation time + **300000 ms**. Closing the pairing window, expiry, PC restart,
or first successful use invalidates it. A new invitation invalidates the old
one. Do not log tokens or QR URIs. QR is the trust channel for the PC pin; users
must scan the actual PC's screen. Discovery never supplies a trusted replacement
fingerprint.

## Common types and JSON rules

JSON control payloads are UTF-8 objects without BOM. Required fields listed
below MUST be present; optional fields MUST be omitted when absent (not `null`).
Unknown object fields and unknown HELLO capability strings are ignored. Reject
duplicate known fields, wrong types, invalid values, invalid UTF-8, trailing
non-whitespace, and non-object payloads. JSON field ordering/whitespace is not
significant. Strings have their byte limits after JSON unescaping, in UTF-8.
Known integer fields MUST use JSON integer tokens (decimal digits, with no
fraction or exponent). For example `1.0` and `1e0` are invalid even though JSON
Schema treats their numeric values as integers. No JSON compression, file
compression or base64 layer exists.

* `Id`: exactly 32 lowercase hex digits, representing 16 random bytes. Device IDs
  persist across launches; transfer IDs are freshly generated with 128 bits of
  CSPRNG entropy per logical operation, then persist until ACK. Reinstallation
  generates a new device ID and requires pairing. The TLS pin is bound to one
  device ID at pairing; a different HELLO ID for a known pin is unauthorized.
* `Hash`: exactly 64 lowercase hex digits, SHA-256 of original file bytes.
* `Timestamp`: integer Unix UTC milliseconds in `0..9007199254740991` (JSON's
  exactly representable integer range). Use a wall clock for metadata, a monotonic
  clock for timers; timestamps are never trusted for authorization. Clipboard
  text uses `created_at` for last-writer-wins ordering as specified below.
* `Nonce`: exactly 16 lowercase hex digits (8 random bytes). It is a string to
  avoid unsigned 64-bit JSON/Kotlin interoperability problems.
* `Kind`: `screenshot`, `photo`, `recording`, `file`, `clipboard_image`. `file` is anything sent
  through the share sheet that is not text.
* No known protocol string field may contain U+0000. Empty clipboard text is valid and clears the
  clipboard. Filenames are 1–255 UTF-8 bytes, never `.` or `..`, and contain no
  `/` or `\`. Receiver MUST choose a safe local basename, reject path traversal,
  and avoid overwrite/symlink attacks regardless of what the sender supplies.

## Frame layout and limits

Every frame starts with an unsigned 32-bit **big-endian** length `L`, followed by
exactly `L` bytes. `L` includes the type byte, excludes the four-byte prefix.
There are no delimiters, padding, flags, checksums or alignment requirements.
TCP/TLS reads may split or combine frames arbitrarily.

| Absolute byte offset | Size | Value |
| --- | --- | --- |
| 0 | 4 | `L`, u32 big-endian |
| 4 | 1 | Message type |
| 5 | `L - 1` | Payload |

* Control JSON payload: at most **65536 bytes** (so control `L <= 65537`).
* Chunk data: **1..262144 bytes** (256 KiB); all but the final chunk SHOULD be
  256 KiB. Small chunks are permitted. Empty files use zero chunk frames.
* Maximum frame `L`: **262169** = type (1) + transfer ID (16) + offset (8) +
  chunk data (262144). Maximum complete wire frame: **262173 bytes**.
* `L=0`, oversized frames, unknown type bytes and malformed payloads are fatal.
  Check the length before allocation. Never allocate based on the file's size.
* File size: **0..68719476736 bytes** (64 GiB). MIME: 1–255 UTF-8 bytes.
* Clipboard text: at most **16384 UTF-8 bytes**, additionally subject to encoded
  JSON payload limit (escaping can increase size).
* `source_media_id`: optional nonempty string, at most 256 UTF-8 bytes.
* Exactly one outgoing and one incoming file transfer may be in progress per
  connection. Control traffic, including clipboard text, may interleave. Wait
  for ACK/NACK before the next outgoing FILE_BEGIN. At most one unacknowledged
  clipboard text operation per direction (wait for ACK/NACK before sending the
  next; newer unsent text replaces older unsent text). FILE_CHUNK belongs to its direction's
  active file transfer, never to another connection or device.

256 KiB amortizes framing/TLS/syscall costs for photos and recordings while
keeping memory bounded and allowing frequent control scheduling; per chunk wire
overhead is 29 bytes, about 0.011%. It does not force TLS records to be that size.
Read/write deadlines belong to the caller, not the protocol codec. An I/O error,
truncated frame, canceled mid-frame read/write or deadline while in a frame makes
that connection unusable: close it and reconnect, never continue with that stream.

## Message registry and payload schemas

All messages except FILE_CHUNK have JSON payloads. Type names in vector `decoded`
objects are lowercase snake_case; these names are not carried on the wire.

| Hex ID | Name | Purpose |
| --- | --- | --- |
| `01` | HELLO | Identity/version/capabilities |
| `02` | PAIR_REQUEST | Phone presents one-time secret |
| `03` | PAIR_RESULT | PC accepts/rejects pairing |
| `04` | PING | Keepalive challenge |
| `05` | PONG | Echo challenge |
| `06` | STATUS | Sender state changed (phone pause) |
| `07` | UNPAIR | Sender forgot this peer |
| `10` | FILE_BEGIN | File metadata, opens receive slot |
| `11` | FILE_CHUNK | Raw binary file bytes |
| `12` | FILE_END | Sender finished bytes |
| `13` | ACK | Durable verified receipt |
| `14` | NACK | Receipt failed |
| `20` | CLIPBOARD | Clipboard text operation |
| `7f` | ERROR | Fatal error or graceful close; then close TLS |

The following JSON Schema (2020-12) defines the control payloads. Select the
matching `$defs` entry by message type. `additionalProperties` is allowed for
forward-compatible unknown fields. The common rules and cross-field constraints
in this document also apply (JSON Schema alone cannot enforce UTF-8 byte limits,
unique known fields or filename safety).

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$defs": {
    "id": {"type":"string","pattern":"^[0-9a-f]{32}$"},
    "hash": {"type":"string","pattern":"^[0-9a-f]{64}$"},
    "timestamp": {"type":"integer","minimum":0,"maximum":9007199254740991},
    "nonce": {"type":"string","pattern":"^[0-9a-f]{16}$"},
    "hello": {
      "type":"object","required":["version","device_id","name","capabilities"],
      "properties": {
        "version":{"type":"integer","minimum":1,"maximum":4294967295},
        "device_id":{"$ref":"#/$defs/id"},
        "name":{"type":"string","minLength":1},
        "capabilities":{"type":"array","maxItems":16,"uniqueItems":true,"items":{"type":"string","minLength":1}},
        "paused":{"type":"boolean"}
      }
    },
    "status": {
      "type":"object","required":["paused"],
      "properties":{"paused":{"type":"boolean"}}
    },
    "unpair": {"type":"object"},
    "pair_request": {
      "type":"object","required":["token"],
      "properties":{"token":{"$ref":"#/$defs/hash"}}
    },
    "pair_result": {
      "type":"object","required":["accepted"],
      "properties": {
        "accepted":{"type":"boolean"},
        "code":{"enum":["invalid_token","expired_token","pairing_closed","already_paired","storage_failed"]},
        "message":{"type":"string"}
      },
      "if":{"properties":{"accepted":{"const":false}}},
      "then":{"required":["code"]},
      "else":{"not":{"anyOf":[{"required":["code"]},{"required":["message"]}]}}
    },
    "ping": {
      "type":"object","required":["nonce"],
      "properties":{"nonce":{"$ref":"#/$defs/nonce"}}
    },
    "pong": {"$ref":"#/$defs/ping"},
    "file_begin": {
      "type":"object","required":["transfer_id","file_name","mime","size","sha256","kind","created_at"],
      "properties": {
        "transfer_id":{"$ref":"#/$defs/id"},
        "file_name":{"type":"string","minLength":1},
        "mime":{"type":"string","minLength":1},
        "size":{"type":"integer","minimum":0,"maximum":68719476736},
        "sha256":{"$ref":"#/$defs/hash"},
        "kind":{"enum":["screenshot","photo","recording","file","clipboard_image"]},
        "created_at":{"$ref":"#/$defs/timestamp"},
        "source_media_id":{"type":"string","minLength":1}
      }
    },
    "file_end": {
      "type":"object","required":["transfer_id"],
      "properties":{"transfer_id":{"$ref":"#/$defs/id"}}
    },
    "ack": {
      "type":"object","required":["transfer_id","deduplicated"],
      "properties":{"transfer_id":{"$ref":"#/$defs/id"},"deduplicated":{"type":"boolean"}}
    },
    "nack": {
      "type":"object","required":["transfer_id","code","retryable"],
      "properties": {
        "transfer_id":{"$ref":"#/$defs/id"},
        "code":{"enum":["hash_mismatch","size_mismatch","storage_failed","disk_full","not_writable","unsupported_kind","id_conflict","clipboard_unavailable"]},
        "retryable":{"type":"boolean"},
        "message":{"type":"string"}
      }
    },
    "clipboard": {
      "type":"object","required":["transfer_id","text","created_at"],
      "properties":{"transfer_id":{"$ref":"#/$defs/id"},"text":{"type":"string"},"created_at":{"$ref":"#/$defs/timestamp"},"origin":{"enum":["clipboard","share"]}}
    },
    "error": {
      "type":"object","required":["code"],
      "properties": {
        "code":{"enum":["version_mismatch","unauthorized","unexpected_message","malformed_message","frame_too_large","unknown_message","timeout","busy","connection_replaced","closing"]},
        "message":{"type":"string"},
        "supported_version":{"type":"integer","minimum":1,"maximum":4294967295}
      }
    }
  }
}
```

HELLO name is 1–128 UTF-8 bytes and is the device's display name shown by the
other side. HELLO `paused` (phone only, default false when omitted) reports
whether the phone's automatic sending is paused.

STATUS (`06`) carries the sender's current `paused` state; the phone sends it in
READY whenever the user pauses or resumes. It has no receipt. A PC MAY send it
with `paused=false` but v1 PCs never do.

UNPAIR (`07`), payload `{}`, means the sender has deleted its pairing with the
receiver. Receiver deletes the peer's pin, device binding and dedupe records,
shows the unpaired state only after durable removal succeeds, and closes the
connection; the sender sends UNPAIR only after its own durable removal succeeds,
then closes too
(best-effort `ERROR closing` first). Either side may send it in READY. If the peer
is offline, it learns of the unpairing on its next connection attempt (pin
mismatch or `unauthorized`).

CLIPBOARD `origin` (default `clipboard` when omitted) is `share` when the text or
link came from the share sheet rather than the clipboard; receivers treat both
the same except for labelling.

`disk_full` and `not_writable` NACKs mean the receiver could not store the file
in its configured folder. The receiver does not keep a copy; the sender retries. Each capability is 1–64 UTF-8 bytes; at most 16
unique entries. Defined capabilities are `files`, `clipboard_text`,
`clipboard_image`. Advertising `clipboard_image` requires `files`. Capabilities
mean ability/willingness to receive that feature, not permission to bypass local
settings. Send a feature only if the receiving HELLO advertises it. Both sides
always implement ACK/NACK, HELLO, ERROR and keepalive. Unknown capabilities are
ignored; local policy MAY NACK an advertised feature (for example after disabling
clipboard in settings). `message` strings in PAIR_RESULT/NACK/ERROR are optional
human-readable diagnostics, at most 512 UTF-8 bytes; never branch on them.
`version_mismatch` ERROR MUST include `supported_version`; other ERROR codes MUST
omit it. Future HELLO versions must retain the v1 HELLO envelope for negotiation.

FILE_CHUNK payload is binary, with these **absolute frame offsets**:

| Offset | Size | Meaning |
| --- | --- | --- |
| 0 | 4 | `L = 25 + data_length` |
| 4 | 1 | `0x11` |
| 5 | 16 | Transfer ID bytes, decoded from FILE_BEGIN's 32 hex digits |
| 21 | 8 | Unsigned u64 big-endian byte offset into original file |
| 29 | 1–262144 | Raw bytes, no encoding |

Offset starts at zero and equals bytes already received for this transfer.
Offsets are at most 64 GiB. Non-contiguous offsets, wrong IDs or chunk data beyond
declared size are fatal `unexpected_message` violations. FILE_END carries only
the transfer ID; the final byte count and digest are already declared in BEGIN.
An END with fewer bytes produces `NACK size_mismatch`, not an ACK.

CLIPBOARD represents text (including links). Images use FILE_BEGIN with
`kind=clipboard_image`, a MIME beginning with `image/` and a nonempty subtype,
and ordinary FILE_CHUNK/FILE_END/ACK. ACK for
an image means the verified file and dedupe record are durable, even if setting
the system clipboard is optional or fails afterwards. A text ACK means the text
was applied and its bounded dedupe history committed, or it was stale/duplicate.
OS denial produces `clipboard_unavailable` and retains no pending text. Clipboard
application is a best-effort side effect, not an exactly-once OS guarantee.
Suppress clipboard watchers for received operations; never echo them with new IDs.

## Connection and pairing state machines

1. **DISCONNECTED → TCP → TLS.** Phone authenticates PC against QR/stored pin.
   PC requires a cryptographically valid client certificate and completes TLS
   before checking fingerprint admission. An unknown fingerprint outside an
   active pairing window, or denied by admission quotas, receives
   `ERROR unauthorized` and closes before PC reads any frame. During a pairing window,
   admitted unknown clients may proceed only into a restricted provisional state.
   Limit provisional connections to one and rate-limit admission/token failures
   locally; total connection and handshake limits also cover unknown clients.
2. **HELLO exchange.** Phone sends HELLO first within 10 s of TLS completion. PC
   replies with HELLO within 10 s of receiving it. No other message is allowed
   before both HELLOs except ERROR. Each endpoint sends HELLO exactly once.
   A version other than 1 yields `ERROR version_mismatch` with
   `supported_version=1`, then close; no downgrade and no queued work is lost.
3. **Known client → READY.** PC verifies the HELLO device ID matches its stored
   certificate binding. Phone verifies PC's HELLO ID matches the stored binding
   after the first pairing. Failure is `unauthorized`. Neither side sends
   PAIR_REQUEST/PAIR_RESULT for a normal authenticated reconnect.
4. **Unknown client → PAIRING.** Only phone may send PAIR_REQUEST and only PC may
   send PAIR_RESULT. Request must arrive within 10 s after HELLO exchange; no
   application frames or keepalives are allowed in this restricted state. Phone
   waits at most 10 s for the result. PC checks window/expiry and compares the
   secret in constant time. PC binds the TLS-presented fingerprint to HELLO
   device ID and durably stores it, then atomically consumes the invitation
   before returning `{"accepted":true}`. Storage failure rejects without
   consuming the token; never report success before persistence. A valid invitation
   replaces the previous phone, including when its device ID is unchanged but
   its certificate changed. Concurrent attempts cannot both consume an invitation.
   `already_paired` remains a recognized v1 code for compatibility, but the PC
   does not use it to reject replacement pairing.
5. **PAIR_RESULT accepted → READY.** Phone durably stores PC's pin, HELLO device
   ID and addresses. PC may send application data immediately after success;
   phone processes it only once its own pairing state is persisted. Rejection
   sends accepted=false plus code, then closes TLS. PC checks failures in order:
   closed window, expiry, token mismatch, storage.
   Lost success response is recoverable: phone reconnects with the same cert
   and repeats PAIR_REQUEST from its pending record as specified below. The
   already-bound PC returns accepted=true without requiring a new token.
6. **READY → CLOSED.** Either endpoint may send ERROR, then orderly TLS close;
   peer does not reply with ERROR. ERROR is legal in every post-TLS state. On
   fatal decode/I/O failures, best-effort ERROR is optional; never wait to drain
   an attacker-supplied length. Close within 2 s. EOF without ERROR is normal
   network loss and triggers reconnect for pending work.

Phone cannot distinguish known/unknown admission from HELLO alone. Before its
first PAIR_REQUEST, the phone MUST durably save a pending pairing record with the
PC fingerprint, ordered addresses and token (and the invitation metadata). If
PAIR_RESULT is lost, it MUST keep that record and reconnect using that pin and
send PAIR_REQUEST again, even after the invitation's `exp`. Only a brand-new
invitation is subject to the local expiry check. A rejected result ends that
attempt; accepted=true durably installs the PC binding and clears the pending
record. The old binding remains until a new pairing succeeds.

For a certificate already bound to the same HELLO device ID, the PC MUST answer
PAIR_REQUEST with PAIR_RESULT accepted=true, without rechecking the token, expiry
or pairing window, then enter (or remain in) READY. This is an idempotent recovery
exception, also legal after a normal known-client HELLO; a second PAIR_REQUEST on
the same connection is unexpected_message. A mismatched device ID remains
unauthorized. A paired phone without a pending attempt sends no PAIR_REQUEST.
No duplicate HELLO is required. Application frames arriving during recovery must
wait until the phone's binding has been persisted.

## Transfer and delivery state machines

File sender persists its queue entry, ID, exact bytes and metadata before sending.
Clipboard text instead retains only the latest pending value as described below.
It may hash while preparing the queue but must know size/SHA-256 before BEGIN.
Changing bytes (including compression), filename, kind, timestamps or other
metadata is a new logical transfer with a new ID. Retrying preserves everything.

**Sender:** QUEUED → FILE_BEGIN → contiguous FILE_CHUNK frames → FILE_END →
WAIT_RECEIPT → ACK removes queue entry; NACK retains/marks it as specified below.
Empty files go directly BEGIN → END with SHA-256 of empty bytes. ACK/NACK may be
sent for CLIPBOARD immediately; for files they are sent only after END, so the
sender can pipeline chunks without a per-chunk RTT. Matching ACK/NACK can
interleave with reverse-direction traffic. An unsolicited receipt (no matching
pending operation on that connection) is fatal `unexpected_message`.

**Receiver:** IDLE → BEGIN (reserve slot, inspect dedupe/policy) → RECEIVING or
DRAINING → END → VERIFY → COMMIT → ACK/NACK → IDLE. Hash incrementally and write
to an owner-only temporary file in the destination filesystem. Check declared
size, final count and digest while receiving, so FILE_END processing does not
require another pass over the complete file. Commit in this order: fsync staging
file contents, rename-noreplace into the destination folder, fsync that folder and newly created
ancestors, insert the `(authenticated sender device_id, transfer_id)` completion
receipt and metadata in one durable database transaction, then ACK. Completion
receipt commits MUST survive crashes and power loss before ACK. On save error,
delete staging and NACK; never keep staging for recovery. Staging lives in a
private disposable `.pneu-staging` directory per destination folder. Persist the
list of ever-used staging folders before creating a stage and wipe those staging
directories at startup, independently of the current save configuration. Cleanup
of unavailable/read-only historical folders is best-effort: log and defer errors,
retry on first use or a later startup, and do not block daemon startup. Before admitting a new transfer to
a deferred folder, cleanup MUST succeed; never sweep a root holding live stages.
Cleanup MUST NOT chmod existing directories. A save path change affects new transfers only; existing transfers retain their folder
handles and configuration. Never chmod a user-selected folder.

There are no rollback intents, persisted inode identities, or recovery of placed
files. **Accepted trade-off:** a crash (or receipt persistence failure) in the
short window between rename and receipt commit can leave a visible file without
a receipt; replay can create a second visible copy. Never delete an uncertain
user file to repair this window. An ACK still means file placement and receipt
are durable. File display and notifications happen only for newly completed
operations, not duplicates; optional desktop action failure does not undo ACK.

**Accepted behaviours:**

- Changing the PC listening port requires restarting pneud.
- Lifecycle persistence failures are reported; durable state stays authoritative.
  The user retries or restarts, and the old connection may be closed.
- If the best-effort UNPAIR frame cannot be sent, the phone learns of the unpair
  on its next connection attempt.
- HEIF/HEIC and AVIF are saved as files, not copied as clipboard images; explicit
  clipboard-image requests for these formats report an unsupported action.
- Automatic desktop actions are best-effort and coalesced to the latest pending
  image; bounded notifications may be dropped. Explicit clipboard-image requests
  are kept separately in a bounded queue; overflow reports a copy failure and
  a best-effort notification without undoing the saved-file ACK. Optional actions
  never backpressure file delivery.
- A crash between final rename and receipt commit may duplicate a visible file
  on replay, as described above.
- OS desktop actions already submitted before an unpair may still become visible.

A completed duplicate with identical known metadata fields (semantic equality of
decoded values, ignoring JSON order/whitespace and unknown fields) consumes chunks in DRAINING mode,
validating ID/offset/count but without rewriting or replaying side effects, then
ACKs with `deduplicated=true` at END. Stored digest is authoritative; it need not
rehash discarded bytes. New success returns `deduplicated=false`. Reusing the
same ID for different metadata (or text vs file) drains and returns `id_conflict`.
Transient storage failures or unsupported local policy likewise drain to END,
then NACK. Still enforce all framing/size/offset limits while draining. Retain
this small drain state even if a disk write failed; sender finishes normally.
Do not accept a second FILE_BEGIN until the first END and receipt are processed.

File completion IDs MUST be retained durably for as long as the device remains paired;
v1 has no garbage-collection horizon. Explicit unpairing clears trust/dedupe and
requires a fresh device identity or user-approved fresh transfer IDs. Capacity
policy may reject new transfers with storage_failed instead of evicting dedupe
records. `source_media_id` is an opaque, optional sender-local media identity
(e.g. `external_primary:images:12345`). The sender persists its own media-ID →
logical transfer mapping to avoid enqueueing the same MediaStore event twice.
The authoritative receiver key is always `(device_id, transfer_id)`; neither
media IDs nor SHA alone collapse intentional sends of the same content.

**Disconnect at any stage:** close the stream, delete incomplete staging files,
retain completed dedupe records, release active slots. Sender retains unACKed
operations and retries the entire file from offset zero on the new connection;
there is no resume query/offset negotiation. A lost ACK yields a complete replay
and deduplicated ACK. Serialize file placement/receipt transactions against
unpair and other commits so revoked work cannot write into a replacement pairing.
Obsolete connections cannot publish progress, status or desktop actions.

Text CLIPBOARD shares the transfer-ID namespace but is **last-writer-wins**,
not a durable FIFO. Receiver keeps one last-applied `created_at` timestamp per
peer and a bounded history of the 64 most recently applied text IDs with SHA-256,
timestamp and origin for metadata equality. It stores no clipboard plaintext.
An identical recent replay ACKs `deduplicated=true`; a conflicting retained ID
NACKs `id_conflict`. A new ID with `created_at` less than or equal to the last
applied timestamp ACKs `deduplicated=true` without applying. Otherwise reserve
that ID atomically against file commits before any clipboard effect, apply the
text, finalize timestamp/recent-ID history, then ACK `deduplicated=false`. A
failed or cancelled effect releases the reservation; a crash discards unfinished
reservations at startup. A reservation is not an applied receipt and cannot yield
a deduplicated ACK until finalized. The first
text is eligible even if its timestamp is zero. Equal timestamps keep the first
applied value. Senders SHOULD assign increasing timestamps to successive local
changes. File receipts retain their IDs permanently; text conflict detection has
only this bounded horizon. Unpair clears both timestamp and recent IDs.

A failed clipboard application NACKs `clipboard_unavailable` and retains nothing:
no pending operation or inbound barrier. Sender MAY retry, drop, or supersede
failed text. PC keeps only its latest pending outgoing text, replacing any older
unsent value; an already-sent value may await its receipt. Concurrent local sends MUST preserve timestamp order: an older or equal
`created_at` MUST NOT overwrite a newer pending value.
Receiving an old receipt MUST NOT remove a newer pending value. Retrying preserves the original
ID, text and timestamp. A crash between clipboard application and history commit
can repeat that value; clipboard application is best effort. Files never block
clipboard text except for bounded transport backpressure.

## Timers, receipts and errors

In READY, phone sends PING after **60 s without transmitting any valid frame**.
Receiving traffic alone does not postpone this ping. PC never initiates periodic
PING in v1 (either side still implements PING/PONG). Receiver replies with the
same nonce immediately, target within 1 s, scheduling ahead of queued chunks.
Only one outstanding PING is allowed; only a matching PONG clears it. Missing
matching PONG after **30 s** closes the connection even if other traffic arrived.
Receivers apply TCP backpressure when bounded queues fill; a storage stall that
delays reading PING beyond the PONG deadline closes the connection for retry.
PC closes after **180 s without any valid received frame**. This gives idle
phones about one wakeup per minute and tolerates scheduling jitter; actual
Samsung background/Doze behavior still requires device testing.

File sender has a 60 s write-progress timeout; receiver has a 60 s timeout between
BEGIN/chunks/END (keepalives do not extend file progress). After END/text, sender
waits at most 120 s for receipt. Receiver must produce a receipt within that
window, or close; expensive storage must not block keepalive handling except
while applying TCP backpressure. PING and receipt timers use monotonic time.
On timeout close and retry all pending work.

| NACK code | `retryable` | Handling |
| --- | --- | --- |
| `hash_mismatch` | true | Recheck local queued bytes; retry at most 3 times, then pause and expose error |
| `size_mismatch` | true | Same bounded retry rule; never change ID/metadata to hide an error |
| `storage_failed` | true | Retry after 60 s (±20% jitter); user can free space |
| `disk_full` | true | As `storage_failed`; show "PC disk is full" |
| `not_writable` | true | As `storage_failed`; show "PC can't save files" |
| `unsupported_kind` | false | Pause operation until configuration/capability change or explicit discard |
| `id_conflict` | false | Implementation/security error; pause and expose diagnostic |
| `clipboard_unavailable` | true | May retry after 60 s, drop or supersede text; expose OS restriction |

These retryable values are mandatory for each code. File NACK never silently
removes a queue entry. Clipboard text may be dropped or superseded on NACK. Reset automatic mismatch retry counts only with explicit user
retry. No per-chunk ACK exists. ACK/NACK IDs refer to operations sent by the
recipient; directions disambiguate identical IDs. NACK detail strings are not
machine-readable. ERROR has no receipt and always closes the connection.

| ERROR code | Meaning |
| --- | --- |
| `version_mismatch` | HELLO version unsupported; include supported_version |
| `unauthorized` | Identity/pin binding failed or forbidden provisional activity |
| `unexpected_message` | Invalid state, duplicate HELLO, invalid transfer sequence/offset |
| `malformed_message` | Invalid JSON, values or binary chunk structure |
| `frame_too_large` | Control or total frame limit exceeded |
| `unknown_message` | Unassigned type byte (never skip it) |
| `timeout` | Handshake, pairing, progress, receipt or liveness timer expired |
| `busy` | Connection/admission quota exhausted |
| `connection_replaced` | Another authenticated connection supersedes this one |
| `closing` | Deliberate normal shutdown/unpairing; retain sender queue |

## Security and implementation notes

Authenticate before creating files or applying clipboard content. Treat names,
MIME, timestamps, DNS/mDNS, source IDs and diagnostics as untrusted. MIME is a hint,
not executable permission; no automatic launching of executable attachments.
Enforce byte limits before large allocations, impose disk/queue quotas, and use
one streaming hash/file buffer rather than accumulating files. Never send file
bytes, clipboard text, pairing secrets or keys to logs. Avoid acknowledging data
before durable storage: this is what protects offline delivery after crashes.
User content and pins remain encrypted on the wire, including over Tailscale.
Protocol syntax validation does not provide certificate/authorization checks.

`docs/protocol-vectors/v1.json` is the interoperability fixture. `frames` contains
exact full wire hex plus typed `decoded` meanings for every ID. Control encoders
need not reproduce its JSON field order or whitespace; decoders must accept its
bytes. `transfers` references frame names and declares reconstructed bytes, count
and hash, including a valid two-chunk transfer and an empty file. `pairing_uris`
contains canonical URI text and decoded fields. `invalid_frames` and
`invalid_pairing_uris` must be rejected. Rust unit tests load all entries. The
crate intentionally does not implement TLS, connection/transfer states or a daemon.
