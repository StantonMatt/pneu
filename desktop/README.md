# Pneu desktop core

`pneud` is the PC daemon; `pneu-sim` exercises the phone transport before Android
is available. The wire contract is `../docs/PROTOCOL.md`, with the shared syntax
and framing in `pneu-proto`. Nothing in this directory installs automatically.

```sh
cd desktop
# This workspace's dependency cache is writable in the worker sandbox.
export CARGO_HOME="$PWD/target/cargo-home"
export QMAKE=qmake6 # Qt 6 development tools are required for the native backend
heavy cargo test
heavy cargo clippy --all-targets -- -D warnings
cargo fmt --check
heavy cargo build --release -p pneud -p pneu-sim
```

Run `pneud run` to start the daemon and tray. An unpaired daemon opens its
pairing window automatically; tray activation or `pneud window` opens the same
window later. `pneud pair` also remains available for headless use and prints the URI,
expiry in UTC milliseconds, and a terminal QR. Treat the URI as a secret; the
service never logs it. An invitation expires after five minutes. Reuse, TLS admission
and pairing completion share one predicate: both wall-clock and monotonic expiry
must remain in the future. `pneud
close-pairing` also invalidates it. The QR includes actual interface addresses,
prioritized as Tailscale IPv4, default-route interfaces, then other physical LAN
interfaces before the eight-address limit. Docker/bridge/veth/virbr, loopback and
link-local addresses are excluded. mDNS advertises the same shortlist and refreshes
on network changes within a minute; DNS address records themselves are unordered.
Discovery failures are reported and retried without stopping the TCP daemon; an
empty candidate list fails the pairing command. Addresses never establish trust.

```sh
pneu-sim --state /absolute/path/to/phone-state pair 'pneu://pair?...'
pneu-sim --state /absolute/path/to/phone-state send-file capture.png screenshot image/png
pneu-sim --state /absolute/path/to/phone-state clipboard 'hello'
pneu-sim --state /absolute/path/to/phone-state listen 120
pneud status
pneud clipboard PHONE_DEVICE_ID 'hello from the PC'
```

The simulator persists its identity, PC binding, pending pairing attempt, pause
state and last assigned clipboard timestamp. Text timestamps increase across
commands even when the wall clock moves backwards. Paused clipboard arrivals
are ACKed without applying them or advancing received history.
Each send command connects, sends one operation, waits for its receipt,
and exits. It creates no durable outbox and ignores legacy queued files. A new
command gets a fresh operation ID. Receipt waits and listening answer PINGs,
originate idle PINGs, and validate PONGs; clipboard diagnostics include IDs only.
`status true|false` sends STATUS, `unpair` removes the binding, `clipboard TEXT
share` marks shared text, and `send-file PATH file MIME` sends an ordinary file.
It checks the PC's advertised clipboard-image capability before sending that kind.

`install.sh` builds the release daemon through `heavy`, installs it into
`~/.local/bin`, installs the app/tray icons and desktop entry, writes the systemd user unit, and reenables it for `graphical-session.target`.
An active graphical session starts/restarts the daemon immediately, including
reinstalls; otherwise it starts with the next graphical session. Run it only
when installation is intended. The unit uses an owner-only umask, follows the
graphical session, and restarts after failures. `pneud` handles SIGINT/SIGTERM,
closes its sessions, removes its own Unix socket and stops its clipboard helper.
An advisory lock prevents competing daemons from recovering the same store.
A stale socket is reclaimed only under its runtime lock after an owner/type
check and a refused connection.

## UI API

Construct `Arc<Core>` with `Paths`, `Config`, and `Arc<dyn PcActions>`, then call
`serve(listener, true)` once. Listener injection and `false` mDNS enable local
integration tests. The executable binds dual-stack `[::]:47821` by default.

- `start_pairing()` returns `PairingUri` (including `expires_at`); call `build()`
  for the QR contents. CLI pairing replaces the active invitation;
  `ensure_pairing()` lets the window reuse it. `pairing_status()` and
  `wait_pairing_change(revision)` provide the owner-only invitation subscription.
  `close_pairing_if(token)` invalidates only the invitation the window displayed;
  `close_pairing()` closes pairing unconditionally.
