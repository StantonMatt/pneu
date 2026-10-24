# macOS

Pneu runs on macOS 15.4 or later. The headless `pneud run` daemon owns Core,
TLS transport, pairing, file admission, receipt storage and the clipboard actor.
One supervised `pneud ui-window` child hosts the native menu bar, notifications,
file pickers and shared Slint window using winit. Closing the window keeps the
menu bar and transfers running. Add Device → Computer also offers Scan QR Code
when AVFoundation discovers a camera.

## Install

M2 packages the native daemon and its supervised UI child as an ad-hoc-signed
`Pneu.app` (macOS 15.4 or later). Run from the `desktop` directory on the Mac:

```sh
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$HOME/.local/bin:$PATH"
./install-macos.sh
```

This builds a release bundle through `heavy`, installs it at
`~/Applications/Pneu.app`, links `~/.local/bin/pneud` to its executable, and
starts Pneu. Add `~/.local/bin` to PATH to use the CLI. Re-running the installer
upgrades the bundle after stopping the session LaunchAgent and waiting for the
daemon and UI child to exit. It preserves the Open at login setting.
`./install-macos.sh --system` selects `/Applications`; run as your logged-in
user with write access to that directory, never with `sudo`.

Install, upgrade and uninstall check every process named `pneud` owned by the
current user, including foreground daemons and instances using custom paths.
They refuse to replace the bundle or delete state while any unmanaged Pneu
process remains. Stop a foreground `pneud run` with Ctrl-C and retry. These
scripts never signal an unmanaged process; an unrelated executable also named
`pneud` must finish before installation can proceed.

Open Pneu from Finder or Spotlight to show its window. A no-argument launch
requests `ShowWindow` from the running daemon, or starts a session LaunchAgent
and then shows the window. `pneud run` remains the foreground daemon command.
Malformed configuration produces a startup error and does not enter a restart
loop. Daemon startup errors are recorded at `run/pneud.log` beside its IPC socket;
opening the app also presents startup errors in a dialog.

**Open at login** under **Settings → This Mac** defaults to on at the first
installation/open. It uses `SMAppService.mainAppService`, independently of the
current session's daemon. Turning it off leaves Pneu running; reopening the
app keeps the setting off. Turning it on registers the app for future logins.
The bundle’s `PneuLoginItemInitialized` macOS preference records that its
one-time default has been applied. Subsequent opens respect changes made in
System Settings, including when Finder uses different paths from a shell.
Registration can display a “background item added” notification. If approval is required,
allow Pneu in **System Settings → General → Login Items**. Inspect/change it with:

```sh
pneud login-item status
pneud login-item register
pneud login-item unregister
```

The daemon uses a separate LaunchAgent bootstrapped only into the current
`gui/<uid>` session. Its private plist is `run/daemon-session.plist`; it is never
placed in `~/Library/LaunchAgents` or registered through SMAppService. Logout
removes the job from launchd. After a stopped app is moved, startup reloads a
session job whose old executable is missing. A running job or another live
bundle is retained and startup reports the conflict. Only the main app's login item can recreate it at
the next login. `KeepAlive { Crashed: true }` restarts signal crashes, while
ordinary nonzero exits (including configuration errors) remain stopped.
Both KeepAlive itself and SuccessfulExit imply RunAtLoad according to
`man launchd.plist`, so removing an explicit RunAtLoad key from a persistent
agent would not implement the off setting. The session-only agent intentionally
uses RunAtLoad; login startup belongs exclusively to the main app.

Build without installing or registering anything:

```sh
./packaging/macos/build-app.sh --profile debug --out "$HOME/Developer/pneu-build/bundles"
codesign --verify --strict "$HOME/Developer/pneu-build/bundles/Pneu.app"
```

Use `--profile release` for an optimized build. Packaging creates `AppIcon.icns`
with `iconutil` from `assets/macos/AppIcon.iconset`, copies `assets/macos/menubar-*.png`
images (including Retina variants) into `Resources/menubar/pneu-tray-*.png`, lints
the Info.plist and daemon template (`Resources/daemon.plist.in`), signs ad-hoc
with the hardened runtime and camera entitlement, and verifies the signature. The executable renders
the session plist with its current absolute bundle path and explicit Pneu paths.
If designer icons are absent the builder reports a temporary SVG-derived icon.
Ad-hoc signing provides no Developer ID or notarization.

