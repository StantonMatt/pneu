# Pneu — plan

Pneu connects Android phones and Linux, macOS and Windows computers directly over
LAN or Tailscale. Each device pairs with each peer it wants to use. Files have
per-recipient queues; clipboard sharing requires a live connection.

## Current scope

### Pairing and connections

- Several phones and computers can connect to a desktop at once. Android supports
  up to 16 paired computers; phone-to-phone pairing is not implemented.
- Every pair has its own pinned mutual TLS 1.3 trust. Adding a device preserves
  other pairings. Pairing a phone with two computers does not pair those computers.
- The phone scans a computer's QR code. Computers use **Add Device… → Computer**
  to show a QR/`pneu://pair` link or paste another computer's link. A Mac with a
  camera can also scan a QR code.
- Computer links dial and reconnect using LAN/Tailscale address hints. Addresses
  supply routes, not trust. Received content is not automatically relayed.
- Each device page has directional clipboard switches. Phone/computer pairs also
  have an automatic-media switch. Edits synchronize and can wait for reconnect.
- Unpairing revokes only that pair and its pending deliveries. Saved files remain.

See [core.rs](../desktop/crates/pneud/src/core.rs),
[mesh.rs](../desktop/crates/pneud/src/mesh.rs) and the
[protocol](PROTOCOL.md).

### Sending and receiving

| Feature | New-install default | Behaviour |
| --- | --- | --- |
| Phone screenshots | On | Send new screenshots to eligible computers |
| Camera photos / screen recordings | Off | Optional automatic media |
| Manual files | Available | Android Share → Pneu or Send files; desktop window, tray/menu, Dolphin or CLI |
| Share clipboard on desktop | Automatically | Text and images; Include images is on by default |
| Phone clipboard | Manual | A button, tile or share action is required |
| Choose devices each time | Off | Otherwise use eligible connected recipients |
| Queue for offline devices | Off | Optional on both phone and desktop; explicit file targets can queue offline |
| Save received desktop files | On | System pictures folder + Pneu on Linux (falling back to ~/Pictures/Pneu), Downloads/Pneu on Mac and Windows |
| Receive notifications / copy images | On | Platform adapters apply supported actions |
| Open images in viewer | Off | Optional |

The recipient chooser can select several devices. File admission freezes their
pairing identities and snapshots the bytes into private storage. Reconnects retry
individual deliveries with the same IDs and bytes. Unpair/re-pair cannot retarget
an old queue. Android manual shares without an explicit selection use offline
recipients only when none are connected and queueing is enabled. Clipboard is
always connected-only.

Desktop Recent shows incoming and outgoing work, including per-recipient state
and retry/cancel actions. Android Recent also exposes recipient outcomes. Durable
receipts deduplicate completed retries; they do not guarantee exactly one visible
file if a desktop crashes between placement and receipt commit.

See [outbox.rs](../desktop/crates/pneud/src/outbox.rs),
[file_sender.rs](../desktop/crates/pneud/src/file_sender.rs), the
[desktop window](../desktop/crates/pneud/ui/window.slint) and
[Android recipient selection](../android/app/src/main/java/io/github/stantonmatt/pneu/data/RecipientSelection.kt).

### Phone connection policy

- Direct Wi-Fi, Tailscale and mobile data are enabled by default. At least one of
  Wi-Fi or Tailscale must stay enabled. Tailscale needs its app connected.
- Originals on mobile data are off by default: automatic images are compressed;
  recordings wait for Wi-Fi. Shared files keep their original bytes.
- Pause suspends automatic media, and Resume re-baselines observation rather
  than sending media captured during the pause. Already queued items remain.
  Manual sends still work. Incoming clipboard is ignored while paused.

## Implementation

- **Android:** Kotlin, Compose/Material 3, coroutines, MediaStore observation in a
  `connectedDevice` foreground service, CameraX and ZXing for QR scanning.
- **Desktop:** Rust with Tokio, rustls, SQLite, mDNS and a shared Slint window.
  Core owns trust, sessions, transfer admission, receipts and clipboard ordering.
  OS effects are behind platform filesystem, IPC and action adapters.
- **Linux:** KDE Plasma on Wayland, systemd user service, ksni tray, Qt window,
  portals and freedesktop notifications.
- **macOS:** per-user daemon and supervised menu-bar/window child, native
  pasteboard and notifications. See [macOS](macos.md).
- **Windows:** per-user daemon with tray, clipboard and toasts, an on-demand
  window child, named-pipe IPC and NT handle-relative storage. Source and
  sideloaded MSIX work; the Microsoft Store listing is coming. See
  [Windows](windows.md).

The [protocol](PROTOCOL.md) defines framing, per-pair authentication, negotiated
`mesh` behavior, file receipts, clipboard ordering and settings synchronization.
Older peers retain compatible wire behavior; unsupported features require an
update rather than silently changing trust or discarding queues.

## Known Android and Samsung limits

1. Android blocks background clipboard reading. Phone clipboard sending needs a
   deliberate foreground action; computers can send to the phone automatically.
2. Battery restrictions can stop observation. Setup and health checks guide the
   user through permissions and unrestricted battery use. Samsung's Never
   sleeping guidance accounts for apps already exempt from optimization.
3. Full photos/videos access is needed for automatic capture. Limited access
   cannot establish that all new media will be observed.

## Historical milestones

The original v1 plan targeted one Samsung phone and one KDE computer, installed
from source/USB. Its setup, protocol spike, core transfer, clipboard/media,
share-sheet and device-acceptance stages led to the first implementation.
The original single-pair and phone-only sender restrictions are superseded.

Later history records the implemented expansion:

- **2.0:** redesigned Android app, desktop window and icons.
- **2.1:** multiple devices, computer links, shared clipboard ordering, file sends
  in both directions, recipient selection, durable desktop outbox and Mac app.
- **Windows M1–M6:** platform filesystem layer, native storage/name mapping,
  discovery and pipes, clipboard, tray/window, Open/Show, toasts and MSIX.
- **Windows first close:** one-time notice that closing the window leaves Pneu
  running in the tray.

## Remaining work

- Microsoft Store identity, submission and certification; physical Windows
  acceptance. Explorer Share and window drops are in review in
  [PR #21](https://github.com/StantonMatt/pneu/pull/21).
- Google Play closed testing and broader Android/network/battery acceptance.
- Developer ID signing and notarization for the Mac app.
- Notification mirroring and screen mirroring remain outside the implemented
  scope. Desktop → phone file sending is already implemented.
