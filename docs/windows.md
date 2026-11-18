# Windows

Pneu runs as a per-user desktop app on Windows. It is available from the
[Microsoft Store](https://apps.microsoft.com/detail/9N48FLMGSDDV) and also works from source or as a sideloaded MSIX. The package targets x64
Windows 10 version 2004 (build 19041) or later. Native acceptance so far is on a
Windows 11 VM; physical-hardware acceptance remains pending.

The daemon owns Core, TLS transport, pairing, storage, the tray, clipboard and
notifications. An on-demand `ui-window` child hosts the shared Slint window.
This is an interactive-session app, not a Windows Service.

## Build and run

Use Rust 1.93 or later with the `x86_64-pc-windows-msvc` toolchain, Visual Studio
C++ Build Tools and the Windows SDK. Qt is not required: the Windows window uses
Slint's winit backend and software renderer. From `desktop` in PowerShell:

```powershell
cargo build -p pneud --features windows-package
./target/debug/Pneu.exe
```

- `Pneu.exe` uses the GUI subsystem. Opening it shows the existing window over
  authenticated IPC, or starts the daemon and opens the window.
- `pneud.exe` is the console CLI. `pneud.exe run` starts the foreground daemon;
  `pneud.exe window` opens its window. Both binaries share the same entry point.
- The `windows-package` feature enables the GUI binary; the console binary is
  available without it.
- StartupTask and Restart Manager launches run in the background without opening
  the window. Toast COM activation uses `-Embedding`.

See [main_windows.rs](../desktop/crates/pneud/src/main_windows.rs),
[main_unix.rs](../desktop/crates/pneud/src/main_unix.rs) and
[Cargo.toml](../desktop/crates/pneud/Cargo.toml).

## Paths and overrides

For an unpackaged build, the base directory is the Local AppData known folder
plus `Pneu`. For an MSIX build it is
`ApplicationData.Current.LocalCacheFolder` plus `Local\Pneu`. Pneu resolves that
real package path explicitly so NT handle-relative I/O and SQLite agree.

| Purpose | Location |
| --- | --- |
| Identity, pairings, SQLite state and outbox | `data` under the base directory |
| Configuration | `config\config.toml` under the base directory |
| Cache | `cache` under the base directory |
| Received files | Downloads known folder plus `Pneu` |
| Local IPC | Named pipe `\\.\pipe\pneu-<user SID>` |

`PNEU_DATA_DIR`, `PNEU_CONFIG_DIR` and `PNEU_CACHE_DIR` override those
directories. XDG data/config/cache overrides also work, with a `pneu` suffix;
`PNEU_*` takes precedence. `PNEU_RUNTIME_DIR` (or `XDG_RUNTIME_DIR`) selects an
isolated pipe namespace using a digest of the override. It is not a pipe's
filesystem parent. The pairing journal is in the private data directory.

See [platform/windows.rs](../desktop/crates/pneud/src/platform/windows.rs) and
[window.rs](../desktop/crates/pneud/src/ui/window.rs).

## Storage and IPC security

The [Windows filesystem backend](../desktop/crates/pneud/src/platform/fs/windows.rs)
walks components with `NtCreateFile`, retained directory handles and
`FILE_OPEN_REPARSE_POINT`. It rejects reparse points and remote storage,
including cloud-placeholder paths that require reparses. Private objects have
user/SYSTEM DACLs. Existing private objects are checked rather than silently
repaired.

Physical path containment uses a volume-GUID path recovered from a handle,
re-walked without following reparses and compared against that handle. Identity
is the volume serial plus the full 128-bit file ID. SQLite uses `win32-longpath`;
its actual database handle must match the locked handle. A `LockFileEx` byte-range
lock on the database file protects the store across path aliases. Database and sidecar
files with multiple hard links or pending deletion are rejected.

Placement uses handle-relative `NtSetInformationFile(FileRenameInformationEx)`
without replacement; deletion uses `FileDispositionInfoEx`. File and namespace
flushes precede the durable receipt and ACK. This does not establish power-loss
durability on every filesystem or device. As on other desktops, a crash after
placement but before receipt commit can leave a duplicate after retry.

[Receive-name mapping](../desktop/crates/pneud/src/storage/windows_names.rs)
handles reserved names, forbidden characters, ADS colons, trailing dots/spaces
and case collisions locally. It does not change wire names.

Local commands use bounded JSON over Tokio named pipes, not Unix sockets.
[windows_ipc.rs](../desktop/crates/pneud/src/platform/windows_ipc.rs) provides:

- A protected DACL granting only the user's SID and SYSTEM, and remote-client
  rejection. The server checks the connected client's session before parsing
  commands. The DACL supplies the user boundary.
- Client checks of the pipe object's owner SID, exact DACL and server session
  before sending commands. It does not trust a PID lookup as identity.
- `first_pipe_instance` to detect an already claimed name. A replacement
  instance is created before releasing an accepted or rejected instance, keeping
  ownership continuous. Name squatting is detected, not prevented.
- A data-directory daemon lock held across sessions. Pending I/O is cancelled
  or disconnected on timeout, rejection and shutdown.

The shared [IPC layer](../desktop/crates/pneud/src/ipc.rs) bounds requests,
responses and handler lifetimes. The DACL excludes other ordinary users; session mismatches fail closed. Hostile
processes running as the same user are outside this boundary.

## Discovery

Discovery uses native Windows adapter metadata and the shared address-selection
rules. The TCP listener explicitly enables dual stack and exclusive address use.
See [windows_discovery.rs](../desktop/crates/pneud/src/platform/windows_discovery.rs)
and [net.rs](../desktop/crates/pneud/src/net.rs).

## Clipboard

A message-only Win32 window uses `AddClipboardFormatListener` and
`GetClipboardSequenceNumber`. Existing clipboard contents form a metadata-only
startup baseline. Provider reads run in a killable `clipboard-read` subprocess,
with a Job Object and a five-second observation deadline. Replacement,
cancellation and shutdown stop and reap the reader. Recipients and policy are
captured before payload reads; later connections cannot join that copy.

Supported representations are Unicode text, registered PNG, DIBV5 and DIB;
bitmap reads are converted to bounded PNG. Sensitive formats suppress sharing:

- `ExcludeClipboardContentFromMonitorProcessing`.
- `Clipboard Viewer Ignore`.
- `CanIncludeInClipboardHistory` unless its value is exactly 1; unreadable or
  malformed values fail closed.

Installation publishes all representations and a private install ticket in one
`OpenClipboard` transaction. Pneu sets `CanUploadToCloudClipboard` to 0.
The independent watcher must confirm the ticket before the actor considers the
installation complete. The shared actor handles ordering and echo suppression;
received clipboard content is not automatically relayed.

See [windows_clipboard.rs](../desktop/crates/pneud/src/ui/windows_clipboard.rs),
[format policy](../desktop/crates/pneud/src/ui/windows_clipboard_content.rs) and
[clipboard_actor.rs](../desktop/crates/pneud/src/clipboard_actor.rs).

## Tray and window

The daemon hosts `tray-icon` and `muda` on a dedicated Win32 message-loop thread.
It uses the shared tray model, adapts icons to taskbar DPI and light/dark mode,
and restores them after Explorer restarts. **Open Pneu**, **Send Files To**,
**Send Clipboard**, **Open Received Files**, settings, **Add Device…** and
**Quit Pneu** are available there.

The Slint child is launched when needed and retained for later requests. Opening
Pneu again raises the same window and restores it if minimized. The daemon
passes foreground rights to the child. Native `IFileOpenDialog` pickers choose
files and folders; the window fits the monitor's work area. A daemon-owned
kill-on-close Job Object contains the window child.

Files can also be sent from outside the picker. The packaged app is a share
target, so **Share → Pneu** in Explorer (and other apps' share sheets) forwards
the selected files to the daemon's chooser, starting the daemon if needed;
folders are refused. Dragging files over the window shows the drop overlay:
dropping on a device row sends there, dropping elsewhere opens the chooser. The
window's OLE drop target is in
[windows_drag.rs](../desktop/crates/pneud/src/ui/windows_drag.rs).

Closing the window keeps transfers and the tray running. The first eligible
close shows a silent **Pneu is still running** toast. Its private seen marker is
written only after successful display; turning notifications off leaves it
unseen. **Quit Pneu** stops the daemon. A session without an input desktop,
including session 0, stays headless and rejects UI requests.

See [windows_runtime.rs](../desktop/crates/pneud/src/ui/windows_runtime.rs),
[windows_tray.rs](../desktop/crates/pneud/src/ui/windows_tray.rs),
[windows_window.rs](../desktop/crates/pneud/src/ui/windows_window.rs),
[windows_picker.rs](../desktop/crates/pneud/src/ui/windows_picker.rs) and
[windows_close.rs](../desktop/crates/pneud/src/ui/windows_close.rs).

## Notifications and Open/Show

The daemon hosts a WinRT toast notifier and an STA COM activator implementing
`INotificationActivationCallback`. Packaged builds use their package identity;
unpackaged builds register a per-user AUMID and `LocalServer32` entry.

Toast arguments contain opaque session, revision, notification ID and action
values, never file paths. Before a live action, Pneu checks the target's
presence, lifetime, revision and offered actions. Supported actions include
Open, Show in Folder, Open Link, Retry, Choose Folder and Add Device.

Clicking a toast can cold-start the daemon. Stale arguments take general routes:
Add Device, the folder picker, the current receive folder for Open/Show, or the
Pneu window. They cannot reopen a specific file from an earlier daemon session.
Bursts coalesce through toast tags/groups; transient notices are silent and
expire.

Open reopens a received path without following reparses and compares its volume
and file ID with the retained handle before calling `ShellExecuteExW`. A moved,
deleted or replaced file reports **The file was moved or deleted**. Shell calls
run on a short-lived STA thread in the requesting process to retain foreground
rights. Show in Folder groups paths by parent and uses
`SHOpenFolderAndSelectItems`; Open Received Files opens the configured folder.

See [windows.rs](../desktop/crates/pneud/src/actions/windows.rs),
[windows_toasts.rs](../desktop/crates/pneud/src/actions/windows_toasts.rs),
[toast authority](../desktop/crates/pneud/src/ui/windows_toast_model.rs) and
[windows_shell.rs](../desktop/crates/pneud/src/actions/windows_shell.rs).

## Packaging and updates

The full-trust MSIX includes both binaries, the CLI execution alias, toast COM
registration and `PneuStartup`. Sign-in startup does not open a window.
**Settings → This PC → Open at login** controls the packaged StartupTask.
If Windows Settings disabled it, Pneu offers Open Settings; policy-managed states
are read-only. Unpackaged builds have no startup toggle.

The manifest declares inbound TCP 47821 and UDP 5353 for both executables on all
profiles, including Public for Tailscale. These rules cover the default port;
changing Pneu's port does not change the manifest rules.

Session-close messages request orderly shutdown. The daemon drains tracked Store
work before releasing its lock. `RegisterApplicationRestart` allows Restart
Manager to resume it with `run` after an update; this registration does not
restart it after a crash, hang or reboot.

Build parameters, signing, sideload installation prerequisites, Store identity
values and submission disclosures are in the
[Windows packaging README](../desktop/packaging/windows/README.md). The release
workflow builds with that Store identity and retains the unsigned MSIX as the
`Pneu-Windows-Store-msix` workflow artifact for Partner Center, which signs it for
distribution. It does not publish the MSIX on GitHub Releases or submit to the
Store. Received files remain outside package state and survive uninstall.

## Native verification

From `desktop` on Windows:

```powershell
cargo test -p pneud --features windows-package
```

Use the host's `heavy` wrapper for full suites and production builds where it is
installed. CI runs the Windows test command with `--locked`. Tests marked ignored
need their stated native fixtures; a regular test run does not exercise all UI
or clipboard behavior.

Run interactive acceptance in a logged-in console session. An SSH shell or
session-0 scheduled job is not evidence for tray, clipboard, foreground-window,
picker or toast behavior. In a VM, launch those checks through an interactive
console-session task and collect its exit status and logs.

Isolate all `PNEU_*` directories, receive folder and TCP port for daemon tests.
The runtime override isolates the named pipe. Stop task-owned daemons and
helpers, and remove test package registrations afterwards. Include standard-user
storage/IPC checks, pairing and transfers, clipboard privacy/cancellation,
Explorer recovery, DPI/theme changes, cold toasts, startup and updates.

## Known gaps

- Pneu passed Store certification and 2.2.0 is live. Store-managed updates on a
  real PC have not been verified yet.
- Windows can show a pairing QR and paste a `pneu://pair` link, but has no camera
  QR scanner.
- Physical acceptance remains pending, including real LAN multicast, Tailscale
  on the Public profile, a fresh standard-user install and mixed-DPI monitors.