For isolated login acceptance, use `--bundle-id io.github.stantonmatt.pneu.test.pkg`
(or `.test.pkg2`), keep the bundle in a private build directory, and override all
four `PNEU_*` directories. Use an explicit non-default port, clipboard mode off,
and a private receive directory. Unregister the test main-app login item and
boot out its test session LaunchAgent afterwards; verify both are absent.
Do not register the production bundle for tests.

Uninstall using the same location flag as installation:

```sh
./uninstall-macos.sh
# Or, for an installation in /Applications:
./uninstall-macos.sh --system
```

Uninstall stops the session agent, refuses if other Pneu processes remain,
unregisters the login item, and removes the bundle and CLI link. User data stays
in `~/Library/Application Support/Pneu` and `~/Library/Caches/Pneu`. Add `--purge`
to unlink Pneu's explicit state/cache entries (DB and sidecars/lock, identity,
fingerprint key, config, runtime files, outbox artifacts, cache files and viewer
leases), without following symlinks. Directories are removed only when empty;
unknown files and received folders are kept, with a message. Bundle removal also
uses an explicit entry list. Upgrade refuses a bundle containing extra files;
move those files before upgrading. No receive-path overlap comparison is used.
Without an installed bundle, `--purge` requires reinstalling its entry remover.
The macOS initialization preference is retained,
so reinstalling respects login-off. Received files are retained; custom
`PNEU_*`/XDG data directories are never removed by this script.

## Paths and overrides

| Purpose | Default |
| --- | --- |
| Identity, pairing and SQLite state | `~/Library/Application Support/Pneu` |
| Configuration | `~/Library/Application Support/Pneu/config.toml` |
| Cache | `~/Library/Caches/Pneu` |
| IPC | `~/Library/Application Support/Pneu/run/pneud.sock` |
| Received files | `~/Downloads/Pneu` |

The runtime directory is created with mode 0700 before binding. Socket paths
must be absolute, have only root/daemon-owned ancestors, and be at most 103 bytes (Darwin's 104-byte `sun_path` includes
its NUL). An overlong path fails with an actionable error; it never silently
falls back to a public directory.

