# Pneu desktop core

`pneud` is the PC daemon; `pneu-sim` exercises the phone transport before Android
is available. The wire contract is `../docs/PROTOCOL.md`, with the shared syntax
and framing in `pneu-proto`. Nothing in this directory installs automatically.

```sh
cd desktop
# This workspace's dependency cache is writable in the worker sandbox.
export CARGO_HOME="$PWD/target/cargo-home"
heavy cargo test
heavy cargo clippy --all-targets -- -D warnings
cargo fmt --check
heavy cargo build --release -p pneud -p pneu-sim
```

Run `pneud run`, then `pneud pair` in another terminal. Pairing prints the URI,
expiry in UTC milliseconds, and a terminal QR. Treat the URI as a secret; the
service never logs it. An invitation expires after five minutes. `pneud
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
`~/.local/bin`, writes the systemd user unit, and reenables it for `graphical-session.target`.
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
  for the QR contents. `close_pairing()` is the pairing window's close handler.
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
IPC. There is no clipboard watcher in this core: a future event-driven desktop
watcher can call `send_clipboard`, suppressing changes applied by `PcActions`.
PC-to-phone images are outside this core command API; the v1 plan postpones PC
file sending. The tray, pairing/settings window, and final notification copy
remain the UI follow-up.

## Durability, tasks and desktop actions

The PC pairs with one phone. Accepting a valid invitation replaces the old pin,
binding, dedupe and pending text while preserving saved files. Newest authenticated
connection wins; a connection ID suppresses stale events from the old socket.
Unpair marks the lifecycle before closing/forgetting, so replacement cannot slip
in while trust is being removed. Pairing epochs cancel queued and running desktop
actions and the viewer debounce.

SQLite uses WAL, `synchronous=FULL`, owner-only files and foreign-key cascades.
It stores the single binding, completed file receipts, clipboard timestamp and
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
PC initiates no periodic PINGs. Cache pruning runs hourly.

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
seven-day default retention.

`PcActions` isolates OS integration from protocol tests. Production uses zbus
notifications with a bounded inline image preview, Open/default and Show
in folder actions; Show uses FileManager1.ShowItems. Open passes the retained
file descriptor through the desktop portal's OpenFile method (no pathname
fallback). Clipboard bytes and previews use the same received-file descriptor,
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