- `unpair_device(Id)` refuses new registration, durably removes the certificate
  binding, receipts and queued text, then sends best-effort UNPAIR and closes.
  Persistence failure closes without UNPAIR and retains trust for retry/reconnect.
  Saved user files remain.
- `config()` returns the current configuration. `update_config(Config)` validates,
  writes it durably, and applies toggles. Port changes are rejected with
  "changing the port takes effect after restarting pneud"; edit the configuration
  file and restart for a new port. `pneud reload` applies other edited settings.
  The configured port is available to IPC immediately after Core construction.
- `send_clipboard(Id, String)` retains only the latest pending PC-to-phone text
  and returns its operation ID. Delivery waits for the receiving HELLO capability
  and honors the sync toggle. New unsent text replaces older pending text;
  in-flight text can finish awaiting its receipt. `discard_clipboard(Id)` drops
  the latest pending text. Text is last-writer-wins by `created_at` at the receiver.
- `paired_devices()` and `connected_devices()` return device IDs. `subscribe()` returns a bounded Tokio
  broadcast receiver for `Event`: connection changes, transfer start/progress/
  finish, received/sent clipboard operation IDs, config changes and errors.
  Events never include pairing secrets or clipboard contents. Handle lag by
  refreshing state; events are observations, not an authorization source.
- `request_shutdown()` stops the server; await `serve` to join connection workers.
  `shutdown_actions()` can also explicitly close desktop integrations.

The CLI's owner-only `$XDG_RUNTIME_DIR/pneud.sock` accepts bounded newline JSON
`ipc::Command` requests. Rust UI code can call `Core` directly; it does not need
IPC. The ksni tray runs in the daemon and remains optional: the transport works
without a StatusNotifier host. A single supervised `pneud ui-window` child runs
Slint's native Qt event loop and communicates through owner-only IPC (`UiStatus`,
`ShowWindow`, `ClosePairingIf`). This isolates window/backend failures from the
transport. The window uses native Breeze widgets, an error-correction-M QR with
five-minute refresh, and the XDG portal folder chooser. UI snapshots read the phone
ID, metadata, connection generation and status together; UiStatus and Status never
combine separate phone reads. Cached metadata carries its device ID. A replaced
pairing or legacy/mismatched cache displays “Phone” until current metadata arrives.
Core owns one invitation
shared by CLI and window; owner-only IPC subscriptions immediately update the QR
when CLI pairing replaces it. Closing hides the window and invalidates its
displayed token if still active; the next request raises it.

The Wayland data-control watcher observes only CLIPBOARD offers. It skips password
markers, Pneu's origin MIME and recent content hashes, debounces 300 ms, and sends
only to the exact connected, unpaused phone. Observation captures policy before
queuing data reads to a separate reader with one pending offer. New selections
cancel earlier reads, including slow password markers; shutdown joins both
threads. `send_automatic_clipboard` and
`send_automatic_image` share an ephemeral latest-only selection slot. Automatic
text persists only its monotonic timestamp, never its contents; every new offer, disconnect, pause
or changing sync/image policy invalidates pending automatic selections, including
content already prepared in Core or queued to the writer. Automatic work retains
its session/offer token until the writer accepts its first byte; later offer/policy
changes drop unsent work, while a begun frame or image finishes as in-flight.
Selection removal checks ownership atomically. Explicit IPC sends retain their durable outbox. Include Images defaults off, requires
the peer's `clipboard_image` capability, and sends PNG up to 10 MiB. General PC
file sending remains outside this API.

Tray SVGs are bundled and installed. The default ARGB fallback uses kdeglobals'
active Plasma desktop theme palette (or the window scheme for adaptive themes), refreshed by KGlobalSettings
`notifyChange` and periodically as a fallback. `PNEU_TRAY_NAMED_ICONS=1` enables
the named-icon spike; `PNEU_KDEGLOBALS` selects an isolated color configuration.

## Durability, tasks and desktop actions

The PC pairs with one phone. Accepting a valid invitation replaces the old pin,
binding, dedupe and pending text while preserving saved files. Newest authenticated
connection wins; a connection ID suppresses stale events from the old socket.
Unpair marks the lifecycle before closing/forgetting, so replacement cannot slip
in while trust is being removed. Pairing epochs cancel queued and running desktop
actions and the viewer debounce.