`PNEU_DATA_DIR`, `PNEU_CONFIG_DIR`, `PNEU_CACHE_DIR` and `PNEU_RUNTIME_DIR`
override the corresponding directories. Existing `XDG_DATA_HOME`,
`XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and `XDG_RUNTIME_DIR` test tools also work;
`PNEU_*` takes precedence. XDG data/config/cache bases retain their `/pneu`
suffix. Every IPC ancestor is opened without following links and checked for ownership,
mode and ACLs. Group/other-writable ancestors are refused unless sticky; ACL
allow entries on ancestors are conservatively refused (deny-only ACLs are allowed).
A private leaf below a non-sticky writable shared directory is therefore refused.
Override every directory for private acceptance tests. The config's
`save_path` and non-default `port` must also be explicit.

Linux retains its original XDG layout, Pictures/Pneu receive default, Wayland,
D-Bus, ksni and Slint/Qt behavior. Linux-only dependencies are target-scoped;
The daemon never creates an NSApplication or window. Its NSPasteboard adapter
remains independent of the child; all rendered UI runs on the child's main thread.

## Storage and IPC security

Directory traversal uses retained directory fds and `openat` with
`O_DIRECTORY | O_NOFOLLOW`, one component at a time, on both platforms.
Staging checks effective uid, private mode and extended ACLs, uses exclusive
0600 files, and checks device/inode identity before deletion. Pneu removes
inherited ACLs from newly created private directories/files through `acl_set_fd`
and verifies the empty ACL before writing sensitive bytes. This covers identity,
fingerprint keys, atomic configuration publication, store/lock files, staging,
outbox and runtime directories/locks. Existing private objects with ACL entries
are rejected rather than silently repaired; even owner-only or deny-only ACLs
on private objects are conservatively refused. Directory inheritance is removed
before SQLite creates WAL/SHM/journal sidecars; existing sidecars are checked
before opening SQLite. ACL checks use retained descriptors, never a chmod of a
pathname. Modes remain 0700 for private directories and 0600 for private files
(outbox snapshots become 0400 after staging). Writes, fsync,
no-replace placement and cleanup remain descriptor-relative. Symlinks in
receive-folder paths are refused, including intermediate components.

Darwin provides `O_NOFOLLOW_ANY`, `O_SYMLINK`, `O_SEARCH`/`O_EXEC` and
`fcntl(F_GETPATH)`. M1 uses the existing component traversal instead of an
absolute `O_NOFOLLOW_ANY` open: it also anchors creation and later operations.
`O_SEARCH` is usable for directory search but not enumeration; `O_EXEC` is not
a general metadata-only replacement for Linux `O_PATH`. The existing read-only
directory handles are retained. `F_GETPATH` supplies display/recovery paths;
M1 reopens without following links and verifies dev/ino before returning them.
It does not use the display path to authorize storage mutation.

For private staging cleanup, `O_EVTONLY | O_SYMLINK` opens regular files and
symlinks themselves without following the latter. Darwin refuses opening
socket vnodes this way; an unexpected socket in staging fails cleanup closed.
Linux continues to use `O_PATH` for metadata handles.

The macOS SDK has no `bindat` or `connectat`. `connectx` has no directory-fd
anchoring equivalent. IPC therefore retains a no-follow directory fd, requires
same-uid ownership and mode 0700, holds a no-follow private regular flock file,
validates the ancestor chain before bind/probe, checks the named parent's
dev/ino before and after bind, and compares the socket entry's dev/ino before and
after descriptor-relative `fchmodat(AT_SYMLINK_NOFOLLOW)` sets 0600. No
pathname-following chmod is used. Connections
still require `peer_cred().uid()` to match the daemon uid. Stale sockets are
removed only after an owned-socket check and `ECONNREFUSED`; foreign entries,
symlinks and active listeners are preserved. Shutdown unlinks only the original
entry relative to the retained directory, holding a cloned listener until then. Darwin's
[`unp_bind`/`unp_detach`](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/uipc_usrreq.c#L1253)
retain/release the bound vnode; the clone keeps that reference alive during
owner cleanup. This does not pin pre-existing stale socket entries.

**Weaker than Linux:** absolute-path bind/probe have a residual race with a
same-uid process or root able to rename/replace a parent between checks. The
ancestor ownership/mode/ACL checks exclude foreign-uid rename rights; private
leaf permissions alone would not establish that boundary.
Linux anchors these operations through `/proc/self/fd`. Stale Darwin socket
entries cannot be pinned by opening their vnode, so stale removal has an
additional stat/identity-check/unlink race with a same-uid actor. A 0700 runtime
directory excludes other uids from its entries, but does not make hostile
ancestor renames impossible. Postchecks detect persistent replacements, not an
adversarial swap-and-restore. No equivalent Linux anchoring guarantee is claimed.

## Discovery and host metadata

`if-addrs` enumerates native interfaces using `getifaddrs`. `en` plus a numeric
suffix identifies Wi-Fi/Ethernet candidates; awdl, llw, bridges and arbitrary
utun interfaces are excluded. A Tailscale 100.64/10 address remains explicitly
eligible even on utun and is prioritized for invitation address hints. Existing
mDNS advertisement for phones remains enabled; transport/pairing never depends
on mDNS crossing subnets.

IPv6 flags come from Darwin `SIOCGIFAFLAG_IN6` (`in6_ifreq`, SDK size 288,
`IN6_IFF_TEMPORARY = 0x80`). Temporary addresses are omitted. An ioctl failure
conservatively omits that IPv6 address rather than advertising a potentially
temporary one. M1 does not read Linux route tables or rank Mac default-route
metrics: eligible physical interfaces retain deterministic ordering.

The default name is `scutil --get ComputerName`, falling back to uname. No KDE
palette or locale file is read on macOS. The window follows the system light/dark appearance. Last-seen times use the
system 12/24-hour preference, including explicit macOS clock overrides.

## Clipboard

A dedicated thread polls `NSPasteboard.changeCount` every 25 ms. Startup and
watcher restart take their first metadata snapshot under the publication lock.
They read an existing origin marker to confirm an active installation that may
have finished before initialization, but never read or publish its payload.
Unknown startup tickets are retired without payload fallback. Ordinary existing
contents remain a metadata-only baseline. Access recovery after `AlwaysDeny`
uses the same initial-marker path, even if the denied poll saw the same count.
Each changed selection captures
one clipboard-actor observation token and its
frozen recipient policy before reading either the origin marker or payload.
After initialization, retired or unknown markers advance to payload reading under that same token;
connections or policy expansions during the marker read cannot add recipients.
A generation change during reading retires the snapshot. Provider reads
(including the origin marker), image decoding/conversion and result decoding run
outside observation.
At most one supervised `pneud pasteboard-read` subprocess is active per watcher;
a changed selection, actor cancellation, shutdown or a five-second deadline kills
and reaps it. The deadline covers the entire observation, including both reads.
A finishing child is reaped before its replacement is admitted. Each observation
completes exactly once: payload success/failed reads complete the actor read;
replacement, cancellation, shutdown and confirmed install markers retire it
without originating an opaque local change. Discarding a child also releases its
actor ordering barrier, including a held observation behind a manager echo.
A nonblocking bounded result pipe prevents shutdown from depending on a provider
releasing its data. These internal helpers do not start Core or access daemon data. The watcher retains raw representation hashes for the existing
keyed echo-history machinery.

`org.nspasteboard.ConcealedType`, `org.nspasteboard.TransientType` and secret
hints invalidate local sharing without reading their payloads. Text, PNG, JPEG,
GIF, WebP and BMP are supported. Images use the existing bounded decoder;
non-PNG images get a PNG representation for the actor. Original representation
hashes are retained too. Text and image limits are checked before Rust copies;
AppKit/provider allocation of an NSData itself is outside that Rust read cap.

Preparation freezes all representations and hashes without touching the OS.
After actor authorization, installation synchronously publishes one
`NSPasteboardItem` containing every type plus the private
`io.github.stantonmatt.pneu.origin` type, whose value is the 32-hex-digit install
ticket. AppKit retains bytes directly, so no serving helper is needed. A
successful `writeObjects` call is not completion: the independent watcher must
read the same ticket and confirm through `Core::clipboard_install_marker`.
Retired tickets are reobserved with content hashes so the actor can repair late
installs. Clearing the old pasteboard and publishing the item are two AppKit calls,
serialized against watcher metadata snapshots with a shared publication lock.
The watcher cannot submit the intermediate empty selection. An unreadable,
malformed or wrong-length origin marker invalidates sharing. Policy transitions
are tracked independently of log suppression, including Allow/marker/Deny
sequences with an unchanged selection.

`AlwaysDeny` invalidates local sharing and logs a clear access-not-allowed state.
`Default` and `Ask` get at most one payload-reading job per changed selection;
an origin marker gets one separate bounded read before payload admission.
Nil/denied reads invalidate that observation and are not retried until another copy; they never
cause a crash or prompt loop. M1 does not change access policy. These modes can
produce a system permission prompt.

## Persistent UI and local IPC

The daemon starts the child from its own executable and restarts it with a
250 ms to 30 s backoff. A child that runs for a minute resets that backoff.
An initial handshake must finish within 15 s, after the child's main loop starts.
A missing acknowledgement revokes readiness and restarts the child. UI failure
never shuts down Core.
The clipboard watcher remains in the daemon.

Snapshots, settings, pairing, focus leases and send admission use the existing
0600, uid-checked, bounded/paged local IPC. `PresentPicker` adds a macOS menu
handoff carrying frozen recipient authorities, clipboard/file mode and explicit
recipient scope into the same bounded pending-send queue as window/CLI sends.
The same handoff accepts a complete dropped-file selection. The child acknowledges
adoption with `AcknowledgeSend`; it cannot mutate Core storage.
UI-only requests fail before queuing when the particular daemon has no ready
child. Direct `pneud send --to DEVICE FILE...` remains available then, and parser
errors retain their original CLI message.

A separate inherited Unix socketpair carries bounded daemon-to-child frames.
Both peers check uid and exchange a fresh per-child capability before readiness.
There is no filesystem endpoint to reconnect to. Notification submission,
open/reveal, URL and window-show requests have sequence IDs and acknowledgements;
EOF revokes readiness and fails outstanding waits. Shutdown closes the channel
and allows up to three seconds for main-thread notification withdrawal before
killing and reaping a stuck child. Startup sweeps stale Pneu notifications from
earlier daemon namespaces as well as earlier children. Notification responses carry
a per-daemon namespace, notification ID and action ID back to a bounded
daemon-owned registry. A response from an earlier daemon session cannot consume
a current notification with the same numeric ID.
Unknown, expired or unoffered actions cannot act on another notification's file.
Viewer waits use the shared interaction budget outside Core's action gates.

Notification Open transfers the retained received-file descriptor with
`SCM_RIGHTS`; it never reopens the received display path. Since a viewer launched
by NSWorkspace cannot inherit the child's fd, the child copies from that fd with
positioned reads into a private, read-only materialization before asking macOS to
open it. Configured automatic image opens explicitly use Preview; notification
Open and Recent use the file's normal application. An unavailable cache disables
only descriptor-materialized opens; the menu bar still starts and later opens
retry lease creation. A per-child cache lease
retains those bytes until shutdown; the daemon
cleans the lease by retained directory identity even after a child is killed.
Recent first resolves the current regular file at its recorded path, rejecting
symlinks in every component, then opens that path directly through NSWorkspace
without making a cache copy. Recent is navigation to the current file, not an
identity guarantee.
Reveal uses paths only for Finder navigation.

Pickers use asynchronous NSOpenPanel requests; cancellation closes the panel.
Menu items retain immutable recipient device/generation/fingerprint payloads.
Login-item status uses one background worker, with at most one query in flight;
ServiceManagement XPC cannot stall the menu or window.
Native drag callbacks capture the complete file selection, recipient authorities
and NSDraggingInfo coordinates as one gesture, without inferring gesture boundaries
from winit's path-only events. Drop hit-testing uses the rendered, clipped and scrolled
Slint target list and frozen authorities. An unknown location or offline target
is rejected. System Settings links are available for Camera,
Notifications, Clipboard and Login Items.

## Camera pairing

Add Device → Computer discovers built-in and external video cameras without
requesting access; discovery is refreshed on entry and while the Computer rows
are visible. Scan QR Code requests macOS camera access only when opened.
NotDetermined waits for the system prompt; Denied and Restricted show
`Pneu can't use the camera` with Open System Settings → Privacy & Security →
Camera. Granting access there while the page is visible resumes capture.
Disconnected, busy or failed cameras stop capture and offer Try Again.

