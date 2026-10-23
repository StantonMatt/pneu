# macOS daemon (M1)

M1 runs `pneud run` headless on macOS 15.4 or later. It uses the same Core,
TLS transport, pairing, file outbox, receipt store and clipboard actor as Linux.
A native Rust build is required. No `.app`, LaunchAgent, login item, menu bar,
window, picker, notification delivery or camera scanner is installed by M1.

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
M1 does not link Slint into the Mac daemon. AppKit is used for NSPasteboard
without creating an NSApplication or window.

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
palette or locale file is read on macOS. Native appearance and the user's system
time-format preference belong to M2; M1's shared last-seen formatter uses a
24-hour placeholder.

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
produce a system permission prompt. Unavailable desktop notifications are logged
without pretending to deliver them; open/reveal/window actions return an explicit
M1-unavailable error. Window/picker IPC requests (including sends requiring a
recipient picker) fail before queuing UI work. Direct `SendFiles` IPC and
`pneud send --to DEVICE FILE...` remain available, including relative paths
resolved against the CLI's working directory. Untargeted `pneud send` awaits M2's
recipient picker. Send parser errors return their original message over IPC to
the CLI when no window is available, rather than replacing the cause with the
M1 UI-unavailable error. Picker/tray/window
supervision is a cfg-selected runtime
boundary ready for the persistent M2 UI child.

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