SQLite uses WAL, `synchronous=FULL`, owner-only files and foreign-key cascades.
It stores the single binding, completed file receipts, one failed-save cache
reference per transfer (metadata and path, never a success receipt), clipboard timestamp and
64 recent text IDs (hash, timestamp, origin), latest outgoing text, and ever-used
staging folders. There are no pending inbound text barriers or delivery leases.
Calls use one connection mutex in Tokio's blocking pool. Shutdown drains tracked
blocking work and cancellation cleanup. Streaming transfers never buffer a whole
file. Bounded reader, writer, file and text queues await capacity, applying TCP
backpressure. Once read, PING schedules PONG ahead of queued outgoing frames;
a storage stall can delay reading until the peer's PONG deadline. All writes
flush within their deadlines. Idle file workers have no transfer deadline.
One outgoing DB query survives select iterations instead of spawning jobs for
each status/config event. One provisional TLS connection and sixteen total
connections are admitted; unknown admission is limited to once per second.
PC initiates no periodic PINGs. Cache pruning runs hourly. Successful retries
retire failed-save previews by reopening without following links and checking
recorded device, inode, size and nanosecond modification/change times, relative
to the original directory handle. The evidence holds no file descriptors, so
pruning reclaims blocks even without a retry. Evidence is bounded to 128 entries;
after restart or eviction, previews remain for normal retention pruning.
Staging, temporary-config and runtime-socket cleanup also check identity and
use retained directory handles, preserving replacement entries.
Tray icons use the same anchored atomic writes and refuse symlinked descendants.

TLS is 1.3 only, with resumption/0-RTT disabled. Both verifiers check exact DER
fingerprints, P-256, certificate self-signature, validity dates and rustls's real
ECDSA P-256 CertificateVerify. Pairing has the prescribed HELLO/restricted state,
constant-time token comparison, error ordering, durable binding and one-time
consumption. Before sending PAIR_REQUEST, the simulator persists the PC pin, addresses and
token. A lost success reply keeps this pending record; reconnect repeats the
request even after invitation expiry. An already-bound certificate/device gets
accepted=true without checking the old token, then the phone persists its binding.

Receive files use directory handles for staging, rename and cleanup, streamed
SHA-256, and Linux `renameat2(NOREPLACE)`. Commit order is staging file fsync,
rename without replacement, destination/new-ancestor fsync, receipt transaction,
then ACK. Commit consumes the verified streamed count/digest and checks staged
length; it never rehashes completed file contents or repeats the file fsync.
Existing selected-folder permissions are preserved. Each save folder
has its own daemon-owned private `.pneu-staging/`: all bytes inside are disposable.
The store records each folder before stage creation; startup attempts cleanup of
every recorded staging root even after a config edit or a crash before the first
receipt. Unavailable/read-only roots are logged and deferred; they cannot prevent
startup, and existing staging-directory permissions are never changed. Background
maintenance does not sweep staging. New transfers retry cleanup before folder
reuse and are refused while it still fails; ready roots with active stages are
never swept, including aliases created by directory renames: readiness uses
directory identity instead of pathname. Optional cache creation and mDNS failure also leave the daemon running.
Cancellation removes only its transfer's stage. Save-path changes apply to new
transfers immediately; active transfers keep their captured config and handles.

There are no staging journals, rollback intents, persisted inode identities or
placed-file recovery. A crash or receipt persistence failure between rename and
receipt commit can leave a visible copy without a receipt, so replay may create
a duplicate. This small window is an explicit product trade-off. Durable ACKs
still deduplicate file replays permanently until unpair, and no recovery path
inspects or deletes uncertain user files. Metadata conflicts NACK after draining.
Capacity checks enforce configured size and a 16 MiB free-space reserve.

Text application uses a last-applied timestamp and 64 recent IDs per peer.
Identical recent replays and stale/equal timestamps ACK as deduplicated without
changing the clipboard. Newer text applies then commits its small history;
OS denial retains nothing and NACKs `clipboard_unavailable`. A sender may retry,
drop or supersede it. A crash before history commit may repeat an application.
There is no FIFO or pending-operation barrier. No clipboard contents appear in
events or logs. Completed file IDs have no expiry; cached files expire with a
one-day default retention.