AVCaptureSession runs exclusively in the UI child on a serial session queue.
A separate serial output queue discards late frames, supplies a mirrored Slint
software-rendered preview at at most 10 fps, and runs a QR-only Vision request
at most every 300 ms. Frames are bounded to 1280×720 and held only in memory in
a single replaceable mailbox. Vision sees the original, unmirrored image.
Other QR destinations are ignored. The first bounded `pneu://pair` payload stops
the session before entering the ordinary Paste Link callback and Join IPC;
confirmation, own-code, expiry and already-paired behavior are shared.

Navigation, window close/hide, application hide, minimization, errors and quit
invalidate the scan generation and stop capture. Permission callbacks and frames
from older generations cannot restart a camera or navigate. Hiding/minimization
is checked against native window/application visibility every 100 ms. A new
session waits behind the previous session's stop; shutdown drains queued stops.
Camera images and scanned payloads are never logged or written by the scanner.

Linux-runnable tests cover authorization/availability mapping, payload admission,
generation fencing, and handoff through the existing Join confirmation/errors.
The native `vision_decodes_generated_still_through_capture_decoder` test generates
a QR image and runs the same Vision decoder without opening a camera. Private
`PNEU_UI_TEST` controls include `add-computer`, `scan-qr`, `scan-back`,
`assert-camera-available`, `assert-camera-preview`, `assert-camera-denied` and
`assert-camera-stopped`; production sessions reject these controls.

