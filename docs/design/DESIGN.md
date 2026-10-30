# Pneu: design spec

This document describes the current multi-device app: Android Compose screens
and a shared Slint desktop window with Linux, macOS and Windows integrations.
The [protocol](../PROTOCOL.md) defines wire behavior. The source links below
provide the exact strings and layout.

The original v1 [mockups](mockups.html) and [icon assets](assets/) are historical
references. Their single-phone layouts, teal/brass branding and proposed copy
are superseded by the current screens and [v2 assets](v2/assets/).

Rules that apply everywhere:

- Use short labels. Add supporting text only for information such as a reason,
  path, pending change or count.
- Phone labels use sentence case; desktop menus and buttons follow the existing
  platform conventions. Source strings take precedence over historical mockups.
- Device names are user-visible names exchanged during pairing/HELLO. Use the
  selected device's name, not an assumed single phone or computer.
- `…` is U+2026; `·` is U+00B7 with surrounding spaces.

---

## 1. Shared behaviour (both sides must agree)

| Topic | Rule |
| --- | --- |
| Topology | Several phones/computers can pair with a desktop. Android pairs with several computers. Every link is direct and separately trusted; no automatic relay or transitive pairing. |
| Pairing | Add a device without replacing unrelated pairs. Phones scan a computer's QR; computers show a QR/link or paste `pneu://pair`. Mac camera scanning is available when a camera exists. Invitations are short-lived and one-use. |
| Unpair | Remove only the selected pair, its trust and pending deliveries. Send best-effort UNPAIR if connected. Saved files remain. Offline peers discover lost trust on reconnect. |
| Pair switches | Send clipboard, Receive clipboard, and phone → computer automatic media start enabled. Edits synchronize; offline changes show as pending. Computer pairs have only the clipboard directions. |
| Recipients | Default file sends use eligible connected devices. Choose devices each time opens a picker. Explicit file targets may queue offline. Capture pairing identity before reading sources; later pairings/connections cannot gain access to an admitted operation. |
| Queue | Queue for offline devices defaults off. Automatic phone media can include offline eligible computers when enabled. Untargeted Android manual shares use offline recipients only when none are connected and queueing is enabled. Desktop file deliveries use an immutable per-recipient outbox. Clipboard never queues offline. |
| Pause | Pause suspends automatic phone media and ignores incoming clipboard; manual sends still work. Resume re-baselines observation, skipping media captured during the pause. Existing queued files remain. |
| Mobile data | Mobile data off waits for Wi-Fi. With originals off, automatic images are compressed and recordings wait; shared files stay unchanged. |
| Dedupe | Stable transfer IDs and per-sender receipts deduplicate completed retries. A desktop crash after placement but before receipt commit can leave a duplicate visible file. |
| Clipboard ordering | Text/images share a device-wide `(hlc, source_device)` register on mesh links. Install tickets and recent keyed fingerprints suppress echoes. Received content is not automatically forwarded. |
| Clipboard policy | Desktop Automatically / Manually / Off and Include images (on by default) combine with per-pair direction switches. Sends require a live eligible session; new connections do not join an existing observation. Phone sends require a foreground action. |
| Sensitive data | Platform password/sensitive markers suppress sharing. See platform guides for exact formats and limits. |
| Receipts and errors | Saving, clipboard effects and user navigation are distinct. A successful file receipt does not promise every optional OS action succeeded. UI errors use readable reasons, not wire enum names. |

See [Core](../../desktop/crates/pneud/src/core.rs),
[mesh links](../../desktop/crates/pneud/src/mesh.rs),
[clipboard actor](../../desktop/crates/pneud/src/clipboard_actor.rs),
[file admission](../../desktop/crates/pneud/src/file_sender.rs),
[outbox](../../desktop/crates/pneud/src/outbox.rs),
[pair settings](../../desktop/crates/pneud/src/peer_settings.rs) and
[Android recipient selection](../../android/app/src/main/java/io/github/stantonmatt/pneu/data/RecipientSelection.kt).

---

## 2. Brand and icons

The current Crossing mark and its platform variants live in
[v2/assets](v2/assets/). Android uses bundled vector resources; desktop adapters
supply app and tray/menu-bar assets for their platform.

