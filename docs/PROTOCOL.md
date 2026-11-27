# Pneu wire protocol v1

This document is normative for independent Kotlin and Rust implementations. MUST,
SHOULD and MAY have their usual RFC 2119 meanings. The protocol crate implements
syntax and framing; certificate verification, clocks, retry queues, durable
storage and these state machines belong to the applications.

## Transport and discovery

The TCP initiator is the **dialer**; the listener is the **acceptor**, independent
of device role. Computers listen on TCP port **47821** by default and, for mesh
links, also dial other computers. The Android phone only dials. One bidirectional,
long-lived TLS connection per peer carries that peer's frames.
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
and the certificate fingerprint is the identity. Dialers MUST verify the expected
acceptor pin; acceptors check dialer fingerprint admission immediately after TLS. Both MUST
verify the P-256 public key, certificate self-signature, current certificate
validity dates, and the TLS CertificateVerify signature. A custom Android TrustManager or rustls
verifier MUST NOT bypass CertificateVerify. Disable TLS 0-RTT and session
resumption in v1 so every connection presents and checks both certificates.
An absent certificate or a mismatching acceptor pin fails TLS before protocol traffic.
The acceptor completes TLS for any dialer certificate that passes all cryptographic and
structural checks. Immediately after TLS, before reading any protocol frame, it
checks the client fingerprint. Unless it is paired or admissible under an active
pairing invitation and admission quotas, the acceptor sends `ERROR unauthorized` with a
short message and closes within the 2 s fatal-close bound. Unknown clients count
toward the same total connection and handshake limits. During pairing only,
admitted unknown clients enter the restricted provisional state specified below.
Dialers treat `ERROR unauthorized` from an acceptor authenticated against the stored pin
as a revocation signal ("Pairing lost"), rather than a network failure.

Each computer advertises mDNS service `_pneu._tcp.local.` (service type `_pneu._tcp`), SRV
port and TXT keys `v=1`, `fp=<computer fingerprint>`. Instance name is for display only.
Android phones discover computers through this advertisement using NsdManager;
computers do not browse mDNS. For each computer peer, the dialer tries: (1) the
last-good LAN endpoint, (2) HELLO address hints learned from that peer, (3)
invitation addresses, (4) Tailscale addresses. The persisted route cache keeps
last-good first, then HELLO hints before invitation fallbacks. Tailscale addresses
always fall in the final category, even when last-good. Preserve candidate order
within a category, trying each once before the next category. mDNS TXT is
untrusted and cannot change a phone's stored pin.
Tailscale `100.64.0.0/10` addresses, including the planned `100.67.144.28`, route
through the phone's Tailscale app. LAN addresses can be IPv4, IPv6 or DNS names.
PC selects QR and mDNS advertisement addresses from the same prioritized
shortlist: Tailscale IPv4, interfaces carrying a default route, then other
physical LAN interfaces.
Exclude Docker/bridge/veth/virbr, loopback, unspecified, multicast and link-local
addresses before applying the eight-address limit. DNS A/AAAA records have no
ordering guarantee; priority governs the selected shortlist. Local interface
discovery failure MUST NOT prevent the TCP daemon from running.

Per endpoint, TCP connect timeout is at most 5 s (1 s for secondary LAN
hints and invitation fallbacks) and TLS handshake timeout is 10 s. A computer
dial sweep has a fixed 30 s deadline including admission waits, DNS, TCP, TLS and
HELLO; reaching it ends the sweep and releases the candidate's socket and permits. After all
candidates fail or the sweep deadline expires, retry after 2, 4, 8, 16, 32, then
60 s, with uniform ±20% jitter. Network changes and user-initiated sends MAY trigger an immediate retry.
Never run parallel connection races to the same peer; independent peer dial loops
may run concurrently. Mesh trust, queues, capabilities, sessions and binding
generations are scoped by peer device ID. Pairing a new device MUST NOT remove or
invalidate any other pairing, receipt, queue or pending action. Re-pairing the
same device replaces only that peer's binding. Saved files remain.
Only one active READY connection per peer is retained. For two computers, if two
authenticated READY connections to the same peer coexist, both endpoints MUST
keep the connection dialed by the lexicographically lower device ID (compare the
32 lowercase hex strings bytewise), and close the other with
`ERROR connection_replaced`. Never close a lone connection merely because the
higher device ID dialed it. Do not deduplicate provisional/pairing connections
before authentication, binding persistence and READY. If duplicates have the same
dialer (including phone reconnects), the newest authenticated READY connection
wins. Registration/replacement MUST be serialized per peer. On a replaced
connection suppress subsequent connection events and pending side effects;
reschedule unacknowledged deliveries on the retained connection. Duplicate close
is not revocation: it MUST NOT delete trust, receipts, settings or queued work.
For a connection without negotiated `mesh`, retain the legacy phone/PC wire and
single-connection replacement behavior for that peer. An old implementation may
still support only one paired device; a mesh implementation MUST NOT evict its
other peers to imitate that local limitation.
An ordinary disconnect does not revoke completed-transfer desktop actions or
admitted clipboard text history; these remain allowed unless the pairing is
revoked or a newer connection is admitted.
While unpairing, refuse new connection registrations for that peer only. Work
admitted under an older pairing MUST NOT mutate the replacement pairing's state
or initiate desktop actions after replacement/unpair completes, even if the
device ID is reused. A failed lifecycle transaction MUST NOT announce successful
pairing or unpairing; report the failure, retain the durable binding and permit
reconnect. The old connection may be closed. Completed files remain untouched.