## Native verification and private acceptance

Use rustup before Homebrew on Apple Silicon:

```sh
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$HOME/.local/bin:$PATH"
ulimit -n 4096
export TMPDIR="$HOME/.cache/agent-scratch/pneu/mac-m1/tmp"
mkdir -p "$TMPDIR"
heavy cargo test -p pneud --locked
heavy cargo clippy -p pneud --locked --all-targets -- -D warnings
```

The larger shell fd limit is for parallel integration tests; the macOS SSH
shell defaults to 256. A canonical TMPDIR avoids Darwin's `/var` and `/tmp`
symlink aliases in tests intentionally rejecting symlink ancestors. The normal
suite never accesses NSPasteboard. The ignored
`ui::clipboard::tests::real_pasteboard_roundtrip_restores_contents` test is opt-in:
it requires AlwaysAllow, snapshots all readable types/items before any write,
uses a labelled synthetic string, and restores on unwind. Run it only when
brief real-pasteboard access is authorized.

`cargo run -p pneud --example headless_peer --locked` is a test-only daemon
harness for the Linux side of cross-host acceptance. It requires
`PNEU_HEADLESS_TEST=1`, all four explicit XDG roots and a non-47821 configured
port. It uses the production Core, TLS, store, file sender/receiver and owner-only
IPC; its clipboard backend stores only a SHA-256 plus copy count in memory.
`headless_peer status` combines real daemon status with the private diagnostic
socket (`pneu-test.sock`). It never starts a watcher, contacts a desktop bus,
reads the clipboard, advertises mDNS, or opens a desktop application.