`PcActions` isolates OS integration from protocol tests. Production uses zbus
notifications with the approved text, actions, urgency and transient hints.
File notifications use `x-kde-urls` for Plasma thumbnails; explicit clipboard
images use a bounded inline image preview. Open/default and Show
in folder actions are supported; Show uses FileManager1.ShowItems. Arrivals within
four seconds replace the notification with every item URL, latest first. Both
the gap and 30-second lifetime use transfer arrival times, so action-queue
delays cannot change grouping; clipboard debounce likewise uses offer arrival.
Burst folder actions retain pathnames and only the latest file descriptor. Bursts start
a new notification at 128 items or after 30 seconds, bounding each message. Open passes the retained
file descriptor through the desktop portal's OpenFile method (no pathname
fallback). Portal calls subscribe before requesting and wait for Response;
subscription/activation and method acceptance have five-second deadlines. Requests
bind to the issuing unique bus owner; losing the portal service releases pending
choosers/viewers and interaction slots, and Close never targets a replacement.
Interactive responses wait
until completion or cancellation, including application choosers and URI dialogs.
Viewer requests are authorised and issued under the action gate; their human
responses run in separately tracked tasks. Notification buttons also issue requests
without waiting for interaction in the signal loop. Each consumer permits eight
outstanding interactions and ignores further opens until capacity is available.
Shutdown cancels and joins these waits; viewer waits also cancel when their pairing
scope or connection generation is revoked. Unpair confirmation retains the phone
ID and name displayed when it opened, so pairing a replacement cannot retarget it.
Portal URI failures/acceptance timeouts use a
bounded `xdg-open` fallback. The folder chooser reports portal failures
and treats cancellation as no selection. Clipboard bytes and previews use the same received-file descriptor,
so replacement of the saved pathname cannot redirect an action to other bytes.
Automatic clipboard actions support PNG, JPEG, GIF, WebP and BMP with recognizable
content. Viewer launches additionally require a matching extension. HEIF/HEIC
and AVIF are ordinary saved/notified files; explicit clipboard-image requests
report an unsupported action and are never claimed as copied.
Ordinary images keep only the latest pending clipboard/viewer action, with a
one-second viewer debounce. Notifications have a 16-item best-effort queue;
old notifications may be dropped. Desktop actions never block file delivery.
Explicit clipboard-image requests are kept separately and are not coalesced,
with at most eight pending requests. Overflow emits an error and queues a
best-effort "Couldn't copy image" notification; the saved-file ACK is unaffected.
Explicit clipboard images use cache, copy regardless of the ordinary-image toggle,
never open a viewer, and get a dedicated action-free thumbnail notification after
copying succeeds. Preview reads share the 16 MiB input cap and resist file growth.
Storage failures identify the destination actually attempted: the selected save
folder or the daemon cache. Only folder errors offer Choose Folder; disabling
folder saving or changing the folder clears that error, while a successful cache
transfer clears a cache error. Listener errors take precedence. Clipboard-image
storage failures report failure without claiming a successful copy.
Notification closure removes its action mapping; at most 256 button mappings
are retained, so older notifications may lose their buttons' actions.

Wayland clipboard ownership is served by a tracked foreground helper using
wl-clipboard-rs/data-control, rather than forking the Tokio daemon. The library
buffers selection bytes; image copying is therefore capped at **16 MiB**.
Supported clipboard images offer their original MIME plus converted PNG bytes, with bounded
decode dimensions/allocation and a 32 MiB converted-output limit. Larger
images remain saved/ACKed and emit a clipboard-action error. Text is protocol-bounded at 16 KiB. A new selection replaces the old helper only
after its READY reply; shutdown kills and reaps it. Optional clipboard and
notification failures cannot select an older image for the debounced viewer:
the newest eligible image wins independently of those actions' timeouts.
Echo hashes replace the previous successful selection only after the helper's
READY response, including hashes for converted image formats; failed clipboard
writes preserve the previous guard.