## Pairing invitation URI

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
versions and locally expired brand-new invitations before dialing. A syntactically
valid unsupported invitation shows **"Update required"**; malformed links remain
invalid. A persisted
pending pairing attempt is recovery, not a brand-new invitation, and MUST be
retried even after its original `exp` has passed. PC's clock is decisive
for token expiry; explain a clock disagreement to the user.

PC generates a token with an OS CSPRNG, keeps it only in memory, and sets `exp`
to creation time + **300000 ms**. Closing the pairing window, expiry, PC restart,
or first successful use invalidates it. A new invitation invalidates the old
one. Do not log tokens or invitation URIs. The invitation is the trust channel
for the computer pin: scan its QR from the actual computer's screen, or copy the
unchanged `pneu://pair` link through a user-trusted channel and paste it in the
joining device's UI. A device with a camera MAY scan the QR, including a computer.
For computer-to-computer pairing the joiner is the dialer and runs exactly the
same invitation/token/pin/expiry flow as the phone. An invitation MUST NEVER be
passed as a command-line argument (including a shell command containing the URI);
a CLI MUST read it from stdin, or use a UI paste field. A bare short numeric code
is forbidden: it would require a separately designed PAKE protocol. No phone
introduction or transitive trust is defined. Discovery never supplies a trusted
replacement fingerprint.

## Mesh (capability `mesh`)

Framing, protocol version (`1`), TLS, pairing invitations and all existing type
numbers remain v1. A connection negotiates mesh semantics **only if both HELLOs
advertise `mesh`**. New senders MUST NOT send PEER_SETTINGS or ordering fields to
a non-mesh peer; they retain the existing v1 clipboard, file, pairing and timer
wire behavior on that connection. Optional HELLO hints are safe to ignore, but
do not by themselves negotiate mesh. Codecs validate syntax; applications enforce
negotiation, authenticated identities, clocks and settings.

Mesh uses direct connections to each selected peer, originator-only fan-out,
device-wide clipboard ordering and synced per-pair settings specified below.
Received content MUST NOT be forwarded automatically, including through a clipboard
watcher. An explicit user-initiated send of previously received content is a new
operation originated by the sending device, with a new `transfer_id` and `hlc`,
and `source_device` set to that device. It excludes the device from which the
content was received. Origin markers plus a bounded recent-content hash set SHOULD
suppress automatic OS echoes.

A version or capability mismatch MUST NEVER unpair a peer, change its binding
generation, or discard queued work. Unsupported versions close with
`ERROR version_mismatch` and show **"Update required"**. Missing `mesh` continues
legacy-compatible features; an attempted mesh-only feature shows **"Update
required"** and waits for an update, rather than sending an unknown type or
reporting pairing lost. `connection_replaced` is never pairing lost.

Existing pairings migrate without re-pairing: preserve device IDs, certificates,
pins, receipts and queued deliveries. Trust is per device, with a **per-peer
binding generation** fencing work admitted under that peer's old binding.
Replacing/revoking one binding advances only its generation; old work cannot
mutate that peer's replacement state or perform clipboard/desktop effects.
Capability/version negotiation does not advance any generation. Unpairing one
peer preserves all other peers and deliveries; local identity MUST remain stable
while any other pairing exists. Re-pairing the same peer preserves pair switches.

## Common types and JSON rules

JSON control payloads are UTF-8 objects without BOM. Required fields listed
below MUST be present; optional fields MUST be omitted when absent (not `null`).
Unknown object fields and unknown HELLO capability strings are ignored. Reject
duplicate known fields, wrong types, invalid values, invalid UTF-8, trailing
non-whitespace, and non-object payloads. JSON field ordering/whitespace is not
significant. Strings have their byte limits after JSON unescaping, in UTF-8.
Message and setting object member names MUST be valid Unicode strings, including
unknown names (escaped unpaired surrogates are invalid).
Known integer fields are unsigned and MUST use JSON integer tokens (decimal
digits, with no minus sign, fraction or exponent). In particular, `-0` is invalid.
For example `1.0` and `1e0` are invalid even though JSON
Schema treats their numeric values as integers. No JSON compression, file
compression or base64 layer exists.

* `Id`: exactly 32 lowercase hex digits, representing 16 random bytes. Device IDs
  persist across launches; transfer IDs are freshly generated with 128 bits of
  CSPRNG entropy per logical operation, then persist until ACK. Reinstallation
  generates a new device ID and requires pairing. The TLS pin is bound to one
  device ID at pairing; a different HELLO ID for a known pin is unauthorized.
  A remote HELLO claiming the receiver's own device ID is always unauthorized,
  regardless of role, certificate or pairing token. Reject it before accepting
  PAIR_REQUEST, installing trust, or changing peer/settings state. Invitation
  previews reject the local certificate fingerprint; the remote device ID is
  checked when HELLO arrives, before sending PAIR_REQUEST.