The original tube-carrier drawings in [assets](assets/) remain a v1 reference.
They do not specify the installed icon or current palette.

- Android colors, typography, spacing and motion are in
  [PneuTheme.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/ui/PneuTheme.kt).
- Desktop shared controls and palette roles are in
  [components.slint](../../desktop/crates/pneud/ui/components.slint) and
  [theme.slint](../../desktop/crates/pneud/ui/theme.slint).
- Windows packaging logos and resources are described in the
  [packaging README](../../desktop/packaging/windows/README.md).

---

## 3. Phone (Android, Jetpack Compose, Material 3)

### 3.1 Theme

Android uses Pneu's explicit light/dark palette through Material 3, following
system appearance. It no longer uses the original v1 dynamic teal scheme.
`PneuType`, `PneuSpace` and `PneuMotion` define shared tokens; the system animation
scale controls motion. Layouts use the system font, inset-aware scaffolds,
grouped cards and a bounded content width.

See [PneuTheme.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/ui/PneuTheme.kt)
and [shared UI controls](../../android/app/src/main/java/io/github/stantonmatt/pneu/ui/).

### 3.2 Navigation

- Incomplete setup opens **Set up Pneu**. With no paired computers and no lost-pair state, open the
  scanner; otherwise open Home.
- Home opens Settings, a device page, Add device and Recent recipient details.
- Add device preserves existing links. Cancelling a scan leaves them intact.
- A device page contains that computer's status, Send files, sharing switches
  and Unpair. Removing one computer does not reset the other devices.

Routing lives in [MainActivity.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/MainActivity.kt);
device and recipient surfaces live in
[DeviceScreens.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/DeviceScreens.kt).

### 3.3 Setup (first run) and Permissions and battery

Setup and the later Permissions and battery screen cover:

- Notifications.
- Photos and videos, with full access for automatic capture. The disclosure
  explains background sending to paired computers.
- Local network access when the permission applies.
- Unrestricted battery use.
- Samsung Never sleeping apps guidance. An app exempt from battery optimization
  already satisfies this step and may be hidden from Samsung's picker.

Home banners expose missing permissions and background-stop evidence. Setup
continues with unfinished steps; the app does not claim those permissions were
obtained. See [MainActivity.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/MainActivity.kt)
and [Health.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/Health.kt).

### 3.4 Pair

**Add device** scans **the QR code on your computer** using CameraX and ZXing.
It validates Pneu links, handles expiry/reachability errors, previews the
computer's name and asks for confirmation. Existing computers remain paired.
An already paired computer is identified as such.

See [PairCamera.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/PairCamera.kt)
and [pairing strings](../../android/app/src/main/res/values/strings.xml).

### 3.5 Home

Home has an aggregate connection/send status, Send clipboard and Pause/Resume,
then **Devices** and **Recent**. Devices has one row per paired computer plus
**Add device**. Each row reports its own connection, route, last seen or problem.
A single lost pair does not turn the whole mesh into a single Pairing lost page.

Recent combines outgoing and received work. Sending/waiting/failed rows expose
per-recipient outcomes; users can retry or stop a delivery without retargeting
its siblings. The waiting menu offers **Send now** and **Don't send any**.
Received rows identify the source computer.

See [MainActivity.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/MainActivity.kt)
and [DeviceScreens.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/DeviceScreens.kt).

### 3.6 Settings

| Group | Controls and new-install defaults |
| --- | --- |
| Send automatically | Screenshots on; Camera photos and Screen recordings off |
| Sharing | Show in share sheet on; Choose devices each time off |
| Clipboard | Send button in notification on |
| Connection | Direct on home Wi-Fi, Tailscale when away and Use mobile data on; Send originals on mobile data and Queue for offline devices off |
| This phone | Name, Quick Settings tiles on, Permissions and battery, Privacy policy and version |

At least one of Wi-Fi and Tailscale stays enabled. Media toggles recheck required
access. Per-computer Send clipboard, Receive clipboard and Automatic sending
switches live on the device page, with pending changes labeled until reconnect.
Unpair confirmation concerns that computer and its waiting deliveries.

See [SettingsScreen](../../android/app/src/main/java/io/github/stantonmatt/pneu/MainActivity.kt),
[DeviceScreen](../../android/app/src/main/java/io/github/stantonmatt/pneu/DeviceScreens.kt)
and [Prefs](../../android/app/src/main/java/io/github/stantonmatt/pneu/data/Preferences.kt).