Manual clipboard verification uses the same Wayland library:

```sh
cargo run -p pneud --example clipboard_probe -- capture.png image/png
```

It reads the current selection, streams a digest, and compares it to the file.
A manual run with isolated XDG paths needs an absolute `WAYLAND_DISPLAY` pointing
to the real compositor socket, because its runtime directory is temporary.

## Specification decisions

PROTOCOL.md takes precedence over PLAN.md's older "deduplicated by media ID"
shorthand: identity is `(authenticated sender device_id, transfer_id)`, with all
known decoded metadata included in equality. Inbound text reserves its ID against
file commits before touching the clipboard, then finalizes its bounded hashed
history after success. Failed/cancelled effects release the claim; startup drops
unfinished claims. Notification/viewer failures never
undo a durable file receipt; clipboard text OS denial produces a retryable NACK.

Unpairing cannot revoke a TLS handshake already in flight by pin deletion alone:
the daemon rechecks its binding while registering READY and refuses registration
while unpairing. Local and remote unpair remove trust/dedupe transactionally,
signal closure only after persistence succeeds, and preserve completed user files.
Failure closes without UNPAIR, preserving both sides' binding. A durable pairing
generation changes with each committed pair/unpair. Sessions and local commands
capture it before scheduling work; one transaction helper rejects revoked file,
text, outgoing and receipt writes, including reuse of the same device ID. READY
registration captures the generation and action scope under the pairing gate;
connection work never looks up a later scope by device ID. Unpair/re-pair discard
pending automatic work and cancel the admitted action scope. One scope helper
checks cancellation and the captured connection generation immediately before each
clipboard text/image, notification or viewer side effect, after acquiring the
action mutex and after storage or earlier-action awaits. Replaced sessions cannot
initiate delayed desktop actions. Only admission of a newer connection advances
that generation; an ordinary disconnect preserves completed-transfer actions and
admitted clipboard text history. Completed-file actions are queued before ACK so
an immediate sender disconnect cannot interrupt their admission. Failed unpair
persistence retains trust, clears the admission latch and restores a fresh action
scope even if its caller was cancelled; failed replacement preserves the old
connection, action scope and invitation. Outgoing clipboard timestamps are
assigned inside the enqueue transaction, and replacement retains only the greater
`created_at` so concurrent IPC requests cannot overwrite newer pending text.

UI folder changes and tray toggles use field-specific Core mutations under the
config lock. A portal folder request runs alongside window status/QR polling and
is closed when the Pneu window closes. Open Folder falls back to `xdg-open` when
the portal is absent or fails. Ordinary image copy-failure notifications retain
the received descriptor for their Open action. Window icons are recolored from
the actual window palette, including custom schemes. Production Tokio workers
and the window IPC thread use 8 MiB stacks; connection/session/action/UI futures
are boxed at task boundaries to reduce nested async stack usage.

Failed-save retention follows DESIGN.md's cache-preview requirement despite
PROTOCOL.md's older no-retained-copy sentence. Repeated failures retain one copy
and perform desktop effects only on its first placement; a successful retry
retires the cached copy after committing the folder receipt.

Shutdown claim cleanup captures plain data rather than another RAII claim, so a
canceled blocking task cannot recursively schedule its own cleanup. A debugger
capture of the parallel-test overflow identified that recursion; a queued-cleanup
runtime-shutdown regression exercises it with one occupied blocking worker.

Phone freshness is persisted on disconnect and orderly shutdown with generation
checks, including unpair failure/cancellation callbacks. Session replacement and
all pairing/session endings reset both active transfers and receiving grace.
Folder recovery clears only the resolved folder's error; listener recovery clears
only port errors. Transfer progress does not drive SVG rendering or tray updates:
the tray refreshes icons on theme changes or five-second timer deadlines, and the
window caches rendered icons until its theme configuration changes.

Outgoing receipt timers start on the writer's flushed text frame or FILE_END
completion, never while queued or streaming image chunks. Frame write/flush
progress remains bounded separately. The store explicitly releases its flock
when the final Store owner ends: a concurrent child between fork and exec cannot
prolong ownership merely by inheriting a CLOEXEC descriptor.