* `Hash`: exactly 64 lowercase hex digits, SHA-256 of original file bytes.
* `Timestamp`: integer Unix UTC milliseconds in `0..9007199254740991` (JSON's
  exactly representable integer range). Use a wall clock for metadata, a monotonic
  clock for timers; timestamps are never trusted for authorization. Clipboard
  text uses `created_at` for legacy ordering only; mesh keeps it for display.
* `Hlc`: an unsigned 64-bit JSON integer token in `0..18446744073709551615`.
  It is a scalar millisecond hybrid logical clock: wall milliseconds and logical
  increments occupy the **same integer**, with no bit shift, epoch change or
  separately packed counter. One increment means one scalar tick, not necessarily
  one elapsed millisecond. Implementations MUST parse/compare all 64 bits exactly
  (Kotlin must not narrow to signed Long or Double). It is distinct from Timestamp.
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
| `02` | PAIR_REQUEST | Dialer presents one-time secret |
| `03` | PAIR_RESULT | Acceptor accepts/rejects pairing |
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
| `21` | PEER_SETTINGS | Synced directional pair switches (`mesh` only) |
| `22` | SMS | Ephemeral Android SMS (`sms` + `mesh` only) |
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
        "paused":{"type":"boolean"},"sms_activated":{"type":"boolean"},
        "role":{"enum":["phone","computer"]},
        "addresses":{"type":"array","maxItems":8,"uniqueItems":true,"items":{"type":"string","minLength":1}}
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
        "source_media_id":{"type":"string","minLength":1},
        "hlc":{"$ref":"#/$defs/hlc"},
        "source_device":{"$ref":"#/$defs/id"}
      },
      "dependentRequired":{"hlc":["source_device"],"source_device":["hlc"]}
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
      "properties":{"transfer_id":{"$ref":"#/$defs/id"},"text":{"type":"string"},"created_at":{"$ref":"#/$defs/timestamp"},"origin":{"enum":["clipboard","share"]},"hlc":{"$ref":"#/$defs/hlc"},"source_device":{"$ref":"#/$defs/id"}},
      "dependentRequired":{"hlc":["source_device"],"source_device":["hlc"]}
    },
    "sms": {
      "type":"object","required":["transfer_id","sender","text","received_at"],
      "properties":{"transfer_id":{"$ref":"#/$defs/id"},"sender":{"type":"string"},"text":{"type":"string"},"received_at":{"$ref":"#/$defs/timestamp"}}
    },
    "hlc":{"type":"integer","minimum":0,"maximum":18446744073709551615},
    "peer_setting": {
      "type":"object","required":["key","enabled","hlc","writer_device"],
      "properties": {
        "key":{"type":"string","pattern":"^[0-9a-f]{32}/[0-9a-f]{32}/(clipboard|automatic_media|sms)$"},
        "enabled":{"type":"boolean"},
        "hlc":{"$ref":"#/$defs/hlc"},
        "writer_device":{"$ref":"#/$defs/id"}
      }
    },
    "peer_settings": {
      "type":"object","required":["transfer_id","settings"],
      "properties": {
        "transfer_id":{"$ref":"#/$defs/id"},
        "settings":{"type":"array","minItems":1,"maxItems":4,"items":{"$ref":"#/$defs/peer_setting"}}
      }
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
whether the phone's automatic sending is paused. Optional phone-only
`sms_activated` is a boolean, advertised only as an authenticated UI hint alongside
`sms`: true once the phone has enabled SMS at least once, retained after disabling
it. It makes the desktop SMS settings section visible; it grants no delivery
permission and does not change the pair's SMS register. Missing means false for
this visibility hint. Old HELLO decoders ignore this optional field.
`name` remains required; it is already the display hint, so no second optional
name is added. Optional `role` is `phone` or `computer`; mesh implementations
MUST send it and require it from a mesh peer. It controls applicable pair settings
and dial/listen behavior, never trust or authorization. For legacy peers infer
the existing phone/computer role from the legacy link context, not a universal
"absent means phone" default.
Do not act on new `role`/`addresses` hints until both HELLOs negotiate mesh.

Optional `addresses` is an ordered list of 0–8 distinct endpoint strings using
the invitation `addr` grammar. Computers SHOULD send their current listening
endpoints so a paired computer can later dial them; phones omit it. An empty
list advertises no current endpoints. Apply the same interface exclusions and
shortlist policy as discovery. Only accept these hints after authenticating the
HELLO ID/certificate binding (or persisting a successful pairing). Hints refresh
routing candidates, never pins, device IDs or trust; absence preserves cached
addresses, and an empty list does not erase invitation or last-good caches.

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
`clipboard_image`, `mesh`, `sms`. Advertising `clipboard_image` requires `files`. Capabilities
mean ability/willingness to receive that feature, not permission to bypass local
settings. Send a feature only if the receiving HELLO advertises it. Both sides
always implement ACK/NACK, HELLO, ERROR and keepalive. Unknown capabilities are
ignored; local policy MAY NACK an advertised feature (for example after disabling
clipboard in settings). `message` strings in PAIR_RESULT/NACK/ERROR are optional
human-readable diagnostics, at most 512 UTF-8 bytes; never branch on them.
`version_mismatch` ERROR MUST include `supported_version`; other ERROR codes MUST
omit it. Future HELLO versions must retain the v1 HELLO envelope for negotiation.
The phone advertises the existing `files` capability only once its file receive
state machine is implemented; this enables FILE_BEGIN/CHUNK/END in that direction
without new message types. Codec support alone is not receive capability.

FILE_CHUNK payload is binary, with these **absolute frame offsets**:

| Offset | Size | Meaning |
| --- | --- | --- |
| 0 | 4 | `L = 25 + data_length` |
| 4 | 1 | `0x11` |
| 5 | 16 | Transfer ID bytes, decoded from FILE_BEGIN's 32 hex digits |
| 21 | 8 | Unsigned u64 big-endian byte offset into original file |
| 29 | 1–262144 | Raw bytes, no encoding |

Offset starts at zero and equals bytes already received for this transfer.
Offset plus chunk data length MUST be at most 64 GiB (check `offset <= MAX_FILE`
and `data_length <= MAX_FILE - offset`, without overflowing addition). Non-contiguous offsets, wrong IDs or chunk data beyond
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

1. **DISCONNECTED → TCP → TLS.** Dialer authenticates acceptor against invitation/stored pin.
   The acceptor requires a cryptographically valid client certificate and completes TLS
   before checking fingerprint admission. An unknown fingerprint outside an
   active pairing window, or denied by admission quotas, receives
   `ERROR unauthorized` and closes before the acceptor reads any frame. During a pairing window,
   admitted unknown clients may proceed only into a restricted provisional state.
   Limit provisional connections to one and rate-limit admission/token failures
   locally; total connection and handshake limits also cover unknown clients.
2. **HELLO exchange.** Dialer sends HELLO first within 10 s of TLS completion. The acceptor
   replies with HELLO within 10 s of receiving it. No other message is allowed
   before both HELLOs except ERROR. Each endpoint sends HELLO exactly once.
   A version other than 1 yields `ERROR version_mismatch` with
   `supported_version=1`, then close; no downgrade and no queued work is lost.
3. **Known dialer → READY.** The acceptor verifies the HELLO device ID matches its stored
   certificate binding. Dialer verifies the acceptor's HELLO ID matches the stored binding
   after the first pairing. Failure is `unauthorized`. Neither side sends
   PAIR_REQUEST/PAIR_RESULT for a normal authenticated reconnect.
4. **Unknown dialer → PAIRING.** Only the dialer may send PAIR_REQUEST and only the acceptor may
   send PAIR_RESULT. Request must arrive within 10 s after HELLO exchange; no
   application frames or keepalives are allowed in this restricted state. Dialer
   waits at most 10 s for the result. The acceptor checks window/expiry and compares the
   secret in constant time. The acceptor binds the TLS-presented fingerprint to HELLO
   device ID and durably stores it, then atomically consumes the invitation
   before returning `{"accepted":true}`. Storage failure rejects without
   consuming the token; never report success before persistence. A valid invitation
   replaces only the binding for the same peer device ID, including when its
   certificate changed; it MUST NOT affect other pairings. Concurrent attempts cannot both consume an invitation.
   `already_paired` remains a recognized v1 code for compatibility, but the acceptor
   does not use it to reject replacement pairing.
5. **PAIR_RESULT accepted → READY.** Dialer durably stores the acceptor's pin, HELLO device
   ID and addresses. The acceptor may send application data immediately after success;
   dialer processes it only once its own pairing state is persisted. Rejection
   sends accepted=false plus code, then closes TLS. The acceptor checks failures in order:
   closed window, expiry, token mismatch, storage.
   Lost success response is recoverable: the phone reconnects with the same cert
   and repeats PAIR_REQUEST from its in-memory attempt as specified below. The
   already-bound acceptor returns accepted=true without requiring a new token.
6. **READY → CLOSED.** Either endpoint may send ERROR, then orderly TLS close;
   peer does not reply with ERROR. ERROR is legal in every post-TLS state. On
   fatal decode/I/O failures, best-effort ERROR is optional; never wait to drain
   an attacker-supplied length. Close within 2 s. EOF without ERROR is normal
   network loss and triggers reconnect for pending work.

A dialer cannot distinguish known/unknown admission from HELLO alone.

**Phone dialer.** A phone dialer holds one app-scoped, in-memory pairing attempt
with a unique attempt ID, invitation, and frozen per-peer install fence. At scan it
captures the prior binding by pin (or absence) and its lifecycle revision; at the
first authenticated HELLO it fixes the remote device ID and the prior binding and
revision for that ID. Retries never refresh this fence. Installation requires both
captured targets to be unchanged; changes to unrelated peers do not invalidate the
attempt. Starting a repair also invalidates older connection outcomes for its
target. Explicit unpair cancels an attempt targeting that device; connection loss
never cancels an attempt.

If PAIR_RESULT is lost, the phone keeps this attempt and reconnects using its pin
and sends PAIR_REQUEST again, even after the invitation's `exp`. Only a brand-new
attempt is subject to the local expiry check. Activity or service recreation within
the process preserves the attempt; process death abandons it and requires a
re-scan. Rejection or local cancellation ends the attempt. Cancellation invalidates
its ID before closing transport, sends no UNPAIR, and does not undo acceptance by
the computer. Re-scanning can recover the computer's retained binding using the
known-certificate recovery exception below. Accepted=true durably installs the
binding before processing application data. Any outstanding delivery cleanup is
journaled with the installation and resumes after a crash without reinstalling or
advancing its generation again. The old binding remains until installation.

A re-scan with the same pin and device ID is idempotent: retain the generation,
queue, receipts and switches, and clear pairing loss. A different certificate for a
known device ID may replace that binding only if the prior is lost, under the
frozen fence, using its existing capacity slot and preserving switches. Otherwise
the phone requires the user to unpair it first; there is no replacement prompt
during the handshake. A known certificate claiming a different device ID fails
closed without automatic replacement.

**Computer joiner.** A computer joiner holds its attempt in memory only. Only one
unresolved computer join is allowed, including a preview awaiting confirmation. A
new join MUST fail with a pairing-in-progress error until the existing attempt
completes, is definitively rejected, or is cancelled locally. Display errors,
including TLS or fingerprint mismatch, MUST NOT replace the attempt.

If PAIR_RESULT is lost, the joiner reconnects using the same token and pin and
sends PAIR_REQUEST again, even after the invitation's `exp`. Only a brand-new
invitation is subject to the local expiry check. A definitive rejected result ends
the attempt; storage failures, missing rejection codes and generic admission denials
remain retryable. A computer joiner restart abandons its in-memory attempt; the
user pairs again with a fresh invitation.

At the first PAIR_REQUEST, once HELLO is known, a computer joiner captures the
acceptor's device ID, its local pairing tombstone generation, and the current
admission entry for the invitation fingerprint. Retries MUST NOT refresh this
per-peer install fence. At accepted=true, under the pairing gate, the joiner
installs only if all three remain unchanged. Otherwise it finishes without
installing: report paired if that fingerprint is now bound to that device ID,
and cancelled otherwise. Other peers' pairing changes do not invalidate this fence.
Cancellation is local and immediate; a computer join attempt NEVER sends UNPAIR.
An unconfirmed inviter binding is harmless: the next pairing reuses it through the
known-certificate recovery exception, and the user can remove it. It has no
time-based expiry.

A newly accepted computer binding MUST survive speculative reverse-dial admission
failures while its joiner may still be persisting or recovering PAIR_RESULT.
Destructive handling of reverse-dial unauthorized is deferred until there is proof
that the joiner persisted its binding: joiner-initiated READY-loop PING traffic on
a session after PAIR_RESULT, or a reverse dial accepted by the joiner's acceptor
(which accepts only known certificates outside pairing). A PONG on the pairing
connection is not evidence: the joiner may answer PING before persisting.
Readiness evidence is durable and scoped to the pairing generation.

For a certificate already bound to the same HELLO device ID, the acceptor MUST answer
PAIR_REQUEST with PAIR_RESULT accepted=true, without rechecking the token, expiry
or pairing window, then enter (or remain in) READY. This is an idempotent recovery
exception, also legal after a normal known-client HELLO; a second PAIR_REQUEST on
the same connection is unexpected_message. A mismatched device ID remains
unauthorized. A paired dialer without a pending attempt sends no PAIR_REQUEST.
No duplicate HELLO is required. Application frames arriving during recovery must
wait until the dialer's binding has been persisted.

## Mesh clipboard ordering

On a mesh connection CLIPBOARD MUST include both `hlc` and `source_device`.
The latter MUST equal the authenticated sending peer's device ID: there is no
automatic content forwarding. `created_at` stays a Timestamp for display, never the mesh
ordering key. The same optional fields are carried on image FILE_BEGIN messages
to order deferred clipboard effects; these fields occur together or are both
absent, and require a MIME beginning with `image/` and a nonempty subtype.
They are mandatory for `kind=clipboard_image` on mesh and SHOULD be supplied on
other image transfers so existing optional copy-received-image actions work.
All syntax codecs accept
legacy messages without either field. On a negotiated mesh link, an unstamped
CLIPBOARD or clipboard-image BEGIN is a fatal `ERROR malformed_message` (retain
trust and queues); MUST NOT silently fall back to `created_at` ordering. On a
non-mesh link send neither field and use the legacy rules.

An authenticated peer claiming a different `source_device`, or a mesh HELLO
without `role`, is `ERROR malformed_message`, not a revocation signal.

Each device maintains one HLC `last_seen` and **one applied register for the whole
device**, `(hlc, source_device)`, initially empty. Do not use one applied register
per peer. For each local clipboard copy (text or image) or explicit originating
text-share action, atomically assign
`hlc = max(now_ms, last_seen + 1)`, set `source_device` to the local device ID,
and advance both `last_seen` and the applied register before originating fan-out.
`now_ms` is nonnegative Unix UTC milliseconds. Persist the clock/register so a
restart or clock rollback cannot reuse an older local stamp. Checked addition is
required; at u64 exhaustion stop originating new stamped operations and report a
clock error, never wrap. Every recipient of one operation receives the same
stamp; retries preserve it.

Before observing any incoming HLC, reject `hlc > now_ms + 60000` with fatal
`ERROR malformed_message` and report clock disagreement; use overflow-safe
comparison. **Reject**, do not clamp, so a given operation has the same ordering
key everywhere. Do not advance `last_seen`, the register, receipts or settings on
this error. A syntactically valid u64 can therefore fail session clock validation.
After identity, negotiation and skew validation, advance
`last_seen = max(last_seen, incoming_hlc)`, even for stale valid operations. The
next local operation increments this observed maximum with the formula above.

Compare keys lexicographically by unsigned HLC, then by the lowercase hex device
ID; **greater wins**, including the greater device ID on equal HLC. An incoming
clip is eligible only if its key is strictly greater than the applied register
(or the register is empty). Retained-ID conflicts still NACK `id_conflict`.
Stale text ACKs `deduplicated=true` without an OS effect. An eligible copy identical
to the known current content also ACKs `deduplicated=true` without an OS write,
while advancing the register and recording its receipt. New eligible
text retains the existing reserve/apply/finalize/ACK transaction and bounded
recent-ID history, with `hlc`/`source_device` included in metadata equality.
Failure NACKs `clipboard_unavailable`, does not advance the applied register and
does not leave pending text. Observed `last_seen` need not roll back on failure.

The eligibility, current pair setting and binding-generation checks MUST run
again **immediately before the OS clipboard side effect**, serialized with local
copies and all other incoming clipboard effects. A queued older operation cannot
apply after a newer local or remote one. Advance the register only for an applied
incoming value; a stale queued effect is dropped. A file-based clipboard image
uses its BEGIN stamp when its deferred action runs, not FILE_END time or file
mtime. File placement and file ACK remain durable even when its clipboard action
is stale, disabled or fails. An image without an ordering stamp MUST NOT bypass
this register by triggering an automatic clipboard write on a mesh link;
non-clipboard file saving and notifications continue. For an image file that did
not originate as a local clipboard copy, assign its stamp once at operation
creation using the local HLC formula, advancing `last_seen` but not the local
applied clipboard register. This includes automatic screenshots/photos. It does
not create a local clipboard effect on the sender.
The global applied register survives unpairing any one peer; only that peer's
receipts/history are cleared, so another peer's old clip cannot overwrite it.

Legacy incoming text on a mixed mesh/legacy device first passes its existing
per-peer `created_at` gate. If applied, serialize it through the same global OS
effect boundary and assign a local observation stamp with the local HLC formula
and authenticated legacy peer ID. Do the same at the effect boundary for legacy
file-based image clipboard effects. It is not forwarded or restamped on the wire.
This fences queued mesh actions behind a later legacy effect without claiming
mesh convergence with a legacy endpoint. Clipboard delivery is latest-only, in
memory and connected-only for all peers, including legacy peers. A disconnect
or restart drops unsent clipboard content; a reconnect does not replay it.
Legacy wire messages strip the stamp and set `created_at` to the operation's HLC.
Old durable legacy clipboard rows are discarded at migration/startup.

## SMS (`22`, decimal 34)

```json
{"transfer_id":"00112233445566778899aabbccddeeff","sender":"BancoEstado","text":"Tu clave dinámica es 482913.","received_at":1800000000000}
```

SMS is valid only in READY from an authenticated phone to a computer when both HELLOs advertise `sms` and `mesh`, after the directional settings have synchronized. Both implementations understand it, but computers never originate or relay SMS. Never send SMS to an older peer or use CLIPBOARD/FILE_BEGIN as a fallback. `transfer_id` is an Id, sender is 1–256 UTF-8 bytes, text is 0–16,384 UTF-8 bytes, and `received_at` is the phone arrival time in Unix milliseconds (Timestamp). Strings and timestamps use the existing strict control rules. Unknown fields are ignored; known duplicates, nulls, invalid Unicode and oversized fields are rejected.

When SMS keeps an Android connection alive on mobile data while ordinary mobile
transfers are disabled, its HELLO omits `files`, `clipboard_text` and
`clipboard_image`; it still advertises `mesh` and `sms`. It reconnects when that
route policy changes. Thus peers must not send ordinary content on the SMS-only
connection, and outgoing non-SMS frames retain the ordinary cost gate.

Android joins multipart PDUs from one SMS_RECEIVED broadcast by sender, in order. It requires the global SMS preference, RECEIVE_SMS grant, unpaused state, current pairing and enabled pair SMS register at admission and again before transmission. SMS uses the available paired connection regardless of mobile-data/queue-offline preferences. It is held only in memory, per original recipient and pairing generation, for at most 120 seconds from arrival. Pause, disabling or losing permission/pairing revokes pending messages. No SMS sender/text/code enters durable queues, transfer receipts, Recent or diagnostics.

ACK acknowledges SMS delivery rather than a durable receipt. Retries keep the same transfer ID and are deduplicated in a bounded in-memory receiver window. Disabled pair settings return unsupported_kind; no content is shown. Messages at least 120 seconds old are silently acknowledged and dropped. A timestamp over 60 seconds ahead is also dropped. Reconnect/restart does not create a durable SMS backlog. Receiver code detection, notification masking, lock checks and secret clipboard ownership are local desktop policies. SMS is never forwarded over a computer mesh or written to the phone clipboard.

## PEER_SETTINGS (`21`, decimal 33)

This is the next unassigned control ID after CLIPBOARD (`20`); it remains inside
v1 and is legal only in READY with negotiated `mesh`. No extra ACK message type
is introduced. Payload example, with A and B replaced by real 32-hex device IDs:

```text
{"transfer_id":"<fresh Id>","settings":[
  {"key":"<A>/<B>/clipboard","enabled":true,"hlc":1800000000001,"writer_device":"<A>"},
  {"key":"<B>/<A>/clipboard","enabled":true,"hlc":1800000000002,"writer_device":"<B>"},
  {"key":"<phone>/<computer>/automatic_media","enabled":true,"hlc":1800000000003,"writer_device":"<A>"}
]}
```

`settings` has 1–4 entries with unique keys (at most 3 without negotiated `sms`). Key grammar is exactly
`<from_device_id>/<to_device_id>/<feature>`: two distinct lowercase 32-hex IDs,
two literal `/` separators, and feature `clipboard`, `automatic_media` or (only with negotiated `sms`) `sms`. No
"you/me", names, whitespace, aliases or suffixes. Both IDs MUST be the two
authenticated members of this pair. Each entry has a required boolean `enabled`,
Hlc `hlc`, and Id `writer_device` naming either member of that pair. Known nested
fields obey the same strict duplicate-field/type/null rules as top-level fields;
unknown object fields remain ignored. A malformed key is fatal
`ERROR malformed_message`; a key referring to another pair or a role-inapplicable
direction is `ERROR unexpected_message`. Do not merge any part of an invalid
snapshot. The syntax codecs check key shape, distinct IDs, feature and writer
membership; authenticated pair membership and roles are application checks.

The full state is both clipboard directions, plus exactly one
`<phone>/<computer>/automatic_media` entry for a phone/computer pair. Computer
pairs have just the two clipboard entries. A phone/computer pair with `sms`
negotiated adds exactly one `<phone>/<computer>/sms` entry, default enabled.
Never send this key or a four-entry snapshot to a peer without `sms`; that
peer retains the original three-entry snapshot, even after a capability downgrade. `clipboard` gates text and clipboard
images. `automatic_media` gates that pair's automatic screenshots, photos and
recordings together. This matches the pair's single Automatic sending / Receive
automatic photos switch. The phone's existing global per-kind `screenshots`,
`photos` and `recordings` toggles still decide which kinds exist; they are not
copied into per-pair state. A kind is sent only when its global toggle **and** the
pair switch permit it. The automatic gate applies to `screenshot`, `photo` and
`recording` file kinds, not manual `file` or `clipboard_image` operations.
No per-pair files switch is added: manual file sends use
the recipient's `files` capability and existing local receive/storage policy.
Pause, global sharing mode, privacy rules and OS permissions remain additional
local gates, not settings that one peer can relax remotely.
A snapshot missing any applicable key is `ERROR unexpected_message`; syntax
decoding alone does not establish a complete pair snapshot.

Either member may edit either direction. Every field is an independent LWW
register: compare `(hlc, writer_device)`, with the greater tuple winning. A local
edit uses the HLC advancement formula above and its own device ID; received field
clocks obey the same 60 s rejection rule and advance `last_seen`. Preserve the
original writer when retransmitting merged state; settings synchronization is
control state, not content forwarding. An equal tuple MUST have the same boolean;
an equal tuple with a different value is malformed. A snapshot does not give its
fields a shared timestamp. Merge/persist all winning fields atomically; a change
takes effect locally as soon as it is persisted, even offline. Both the sender
before sending and the receiver before applying MUST check current settings;
recheck at the OS boundary for deferred effects. Turning a switch off suppresses
queued unsent content in that direction; it does not revoke saved files/receipts.
Text arriving in a disabled clipboard direction NACKs `unsupported_kind` without
applying. A mesh text replay matching a retained receipt ACKs `deduplicated` before
checking the disabled direction; conflicting retained metadata still NACKs `id_conflict`.
A disabled automatic-media direction drains file bytes and NACKs
`unsupported_kind` at END. A disabled clipboard direction still allows ordinary
file saving, but suppresses every deferred image clipboard effect. Queued file
deliveries blocked by settings remain paused until enabled or explicitly
discarded; unsent clipboard text is dropped rather than queued.

After every HELLO exchange and binding persistence (after PAIR_RESULT for a new
pair), both endpoints MUST send full applicable state before sending content.
Until peer state is received and durably merged, hold outgoing content; continue
servicing keepalive/control and accept in-flight content only under current local
policy. After every local change send full state again; if a merge contributes
winners the peer did not send, send the resulting full state until both converge.
Do not echo an unchanged state indefinitely. Allow one unacknowledged settings
snapshot per direction; coalesce subsequent edits into the next full snapshot.

Use a fresh `transfer_id` in the shared sender operation-ID namespace for each
changed full stamped state, distinct from file/text IDs. Persist the exact
outgoing snapshot until ACK and retain the acknowledged snapshot across restart.
On reconnect, replay that same snapshot and ID if the full stamped state is
unchanged; mint a new ID only when the state changes. Pending snapshots always
replay before any newer coalesced state. Receiver persists the merged state and
a receipt keyed by
`(authenticated sender device_id, transfer_id)` including snapshot metadata,
then sends ordinary ACK (`deduplicated=false` for a newly recorded snapshot,
`true` for identical retained replay). Settings receipts MAY use a bounded recent
window (PC: latest 4,096 recorded snapshots per authenticated sender, ordered by
local insertion sequence). Prune older receipts atomically with the merge and
new receipt; this is a recoverable window, not a lifetime admission quota.
A retained ID with different metadata NACKs `id_conflict`;
persistence failure NACKs `storage_failed` without a partial merge. ACK/NACK must
match that peer/connection's pending snapshot. The existing 120 s receipt timer
and 60 s storage retry apply; on disconnect retry the exact pending snapshot
before sending a changed full state. Replaying an evicted snapshot safely merges
its fields again under LWW and records a new receipt (`deduplicated=false`);
subsequent retained replay ACKs `deduplicated=true`. Its older tuples cannot
replace newer fields, and equal tuples with conflicting values remain malformed.
ID conflict detection for evicted settings receipts is limited to the retained
window; file receipt retention is unchanged. Lost ACK is safe, and older replay
never replaces newer fields. A settings failure holds content until synchronization
succeeds; keepalive remains live.

For a newly paired pair, both clipboard directions and the pair automatic-media
switch default **on**, each stamped `(0, lower_device_id)` until the first edit.
The phone's existing per-kind defaults remain screenshots on, photos off,
recordings off. This enables everything that worked before without enabling a
previously disabled kind. Migrations seed phone→computer clipboard on (there is
no legacy direction-disable switch), computer→phone from the phone's
`clipboardFromPc` receive switch, and pair automatic media on. Preserve the
computer's existing `clipboard_sync_to_phone` as a local automatic-sharing gate,
its image inclusion/copy preferences, the phone's global kind toggles, and all
other local gates. Hiding a manual clipboard entrypoint is not a direction-disable
switch and MUST NOT disable the pair. Initial disabled imported values use
`(1, owning_device_id)`; initial enabled values use the default `(0, lower_id)`.
This makes a disabled legacy setting win the bootstrap merge. Later user edits
use normal clocks. Preserve saved pair state during re-pairing; defaults never
overwrite an existing field. Legacy peers exchange none of this message.

## Transfer and delivery state machines

For mesh, the originator freezes the selected recipient set when creating the
operation. Resolve "connected devices" at that instant, not on later reconnects.
Automatic media defaults to connected eligible computers only; queueing for
offline computers requires the existing explicit offline-queue policy. A manual
send to a named offline device may wait for that device. A newly connected or
newly paired device is never retroactively added. Clipboard is connected-only.

Stage one immutable artifact, then create one independent delivery per recipient:
separate ACK/NACK, retry budget/timer, queue state and progress. One recipient's
failure or unpairing MUST NOT affect another's delivery. The same `transfer_id`
MAY be used across recipients for the same operation/metadata. This remains safe:
receipts already use `(authenticated sender device_id, transfer_id)` at each
receiver, and outgoing receipts MUST match `(recipient device_id, transfer_id)`
on that recipient's authenticated connection. Never match by transfer ID alone
across connections. Received content is never re-enqueued as originated content.

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

The following text ordering rules apply only without negotiated `mesh`; mesh
uses the device-wide register specified above. Text CLIPBOARD shares the
transfer-ID namespace but is **last-writer-wins**,
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
failed text. PC keeps only its latest pending outgoing text in memory for each connected peer,
replacing any older unsent value; an already-sent value may await its receipt. Concurrent local sends MUST preserve timestamp order: an older or equal
`created_at` MUST NOT overwrite a newer pending value.
Receiving an old receipt MUST NOT remove a newer pending value. A retained in-flight delivery across duplicate connection replacement preserves
its original ID, text and timestamp. A crash between clipboard application and history commit
can repeat that value; clipboard application is best effort. Files never block
clipboard text except for bounded transport backpressure.

## Timers, receipts and errors

In READY, the dialer sends PING after **60 s without transmitting any valid frame**.
Receiving traffic alone does not postpone this ping. The acceptor never initiates
periodic PING (either side still implements PING/PONG). Receiver replies with the
same nonce immediately, target within 1 s, scheduling ahead of queued chunks.
Only one outstanding PING is allowed; only a matching PONG clears it. Missing
matching PONG after **30 s** closes the connection even if other traffic arrived.
Receivers apply TCP backpressure when bounded queues fill; a storage stall that
delays reading PING beyond the PONG deadline closes the connection for retry.
The acceptor closes after **180 s without any valid received frame**. This gives idle
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
| `connection_replaced` | Another READY connection to this same peer supersedes this one; retain trust/queues |
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

Paired computers' address hints can trigger outbound TCP/TLS attempts to arbitrary
host:port destinations, including loopback. The certificate pin prevents application
data exchange with an untrusted endpoint, but does not prevent TCP connection or
TLS ClientHello traffic. This residual routing risk is accepted; hints are bounded
to sixteen persisted routes per peer and twenty-four dial candidates. Computer
peers do not consume mDNS routing hints. The advertisement retains its eligible
interface allowlist, eight-address shortlist and periodic refresh for phones.

`docs/protocol-vectors/v1.json` is the interoperability fixture. `frames` contains
exact full wire hex plus typed `decoded` meanings for every ID. Control encoders
need not reproduce its JSON field order or whitespace; decoders must accept its
bytes. `transfers` references frame names and declares reconstructed bytes, count
and hash, including a valid two-chunk transfer and an empty file. `pairing_uris`
contains canonical URI text and decoded fields. `invalid_frames` and
`invalid_pairing_uris` must be rejected. Rust unit tests load all entries. The
crate intentionally does not implement TLS, connection/transfer states or a daemon.