### 3.7 Status notification (foreground service)

A silent ongoing **Status** notification represents the foreground service.
It reports aggregate connections, sending progress, waiting, no network or pause.
When enabled, **Send clipboard** opens the clipboard activity directly;
Pause/Resume changes automatic sending. Tap opens Pneu.

Alerts cover missing media/network access, battery restrictions, background stops
and lost pairings. The service can retain other usable computer links when a
pair is lost. The manifest uses the `connectedDevice` foreground-service type.

See [PneuService.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/PneuService.kt)
and [AndroidManifest.xml](../../android/app/src/main/AndroidManifest.xml).

### 3.8 Quick Settings tiles

**Pneu** toggles Pause/Resume and reports aggregate connection state. **Send
clipboard** opens the foreground clipboard flow; disconnected or unpaired states
do not create an offline clipboard queue. Settings can disable the tiles.

See [ManualEntries.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/features/ManualEntries.kt)
and [TilePolicy.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/features/TilePolicy.kt).

### 3.9 Clipboard and share target

**Share → Pneu** accepts shared files/text. **Send files** uses the document
picker. **Choose devices each time** presents **Send to** with eligible computers;
explicit file selection can include offline devices. The picker says **Sends
when connected** for queued targets. Without a picker, use the policy in section 1.

Clipboard reads require a foreground activity. Sensitive clipboard content is
refused; recipients are frozen before provider reads. Clipboard text and images
remain connected-only. Receiving clipboard uses Android's ClipboardManager and
respects Pause and each pair's Receive clipboard switch.

Android receives ordinary files as well as clipboard content; receive storage
and its limits are described in the [Android README](../../android/README.md).

See [manual entry flow](../../android/app/src/main/java/io/github/stantonmatt/pneu/features/ManualEntryViewModel.kt),
[clipboard policy](../../android/app/src/main/java/io/github/stantonmatt/pneu/features/ClipboardSendPolicy.kt)
and [received files](../../android/app/src/main/java/io/github/stantonmatt/pneu/features/ReceivedFiles.kt).

### 3.10 Health checks

Health checks revisit permissions, battery restrictions and background-stop
history. Resume refreshes them so returning from system settings updates the
screen. See [Health.kt](../../android/app/src/main/java/io/github/stantonmatt/pneu/Health.kt).

### 3.11 Phone strings

The authoritative copy is [strings.xml](../../android/app/src/main/res/values/strings.xml).
Use device names and plural-aware counts. Current terms include **Add device**,
**Devices**, **Send to**, **Queue for offline devices**, **Send clipboard** and
**Receive clipboard**. The v1 **Send to PC** label and single Paired PC group no
longer describe navigation.

---

## 4. Computers (Rust daemon and native desktop integrations)

### 4.1 States and tray icon

The shared model aggregates paired and connected devices, pairing, receiving,
phone pause and errors. Device pages preserve each link's state. Linux uses a
KDE tray; macOS a menu bar; Windows a Win32 tray. Platform appearance and recovery
are adapter responsibilities.

See [tray model](../../desktop/crates/pneud/src/ui/tray_model.rs) and the
[macOS](../macos.md) / [Windows](../windows.md) guides.

### 4.2 Tray menu

Menus expose file and clipboard sends, received files, clipboard mode, arrival
actions, Add Device and Quit. Recipient submenus use current eligible devices;
the window and CLI share the same admission rules. Menu order and platform
wording differ. macOS additionally uses its native menu-bar integration.

See [tray_model.rs](../../desktop/crates/pneud/src/ui/tray_model.rs),
[Linux tray](../../desktop/crates/pneud/src/ui/tray.rs) and
[Windows tray](../../desktop/crates/pneud/src/ui/windows_tray.rs).

### 4.3 Window (Slint)

One shared window has these pages and dialogs:

- **Main:** Send Files…, Send Clipboard, Devices, Add Device…, Recent and Settings.
- **Device:** status, targeted Send Files…, directional sharing switches and Unpair.
- **Add Device:** Phone and Computer tabs. Phone shows a QR. Computer offers
  Show Pairing Code and Paste Link; Mac camera scanning appears when available.
