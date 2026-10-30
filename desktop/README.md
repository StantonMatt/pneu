# Pneu desktop core

`pneud` 2.2.1 is the Linux, macOS and Windows daemon; `pneu-sim` is a legacy
phone transport simulator. Shared syntax and framing live in `pneu-proto`;
the [wire protocol](../docs/PROTOCOL.md) defines mesh negotiation and delivery.
Nothing in this directory installs automatically.

Linux releases include a .deb and a prebuilt tarball with a per-user installer.
Download them from the [latest release](https://github.com/StantonMatt/pneu/releases/latest).
See the [Linux guide](../docs/linux.md) for installation, uninstalling, package
building and source-install precedence. Supported: Plasma 6 Wayland; GNOME/Mutter
lacks data-control, so file transfers can run but automatic clipboard sync and
clipboard writes are unavailable (sources in the guide).

Linux uses KDE Plasma 6 on Wayland with a Slint/Qt window and tray. Builds need
Rust 1.93+ and Qt 6 development tools (`QMAKE=qmake6`). On macOS 15.4+,
`./install-macos.sh` builds and installs `Pneu.app`, with a menu bar and Slint
window. Apple Silicon is tested; see the [macOS guide](../docs/macos.md) for
requirements, Open at login, native clipboard, notifications and camera pairing.
Windows uses a native tray, clipboard and toasts with a Slint/winit window. It
works from source and as a sideloaded MSIX; the Microsoft Store listing is coming.
See the [Windows guide](../docs/windows.md) for the MSVC toolchain, packaging,
StartupTask and native testing.

From the repository root on Linux:

```sh
cd desktop
export QMAKE=qmake6 # Qt 6 development tools are required for the native backend
heavy cargo test
heavy cargo clippy --all-targets -- -D warnings
cargo fmt --check
heavy cargo build --release -p pneud -p pneu-sim
```

Run `pneud run` to start the daemon and tray/menu bar. On Windows, `Pneu.exe`
opens the window and starts the daemon if needed; `pneud.exe` is the console CLI.
Tray activation or
`pneud window` opens the desktop window and starts the Linux user service if needed. Use **Add Device… → Phone** to show a
QR for the phone. **Add Device… → Computer** offers **Show Pairing Code** and
**Paste Link**, plus **Scan QR Code** on a Mac with a camera. Each computer can
pair with several computers and phones; adding one device preserves the others.

`pneud pair` remains available for headless use and prints the URI,
expiry in UTC milliseconds, and a terminal QR. Treat the URI as a secret; the
service never logs it. An invitation expires after five minutes. Reuse, TLS admission
and pairing completion share one predicate: both wall-clock and monotonic expiry
must remain in the future. `pneud close-pairing` also invalidates it. The CLI and
window share a pairing invitation
with at most three interface addresses. Linux selects them in three rounds
(other platforms are described in the [macOS guide](../docs/macos.md#discovery-and-host-metadata) and [Windows guide](../docs/windows.md#discovery)):

1. One best address per eligible interface: IPv4 if present, otherwise IPv6 with
   global, non-temporary addresses preferred. Interfaces are ordered Tailscale
   IPv4, default-route, then other physical LAN interfaces.
2. Each dual-stack interface's best global, non-temporary IPv6 address, in the
   same interface order.
3. Remaining aliases, until the three-address limit is reached.

Docker/bridge/veth/virbr, loopback and link-local addresses are excluded. mDNS
advertises every eligible address and refreshes on network changes within a
minute; DNS address records themselves are unordered.
Discovery failures are reported and retried without stopping the TCP daemon; an
empty candidate list fails the pairing command. Addresses never establish trust.

```sh
pneu-sim --state /absolute/path/to/phone-state pair 'pneu://pair?...'
pneu-sim --state /absolute/path/to/phone-state send-file capture.png screenshot image/png
pneu-sim --state /absolute/path/to/phone-state clipboard 'hello'
pneu-sim --state /absolute/path/to/phone-state listen 120
pneud status
pneud clipboard DEVICE_ID 'hello from the PC'
```

For desktop-to-desktop CLI pairing, `pneud join` reads the invitation from
standard input. Use `join-status ATTEMPT`, `join-confirm ATTEMPT` and
`join-cancel ATTEMPT` to follow or resolve the returned attempt. Transfer the
link through a trusted channel; do not put real invitation secrets in shell history.

Send files from **Send Files…**, Dolphin's **Send with Pneu**, the Mac menu bar,
or the CLI:

```sh
pneud send --to DEVICE_ID /absolute/path/to/file
pneud send --to FIRST_DEVICE_ID --to SECOND_DEVICE_ID /absolute/path/to/file
pneud operation-status OPERATION_ID
```

Without `--to`, `pneud send FILE...` follows the desktop's recipient-picker and
offline-queue settings. Explicit targets can queue while offline. Files are
snapshotted into a durable per-recipient outbox, with frozen pairing identities;
reconnects retry the same bytes and IDs. Clipboard sends require live sessions.

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

`install.sh` on Linux builds the release daemon (through `heavy` when available), installs it into
`~/.local/bin`, installs the app icon and desktop entries, writes the systemd user unit, and reenables it for `graphical-session.target`.
An active graphical session starts/restarts the daemon immediately, including
reinstalls; otherwise it starts with the next graphical session. `./install.sh --uninstall` removes the per-user installation without rebuilding
or deleting Pneu data. Run the installer only when installation is intended. The unit uses an owner-only umask, follows the
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
  binding, receipts and pending deliveries for that device, then sends best-effort
  UNPAIR and closes.
  Persistence failure closes without UNPAIR and retains trust for retry/reconnect.
  Saved user files remain.
- `config()` returns the current configuration. `update_config(Config)` validates,
  writes it durably, and applies toggles. Port changes are rejected with
  "changing the port takes effect after restarting pneud"; edit the configuration
  file and restart for a new port. `pneud reload` applies other edited settings.
  The configured port is available to IPC immediately after Core construction.
- `send_clipboard(Id, String)` sends text to that device's current authenticated
  session and returns its operation ID. `share_clipboard(Targets)` reads the
  current selection for selected connected devices. Neither is a durable offline
  clipboard queue. `discard_clipboard(Id)` drops pending clipboard work for that
  peer. Mesh clipboard ordering uses the device-wide `(hlc, source_device)` register.
- `peer_settings(Id)` reads synced directional switches; `set_peer_setting(...)`
  persists an edit and schedules synchronization. Pair switches cover each
  clipboard direction and phone-to-computer automatic media. All start enabled.
- `paired_devices()` and `connected_devices()` return device IDs. `subscribe()` returns a bounded Tokio
  broadcast receiver for `Event`: connection changes, transfer start/progress/
  finish, received/sent clipboard operation IDs, config changes and errors.
  Events never include pairing secrets or clipboard contents. Handle lag by
  refreshing state; events are observations, not an authorization source.
- `request_shutdown()` stops the server; await `serve` to join connection workers.
  `shutdown_actions()` can also explicitly close desktop integrations.

The CLI uses bounded newline JSON `ipc::Command` requests. Unix sockets are
owner-only and uid-checked; Linux uses `$XDG_RUNTIME_DIR/pneud.sock`. Windows
uses a protected, user-SID-namespaced named pipe with peer security/session
checks. Platform paths and boundaries are in the [macOS guide](../docs/macos.md#paths-and-overrides)
and [Windows guide](../docs/windows.md#storage-and-ipc-security). Rust callers can
use `Core` directly. A `pneud ui-window` child keeps window/backend failures
separate from transport: on demand on Linux/Windows, persistently supervised on
macOS. Linux uses Slint/Qt and the XDG portal; macOS and Windows use Slint/winit
with native integrations. Closing the window leaves transfers running. Core owns one short-lived invitation shared by CLI and window.

The clipboard actor owns observation, ordering, echo suppression and frozen
recipients. Linux observes Wayland CLIPBOARD offers; macOS observes NSPasteboard;
Windows uses Win32 clipboard format notifications and a killable reader helper.
Reads and OS effects run outside the actor. Password/sensitive markers suppress
sharing, and received content is not automatically forwarded. Disconnect or a
policy change invalidates unsent work; an already-started frame may finish.
Text and images are connected-only, including explicit sends. Automatic clipboard
sharing defaults on; **Include images** defaults on and requires the receiver's
`clipboard_image` capability. Outgoing clipboard images are bounded to 10 MiB.
See the [main README](../README.md#settings) for the current settings and defaults.

On Linux, tray SVGs are embedded and materialized into the data directory at startup. The default ARGB fallback uses kdeglobals'
active Plasma desktop theme palette (or the window scheme for adaptive themes), refreshed by KGlobalSettings
`notifyChange` and periodically as a fallback. `PNEU_TRAY_NAMED_ICONS=1` enables
the named-icon spike; `PNEU_KDEGLOBALS` selects an isolated color configuration.

## Durability, tasks and desktop actions

Trust, receipts, settings and queued deliveries belong to individual peers.
Unpairing or replacing one binding leaves the other peers intact and preserves
saved user files. Per-peer binding and connection generations reject stale
work; mesh links resolve simultaneous computer dials deterministically.

SQLite uses WAL, `synchronous=FULL`, private files and foreign-key cascades.
It stores peer bindings, file receipts, pair settings and their synchronization
state, clipboard clocks/hashed history, outgoing file operations, failed-save
cache references and previously used staging folders. Clipboard payloads are
never a durable offline outbox. Desktop file snapshots are bounded to 2 GiB and
256 pending deliveries globally, 1 GiB and 128 deliveries per peer, with a
64 MiB free-space reserve.

Calls use one connection mutex in Tokio's blocking pool. Shutdown drains tracked
blocking work and cancellation cleanup. Streaming transfers never buffer a whole
file. Bounded reader, writer, file and text queues await capacity, applying TCP
backpressure. Once read, PING schedules PONG ahead of queued outgoing frames;
a storage stall can delay reading until the peer's PONG deadline. All writes
flush within their deadlines. Idle file workers have no transfer deadline.
The daemon allows sixteen established peer sessions, with separate headroom for
sixteen inbound and four outbound candidate handshakes (36 transport slots in
total). Unknown pairing admission is limited to once per second.
The dialer drives periodic PINGs; the acceptor answers them. Cache pruning runs
hourly. Successful retries
retire failed-save previews by reopening without following links and checking
recorded platform file identity, size and modification/change times, relative
to the original directory handle. The evidence holds no file descriptors, so
pruning reclaims blocks even without a retry. Evidence is bounded to 128 entries;
after restart or eviction, previews remain for normal retention pruning.
Staging, temporary-config and runtime-socket cleanup also check identity and
use retained directory handles, preserving replacement entries.
Tray icons use the same anchored atomic writes and refuse symlinked descendants.

TLS is 1.3 only, with resumption/0-RTT disabled. Both verifiers check P-256,
certificate self-signature, validity dates and rustls's real ECDSA P-256
CertificateVerify. Phones also check the exact DER PC pin during TLS. The daemon
completes TLS for cryptographically valid client certificates, then checks their
fingerprints before reading any protocol frame. A client neither paired nor
admissible under an active invitation receives `ERROR unauthorized` with a short
message and closes within the two-second fatal-close bound. Phones can recognize
this authenticated rejection from their pinned PC as "Pairing lost" after offline
unpairing. The simulator reports `unauthorized` explicitly, including before PC
HELLO. Pairing has the prescribed HELLO/restricted state, constant-time token
comparison, error ordering, durable binding and one-time
consumption. Before sending PAIR_REQUEST, the simulator persists the PC pin, addresses and
token. A lost success reply keeps this pending record; reconnect repeats the
request even after invitation expiry. An already-bound certificate/device gets
accepted=true without checking the old token, then the phone persists its binding.

Receive files use directory handles for staging, rename and cleanup, streamed
SHA-256, and no-replace placement through the platform filesystem layer (Unix
`rustix::fs::renameat_with`, Windows `NtSetInformationFile`). Commit order is
staging file fsync,
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

Mesh clipboard text and image effects share one persisted device-wide ordering
register. Recent operation fingerprints suppress replays and clipboard-manager
echoes; legacy connections retain timestamp ordering. OS denial NACKs
`clipboard_unavailable`; a crash after an OS effect but before its receipt is
committed can repeat that effect. Events and logs contain no clipboard payloads.
Completed file IDs have no expiry until unpair; cached files expire with a
one-day default retention.

`PcActions` isolates OS integration from protocol tests. The following desktop
actions describe Linux; see the [macOS guide](../docs/macos.md) and
[Windows guide](../docs/windows.md) for native equivalents and their limits.
Linux uses zbus notifications with the approved text, actions, urgency and transient hints.
File notifications use `x-kde-urls` for Plasma thumbnails; explicit clipboard
images use a bounded inline image preview. Notification Open/default uses only
its retained receive descriptor, including after the pathname is replaced or
removed. Show in folder navigates the recorded pathname with FileManager1.ShowItems.
Recent Open (both received and sent files) opens the current regular file at the
recorded path, rejecting symlinks in every component. Its opened descriptor goes
to the portal without an outbox size quota; edits and replacements are allowed.
Recent is a navigation shortcut, not an identity guarantee. Arrivals within
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
scope or connection generation is revoked. Unpair confirmation retains the device
ID and name displayed when it opened, so another pairing cannot retarget it.
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
Clipboard installation is confirmed through the watcher and the actor's origin
ticket. Recent keyed content fingerprints also suppress echoes after helper
replacement; failed writes do not claim a successful clipboard installation.

Manual clipboard verification uses the same Wayland library:

```sh
cargo run -p pneud --example clipboard_probe -- capture.png image/png
```

It reads the current selection, streams a digest, and compares it to the file.
A manual run with isolated XDG paths needs an absolute `WAYLAND_DISPLAY` pointing
to the real compositor socket, because its runtime directory is temporary.

## Specification decisions

The [protocol](../docs/PROTOCOL.md) defines the wire contract; the
[plan](../docs/PLAN.md) records current scope and historical milestones.
Operation identity is
`(authenticated sender device_id, transfer_id)`. Mesh is negotiated only when
both HELLOs advertise `mesh`; legacy-compatible connections keep v1 wire
behaviour. A feature requiring an update does not revoke trust or discard queues.

Local and remote unpair remove trust and dedupe state transactionally before
best-effort UNPAIR/closure. Persistence failure retains the binding for retry.
Sessions and queued actions retain their captured peer generation, so replacement
or unpair cannot retarget them. Ordinary disconnect preserves completed-file
actions; clipboard deliveries cannot migrate into a new session.

Pair settings use stamped registers and full-state snapshots. Offline edits
remain pending until the peer connects. Settings synchronize before new content;
file deliveries blocked by a disabled direction wait, while unsent clipboard
text is dropped. Desktop settings receipts retain a rolling window of 4096
snapshots per sender; eviction permits safe re-merging rather than imposing a
lifetime quota.

Linux folder changes and tray toggles use field-specific Core mutations under
the configuration lock. Portal requests bind to their issuing bus owner and
close on cancellation. Notification activation tokens are consumed for their
associated action; missing tokens are not inherited from an earlier action.
Tray launches have no activation token and remain subject to KWin focus
prevention. Choose Folder may also open behind other windows.