- **Pairing Code:** QR and copyable link. The daemon owns the invitation shared
  with the CLI. When it expires the page shows "This code expired" with a
  **New Code** button; only the Phone tab's QR refreshes automatically.
- **Settings:** clipboard mode/images, recipient picker/offline queue, receive
  folder/actions and device name. Open at login is available on Mac and packaged
  Windows.
- **Send to:** eligible recipients with offline queue state. Explicit file
  targets retain their captured pairing identity.
- **Recent details:** per-recipient progress and retry/cancel/remove actions.

The window keeps a fixed logical width with height bounded by its content and a
scrolling body. Closing it leaves the daemon and transfers running. Linux and
Windows create a window child on demand; macOS keeps a supervised UI child for
its native integrations. A live Windows child is reused and raised on request.

Linux uses Qt and portals; Mac/Windows use winit and native file/folder pickers.
Drops are supported on Mac. Windows Explorer Share and window drops are
[in review](https://github.com/StantonMatt/pneu/pull/21), not part of this snapshot.

See [window.slint](../../desktop/crates/pneud/ui/window.slint),
[window.rs](../../desktop/crates/pneud/src/ui/window.rs) and the
[desktop README](../../desktop/README.md).

### 4.4 Notifications

The shared [Notice model](../../desktop/crates/pneud/src/ui/notifications.rs)
provides receive, clipboard, burst, pairing, blocked-peer and send-failure copy.
Actions include Open, Show in Folder, Open Link, Choose Folder, Add Device and
Retry where offered. Show notifications can suppress ordinary notices; failures
of optional notification effects do not undo a stored file.

- Linux uses freedesktop notifications, Plasma file previews and portal opens.
- Mac uses native notifications and a supervised UI bridge.
- Windows uses WinRT toasts and COM activation with opaque session/revision/ID
  arguments. Stale or cold actions route to general UI or the receive folder,
  never a file target retained from a previous daemon session.

Platform guides describe action identity checks, lifetime and limitations.
Windows also shows a one-time **Pneu is still running** notice after an eligible
first window close.

### 4.5 Clipboard behaviour

The shared actor owns observations, ordering, frozen recipients, cancellation,
install tickets and echo history. OS provider reads and effects run outside it.
It handles several peers; it does not broadcast received content onward.

- **Automatically:** observe local clipboard changes and send to currently
  eligible peers. **Manually:** explicit sends. **Off:** no local sharing.
- Include images starts on. Sending requires receiver support and the pair's
  direction to be enabled. Phone pause suppresses incoming automatic clipboard.
- Received supported images may be copied according to receive settings and the
  pair's clipboard direction. Ordinary file saving remains independent.
- Linux observes Wayland CLIPBOARD, Mac NSPasteboard, Windows Win32 clipboard
  changes. Platform sensitive markers suppress payload sharing.
- A disconnect or revoked policy cancels affected work; a newer session cannot
  inherit an older clipboard send. There is no durable offline clipboard queue.

See [clipboard_actor.rs](../../desktop/crates/pneud/src/clipboard_actor.rs), the
[desktop README](../../desktop/README.md) and platform guides for image formats,
size caps and permission behavior.

### 4.6 Computer strings

[window.slint](../../desktop/crates/pneud/ui/window.slint),
[tray_model.rs](../../desktop/crates/pneud/src/ui/tray_model.rs) and
[notifications.rs](../../desktop/crates/pneud/src/ui/notifications.rs) own the
current English copy. Use device names, aggregate counts and readable recipient
failures. Historical single-phone strings and tray/window drawings are not the
current UI contract.

---

## 5. Historical spike and acceptance notes

The v1 spike questions concerned Plasma tray recoloring/previews, Samsung battery
guidance, foreground notifications and foreground clipboard access. Those were
implementation questions, not remaining single-pair requirements. The current
code uses `connectedDevice`, CameraX/ZXing and the multi-recipient flows above.

Native acceptance remains platform-specific. The [README](../../README.md#status-and-limitations)
records the physical-device boundary; the [Windows guide](../windows.md#known-gaps)
records pending Store and physical-hardware work. Automated tests and VM checks
do not establish every OEM background, network transition, permission or display
configuration.
