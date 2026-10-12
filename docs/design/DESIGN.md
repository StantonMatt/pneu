# Pneu: design spec

Covers every user-facing surface of v1: the Android app (Compose, Material 3) and the PC daemon (KDE tray, one Slint window, freedesktop notifications). Visual reference: `mockups.html` (open it in a browser; every screen is shown in light and dark). Icons: `assets/`.

Rules that apply everywhere:

- No subtitles, taglines or helper text. A line of secondary text appears only where this spec lists it, because it carries something the controls don't (a reason, a path, a count).
- Strings below are final copy. Phone strings use sentence case (Material). PC menu items, buttons and window titles use Title Case (KDE HIG); notification text and labels use sentence case.
- `{pc}` is the PC's hostname as sent in the pairing QR (for example `MJStanton-Desktop`). `{phone}` is the phone's device name (`Settings.Global.DEVICE_NAME`, for example `Galaxy S25+`).
- `…` is the single ellipsis character (U+2026), not three dots. `·` is U+00B7 with a space on each side.

---

## 1. Shared behaviour (both sides must agree)

These rules decide what the UI says, so they are fixed here. Items marked **protocol** need a field or message in `docs/PROTOCOL.md`.

| Topic | Rule |
|---|---|
| Pairing | One phone and one PC. Pairing again on either side replaces the old pairing. The phone keeps its old pairing until a new one succeeds, so backing out of the scanner changes nothing. |
| Unpair | Unpairing on either side tells the other side if connected (**protocol**: `unpair` message) and both forget each other. If not connected, the other side finds out on the next connection attempt (pin mismatch → "Pairing lost" on the phone, "Blocked an unpaired phone" on the PC). |
| Pause (phone) | The connection stays up. Automatic media already inserted in MediaStore at Resume, including still-pending items, is **skipped, not queued**. Resume re-baselines observation; it requires full media access. Items already waiting stay queued and send after Resume. PC clipboard arriving while paused is ignored. Manual sends (Share → Send to PC, Send clipboard) still work. The phone tells the PC it is paused (**protocol**: `paused` flag in hello and a status message) so the tray can show it. |
| Queue | With "Queue while PC is unreachable" on, automatic items found while the PC is unreachable wait and send in order when it comes back. With it off they are skipped and never listed. Shared items always queue (the user asked for them explicitly). Clipboard is never queued. Queued files are private snapshots: deleting the original does not cancel sending. Use **Don't send** to cancel. |
| Mobile data | "Use mobile data" off → on mobile data the state is "Waiting for Wi-Fi" and automatic items queue. "Send originals on mobile data" off → images are re-encoded (JPEG, quality 85, long edge ≤ 2560 px) and screen recordings wait for Wi-Fi; shared files are always sent as-is. |
| Dedupe | Automatic items are keyed by MediaStore ID; nothing is sent twice after a reconnect. |
| Clipboard loop | Neither side sends back clipboard content that Pneu itself just placed on that clipboard (compare a hash of the last content Pneu set). Without this, a received screenshot copied on the PC would bounce back to the phone. |
| Password managers | The PC never sends clipboard entries that carry the `x-kde-passwordManagerHint` MIME type with value `secret` (KeePassXC and others set it). |
| PC clipboard → phone | Owned by the PC (it decides what it sends). Sent only while the phone is connected and not paused; dropped otherwise. Debounce 300 ms. Only the CLIPBOARD selection, never PRIMARY. |
| Item kinds | The PC labels arrivals by kind, so the phone sends it (**protocol**: `kind` = `screenshot` / `photo` / `recording` / `file` / `text` / `clipboard_text` / `clipboard_image`, plus original file name and MediaStore ID). |
| Save errors | When the PC can't save, it replies with a reason the phone can show (**protocol**: ack with error `disk_full` / `not_writable`). |
| Names | Hello carries `{phone}` from the phone and `{pc}` from the PC (**protocol**). |

---

## 2. Brand and icons

All files are in `assets/`. Geometry is plain paths (no transforms, masks or clip paths), so they import into Android Studio's Vector Asset tool and render in QtSvg.

The mark is a pneumatic-tube carrier: a clear body with brass end caps and dark rings, flying up and to the right through a tube, trailing speed lines. The tray and status bar glyph is the same carrier reduced to three segments (cap, body, cap), at the same 45° angle.

| File | Use |
|---|---|
| `ic_launcher_background.svg` | Adaptive icon background layer (108 dp canvas): teal with a lighter diagonal tube. |
| `ic_launcher_foreground.svg` | Adaptive icon foreground layer: the carrier and speed lines. The carrier sits inside the 66 dp safe zone; the speed-line tails may be clipped by round masks, which is intended. |
| `ic_launcher_monochrome.svg` | Themed icon (`<monochrome>` in `mipmap-anydpi-v26/ic_launcher.xml`). Alpha only. |
| `ic_launcher_preview.svg` | Flattened full-bleed preview, for docs only. |
| `ic_stat_pneu.svg` | Notification small icon and both Quick Settings tile icons (24 dp, white, alpha only). |
| `pneu.svg` | PC app icon: `.desktop` file `Icon=`, notification `app_icon`, window icon. Install to `~/.local/share/icons/hicolor/scalable/apps/pneu.svg`. |
| `pneu-tray-*.svg` | Tray icons, one per state (section 4.1). 22 px grid, KDE color-scheme stylesheet. |

Brand colours (icon only; the app UI uses dynamic colour):

| Token | Hex |
|---|---|
| Teal (icon background) | `#0F6E68` |
| Teal dark (rings) | `#083F3B` |
| Brass (caps, speed lines) | `#F4B13E` |
| Glass (carrier body) | `#EAF6F3` |

Android also uses Teal `#0F6E68` as the seed for the fallback colour scheme (only used if dynamic colour is unavailable) and as the notification accent colour (`setColor`).

**Tray icon recolouring.** The tray SVGs use Breeze's `current-color-scheme` stylesheet (`.ColorScheme-Text`, and `.ColorScheme-NegativeText` for the error badge), the same mechanism as Breeze's own status icons, so Plasma paints them in the panel's text colour on light and dark panels. Install them into an icon theme directory the daemon passes as the SNI `IconThemePath` (for example `~/.local/share/pneu/icons/hicolor/scalable/status/`) and set `icon_name` to the file name without `.svg`. **Verify in the spike** that Plasma 6.6 recolours icons loaded from an app's `IconThemePath`. If it does not, render the SVG to an ARGB pixmap (resvg) with `currentColor` replaced by the panel text colour (from `kdeglobals`, re-rendered on the `org.kde.KGlobalSettings` `notifyChange` signal) and pass it as `icon_pixmap`.

---

## 3. Phone (Android, Jetpack Compose, Material 3)

### 3.1 Theme

- `dynamicLightColorScheme` / `dynamicDarkColorScheme`, following the system dark setting. Fallback scheme: Material Theme Builder output for seed `#0F6E68`.
- Typography: Material 3 default type scale with `FontFamily.Default`, so One UI's system font applies. Don't bundle fonts.
- Edge-to-edge (`enableEdgeToEdge()`), transparent system bars, content padded by insets.
- Icons: Material Symbols **Rounded**, weight 400, grade 0, optical size 24, imported as vector drawables (no icon font, no `material-icons-extended`). Names are listed per screen.

Surface roles. One UI and Android 16 settings both show grouped rows on cards over a tinted background; the mapping flips in dark so cards stay lighter than the background:

| Role | Light | Dark |
|---|---|---|
| `screenBackground` | `surfaceContainer` | `surface` |
| `groupContainer` (cards, hero) | `surfaceContainerLowest` | `surfaceContainerHigh` |

Status colours (container / content):

| Meaning | Container | Content |
|---|---|---|
| Connected | `primaryContainer` | `onPrimaryContainer` |
| Connecting, unreachable, waiting, no network | `surfaceContainerHighest` | `onSurfaceVariant` |
| Paused | `tertiaryContainer` | `onTertiaryContainer` |
| Pairing lost, failed item, banner | `errorContainer` | `onErrorContainer` |

Spacing and shape tokens (dp):

| Token | Value | Use |
|---|---|---|
| `screenPadding` | 16 | Horizontal padding of every screen |
| `blockGap` | 16 | Between hero, banner and the Recent section |
| `groupHeader` | top 24, bottom 8, start 16 | Group headers, measured from the card edge |
| `heroPadding` | top 24, sides 20, bottom 20 | Hero card |
| `heroDisc` | 72 (Material icon 32, `ic_stat_pneu` 36 because the glyph has built-in padding) | Status disc |
| `heroButtonHeight` | 48, gap 8 | Hero buttons |
| `rowHeight` | 56 one line, 72 two lines | List rows; 16 horizontal padding |
| `thumb` | 48, corner 12 | Recent thumbnails and type tiles |
| `cardCorner` | 24 | Groups, Recent card |
| `heroCorner` | 28 | Hero card |
| `bannerCorner` | 16 | Banner |
| `maxContentWidth` | 600 | Centre content on wide windows |

Text styles: top app bar title `titleLarge`; large top app bar `headlineMedium`; hero title `headlineSmall`; hero detail `bodyMedium` in `onSurfaceVariant`; group header `titleSmall` in `primary`; row headline `bodyLarge`; row supporting `bodyMedium` in `onSurfaceVariant`; banner `bodyMedium`.

Motion: hero content cross-fades (200 ms) and the disc colour animates when the state changes; Recent rows use `Modifier.animateItem()`; the connecting state draws an indeterminate `CircularProgressIndicator` (80 dp, 3 dp stroke) around the disc.

Text overflow: row headlines and supporting lines are one line. Text snippets end with an ellipsis; file names use `TextOverflow.MiddleEllipsis` so the extension stays visible. The hero detail wraps to at most 2 lines, then ellipsizes. At large font scales the two hero buttons stack vertically (use `FlowRow`).

### 3.2 Navigation

```
first launch ──► Setup ──► Pair ──► Home
                                     ├─► Settings ─┬─► Permissions and battery
                                     │             └─► Unpair (dialog) ──► Pair (back stack cleared)
                                     ├─► banner "Fix" ──► Permissions and battery
                                     └─► hero "Pair again" ──► Pair
```

- Launch routing: Setup not finished → Setup. Not paired → Pair (back exits the app). Otherwise → Home.
- "Setup finished" is set when the user taps Continue, whatever was granted.
- Status notification tap and Quick Settings tile long-press → Home. Alert notification actions → the screen named in the action.
- Share → Send to PC while not paired → opens Pair; the shared items are dropped.
- Predictive back throughout.

### 3.3 Setup (first run) and Permissions and battery

One composable, two modes. First run: `LargeTopAppBar` titled **Set up Pneu**, no back arrow, a **Continue** button pinned at the bottom (full width, 56 dp tall, 16 dp margins plus the navigation inset). From Settings or a banner: titled **Permissions and battery**, back arrow, no Continue.

One card with these rows (leading icon 24 dp in `onSurfaceVariant`; trailing control). The "Never sleeping apps" row appears only when `Build.MANUFACTURER` is `samsung`.

| Row | Icon | Not done: trailing | Not done: supporting | Done |
|---|---|---|---|---|
| Notifications | `notifications` | tonal button **Allow** → `POST_NOTIFICATIONS` request | none | `check_circle` (filled, `primary`) |
| Photos and videos | `photo_library` | **Allow** → `READ_MEDIA_IMAGES` + `READ_MEDIA_VIDEO` | none | check |
| …limited access granted | | **Allow all** → request again | **Limited access** | |
| Unrestricted battery use | `battery_full` | **Allow** → `ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` | none | check |
| Never sleeping apps | `bedtime_off` | **Open** → Samsung's Never sleeping apps list | **Add Pneu to the list** | check |

- A permission denied twice can't be requested again: the button becomes **Settings** (opens `ACTION_APPLICATION_DETAILS_SETTINGS`) and the supporting text is **Turned off in settings**.
- Camera is not on this list. It is requested on the Pair screen, where the reason is obvious.
- Continue is always enabled. Anything left undone shows up later as a Home banner.
- Rows re-check their state in `onResume`, so returning from a system screen updates them immediately.

**Detecting each item** (also used for banners and alerts, section 3.10):

| Item | Done when |
|---|---|
| Notifications | `NotificationManagerCompat.areNotificationsEnabled()` and the Status channel isn't blocked |
| Photos and videos | `READ_MEDIA_IMAGES` granted (and `READ_MEDIA_VIDEO` if Screen recordings is on). Only `READ_MEDIA_VISUAL_USER_SELECTED` granted = limited. |
| Unrestricted battery use | `PowerManager.isIgnoringBatteryOptimizations(packageName)` and not `ActivityManager.isBackgroundRestricted()` |
| Never sleeping apps | Can't be read directly. Marked done when the user comes back from that screen; reset to not done if Pneu's app standby bucket is `RARE` or `RESTRICTED`, or `getHistoricalProcessExitReasons()` shows the service was killed by the system while it should have been running. |

Samsung intent for the list: try the Device care "Never sleeping apps" activity first, fall back to Device care's battery screen, then to `ACTION_APPLICATION_DETAILS_SETTINGS`. **Verify the exact component on the S25+ in the spike.**

### 3.4 Pair

The Pair screen always uses the dark colour scheme (wrap it in `MaterialTheme(colorScheme = darkScheme)`), in both themes, like other camera screens. Full-screen camera preview (CameraX `PreviewView`, ML Kit barcode scanning, QR only). A transparent top app bar with a back arrow and the title **Pair with PC**, both white. A black scrim at 55 % covers everything except a 256 dp rounded square (corner 24) centred slightly above the middle, outlined in white 2 dp. Below the square, 24 dp gap, white `bodyLarge`, centred:

**Scan the QR code on your PC**

States:

| State | What shows |
|---|---|
| Camera not granted yet | Black screen, `photo_camera` icon 48 dp (white 70 %), filled button **Allow camera** below it. Requested automatically the first time the screen opens. |
| Camera permanently denied | Same, with the line **Camera access is off** above the button, and the button reads **Open settings**. |
| Scanning | Preview, scrim, outline, instruction. |
| Not a Pneu code | Snackbar **That isn't a Pneu code** (3 s). Scanning continues; the same code is ignored for 3 s. |
| Expired code | Snackbar **This code expired. Scan the new one.** The PC window refreshes its code on its own. |
| Code read, pairing | The square's outline turns `primary` and 4 dp thick; a 48 dp `CircularProgressIndicator` sits in the square; the instruction becomes **Pairing…**. Back is disabled for up to 15 s. |
| Can't reach the PC | `AlertDialog`: title **Can't reach your PC**, text **Make sure both are on the same Wi-Fi, or that Tailscale is connected.**, buttons **Cancel** (resume scanning) and **Try again** (retry with the same code). |
| Paired | The square shows a 64 dp `primary` disc with a `check` icon; instruction **Paired with {pc}**; after 800 ms navigate to Home (connected). The service starts now. |

### 3.5 Home

`TopAppBar` with title **Pneu** and one action, `settings` (content description **Settings**). Content scrolls in a `LazyColumn`: optional banner, hero card, Recent section.

**Banner** (only when something in section 3.3 is not done, or a background kill was detected). `errorContainer` surface, corner 16, padding 12/16, `warning` icon, one line of text, `TextButton` **Fix** → Permissions and battery. Only one banner shows. If several items need attention, it reads **{n} settings need attention**; otherwise one of these, in priority order:

1. **Pneu can't see new screenshots** (photos missing or limited)
2. **Pneu was stopped in the background** (system kill detected)
3. **Battery optimization can stop Pneu**
4. **Pneu isn't in Never sleeping apps**
5. **Notifications are off**

**Hero card** (`groupContainer`, corner 28, contents centred): status disc, title, detail line, two buttons side by side. The left button is `FilledTonalButton`, the right is `OutlinedButton`, unless noted.

| State | Disc icon / colours | Title | Detail | Left button | Right button |
|---|---|---|---|---|---|
| Connecting | `ic_stat_pneu` / neutral + spinner ring | **Connecting…** | `{pc}` | Send clipboard (disabled) | Pause |
| Connected, Wi-Fi | `ic_stat_pneu` / connected | **Connected** | `{pc} · Wi-Fi` | **Send clipboard** | **Pause** |
| Connected, Tailscale | same | **Connected** | `{pc} · Tailscale` | Send clipboard | Pause |
| PC unreachable | `cloud_off` / neutral | **PC unreachable** | **Last connected {time}** | Send clipboard (disabled) | Pause |
| PC unreachable, Tailscale off (not on the home network, no VPN transport active) | `cloud_off` / neutral | **PC unreachable** | **Tailscale isn't connected** | **Open Tailscale** (launches `com.tailscale.ipn`) | Pause |
| Waiting for Wi-Fi | `wifi_off` / neutral | **Waiting for Wi-Fi** | **Mobile data is off in settings** | Send clipboard (disabled) | Pause |
| No network | `cloud_off` / neutral | **No network** | **Last connected {time}** | Send clipboard (disabled) | Pause |
| Paused | `pause` / paused | **Paused** | **Automatic sending is off** | Send clipboard (enabled when connected) | **Resume**, filled `Button` |
| Pairing lost | `error` / error | **Pairing lost** | **{pc} doesn't recognize this phone** | one full-width filled **Pair again** | none |

- Precedence: Pairing lost, then Paused, then the connection state.
- Button icons: Send clipboard `content_paste_go`, Pause `pause`, Resume `play_arrow`, Open Tailscale `open_in_new`, Pair again `qr_code_scanner`.
- `{time}`: `h:mm`/`HH:mm` per system setting if today, **Yesterday**, otherwise a short date (`d MMM`).
- Send clipboard here reads the clipboard directly (the app has focus) and shows the same toasts as section 3.9.
- Pause/Resume act immediately; no confirmation.

**Recent section.** Header row: **Recent** (group header style) at the start; when items are waiting, a `TextButton` **{n} waiting** with a trailing `arrow_drop_down` icon at the end that opens a `DropdownMenu` with **Send now** (retry the connection immediately) and **Don't send any** (empties the queue, no confirmation). Below it, one card with up to 50 rows, newest first; waiting and sending rows are always on top in send order.

Row anatomy: leading 48 dp thumbnail (images and videos centre-cropped, corner 12; videos get a 16 dp `play_arrow` badge at bottom-start) or a 48 dp `secondaryContainer` tile with an icon (`description` for files, `link` for links, `content_paste` for clipboard and text). Headline one line, supporting one line, optional trailing icon buttons.

| Item | Headline |
|---|---|
| Screenshot | **Screenshot** |
| Camera photo | **Photo** |
| Screen recording | **Screen recording** |
| Shared image / video / file | file name (middle ellipsis) |
| Shared several at once | one row per item |
| Clipboard or shared text | first line of the text |
| Shared link | the URL without `https://` |

| Status | Supporting text | Trailing |
|---|---|---|
| Sent | `{time}`; for clipboard rows **Clipboard · {time}** | none |
| Waiting | **Waiting** | `close` button, description **Don't send** (removes the item) |
| Held for Wi-Fi (a screen recording on mobile data with originals off) | **Waiting for Wi-Fi** | same as Waiting |
| Sending (shown only for transfers over 1 s) | **Sending · {sent} of {total}** (sizes like `12 MB`) | `close` button with a determinate progress ring around it, description **Cancel** |
| Failed | **Failed · PC disk is full**, **Failed · PC can't save files** (in `error`) | `refresh` (**Retry**) and `close` (**Remove**) |

Completed rows keep metadata and a small thumbnail, not the full file. Tapping a sent automatic image or video opens its original MediaStore item with `ACTION_VIEW` if it still exists; a sent link opens in the browser; other completed rows do nothing. Pending previews use staged bytes.

Empty Recent (just paired): the card holds a 120 dp area with an `image` icon (32 dp, `onSurfaceVariant`) and **Nothing sent yet**.

### 3.6 Settings

`LargeTopAppBar` **Settings** with a back arrow. Groups: a header, then a card of rows. Toggle rows are whole-row clickable with a trailing `Switch` (on-state thumb shows a `check` icon). No leading icons.

**Send automatically**

| Row | Default |
|---|---|
| Screenshots | On |
| Camera photos | Off |
| Screen recordings | Off |

Turning on Camera photos or Screen recordings re-checks media permissions; if `READ_MEDIA_VIDEO` is missing, the system prompt shows immediately.

**Connection**

| Row | Default | Notes |
|---|---|---|
| Direct on home Wi-Fi | On | |
| Tailscale when away | On | If the Tailscale app isn't installed, supporting text **Tailscale app isn't installed** in `error`. |
| Use mobile data | On | |
| Send originals on mobile data | Off | Disabled (38 % alpha) while Use mobile data is off. |
| Queue while PC is unreachable | On | |

Direct on home Wi-Fi and Tailscale when away can't both be off: turning off the last one leaves it on and shows the snackbar **Pneu needs Wi-Fi or Tailscale**.

**Clipboard**

| Row | Default | Notes |
|---|---|---|
| Send button in notification | On | Adds **Send clipboard** to the status notification. The Home button and the Quick Settings tile are not affected. |

**Paired PC**

| Row | Behaviour |
|---|---|
| `{pc}`, supporting text: the addresses it uses, `192.168.1.20 · 100.67.144.28` (only the ones known) | Not clickable. |
| **Unpair**, text in `error` | Opens the Unpair dialog. |

**This phone**

| Row | Behaviour |
|---|---|
| **Permissions and battery**, supporting **All set** or **{n} needs attention** / **{n} need attention** (plural, in `error`) | Opens Permissions and battery. |

Footer: **Version {versionName}**, `bodySmall`, `onSurfaceVariant`, centred, 24 dp padding.

**Unpair dialog** (`AlertDialog`): title **Unpair from {pc}?**. Text only when items are waiting: **{n} waiting items won't be sent.** Buttons **Cancel** and **Unpair**. Unpair stops the service, clears the queue and the Recent list, sends `unpair` if connected, and navigates to Pair with the back stack cleared.

### 3.7 Status notification (foreground service)

Channel `status`, name **Status**, `IMPORTANCE_LOW` (silent; One UI lists it under Silent). Builder: `setOngoing(true)`, `setOnlyAlertOnce(true)`, `setShowWhen(false)`, `setSilent(true)`, `setCategory(CATEGORY_SERVICE)`, `setForegroundServiceBehavior(FOREGROUND_SERVICE_IMMEDIATE)`, small icon `ic_stat_pneu`, colour `#0F6E68`. Tap opens Home.

Title and text mirror the hero:

| State | Title | Text | Actions |
|---|---|---|---|
| Connecting | **Connecting…** | `{pc}` | Send clipboard, Pause |
| Connected | **Connected** | `{pc} · Wi-Fi` / `{pc} · Tailscale` | Send clipboard, Pause |
| Sending (over 1 s) | **Sending screenshot** / **Sending photo** / **Sending screen recording** / **Sending {file name}**; several: **Sending {n} items** | **{sent} of {total}**, determinate progress bar | Send clipboard, Pause |
| PC unreachable | **PC unreachable** | **{n} waiting** if any, else **Last connected {time}** | Send clipboard, Pause |
| Waiting for Wi-Fi | **Waiting for Wi-Fi** | **{n} waiting** if any, else **Mobile data is off in settings** | Send clipboard, Pause |
| No network | **No network** | **{n} waiting** if any, else **Last connected {time}** | Send clipboard, Pause |
| Paused | **Paused** | **Automatic sending is off** | Send clipboard, **Resume** |

When pairing is lost the service stops (there is nothing to connect to), so the status notification goes away and the Pairing lost alert below takes its place.

- **Send clipboard** appears only when Settings → Send button in notification is on. Its `PendingIntent` targets the clipboard activity directly (`PendingIntent.getActivity`), not a receiver, because Android blocks notification trampolines and background clipboard reads (section 3.9).
- Pause/Resume use a `PendingIntent.getService`/broadcast; they act silently and the notification updates.

**Alerts channel**: id `alerts`, name **Alerts**, `IMPORTANCE_DEFAULT`. Posted only when the app isn't in the foreground, at most once per 24 h per alert, and cancelled when the cause is fixed.

| Cause | Title | Text | Action |
|---|---|---|---|
| Battery optimization or background restriction turned back on | **Pneu may stop in the background** | **Battery use is set to Optimized** | **Fix** → Permissions and battery |
| Service killed by the system | **Pneu was stopped in the background** | **Add Pneu to Never sleeping apps** | **Fix** → Permissions and battery |
| Photo access revoked or limited | **Pneu can't see new screenshots** | **Photo access was turned off** | **Fix** → Permissions and battery |
| Pairing lost | **Pairing lost** | **{pc} doesn't recognize this phone** | **Pair again** → Pair |

### 3.8 Quick Settings tiles

Two `TileService`s. The Pneu tile's icon is `ic_stat_pneu`; the Send clipboard tile's icon is Material Symbol `content_paste_go`.

**Pneu** (toggle, `android.service.quicksettings.TOGGLEABLE_TILE` = true). Label **Pneu**. Tap toggles Pause. Long-press opens Home (declare the Home activity for `ACTION_QS_TILE_PREFERENCES`).

| State | Tile state | Subtitle |
|---|---|---|
| Connected | `STATE_ACTIVE` | **Wi-Fi** / **Tailscale** |
| Connecting | `STATE_ACTIVE` | **Connecting…** |
| Unreachable, waiting for Wi-Fi, no network | `STATE_ACTIVE` | **Offline** |
| Paused | `STATE_INACTIVE` | **Paused** |
| Pairing lost | `STATE_INACTIVE` | **Pairing lost** (tap opens Home instead of toggling) |
| Not paired | `STATE_UNAVAILABLE` | **Not paired** |

**Send clipboard** (action tile). Label **Send clipboard**. `STATE_INACTIVE` normally, with no subtitle when connected and **Offline** when not; `STATE_UNAVAILABLE` with **Not paired** when not paired or pairing lost. Tap → `startActivityAndCollapse(PendingIntent)` to the clipboard activity.

### 3.9 Clipboard and share target

**Send clipboard activity.** Android 10+ only lets the focused app read the clipboard, so the notification action and the tile open `SendClipboardActivity`: translucent theme, no UI, `excludeFromRecents`, `noHistory`, its own task affinity. It reads the clipboard in `onWindowFocusChanged(true)`, hands it to the service, shows a toast and finishes. Text is sent as text; a clip with an image URI is sent as a clipboard image. Android 12+ also shows its own "pasted from your clipboard" toast; that is expected.

Toasts (plain text toasts, which work from any context):

| Situation | Toast |
|---|---|
| Sent | **Clipboard sent** |
| Clipboard empty | **Clipboard is empty** |
| Not connected (clipboard is never queued) | **PC unreachable. Not sent.** |

**Share target.** Activity label **Send to PC**, Pneu icon, `ACTION_SEND` and `ACTION_SEND_MULTIPLE` for `*/*` and `text/plain`. Translucent, no UI: it captures the paired PC once, copies granted content URIs to private storage, queues them, toasts and finishes. No persistable URI grants or Direct Share shortcuts are published; cached shortcut launches are rejected.

| Situation | Toast |
|---|---|
| Connected (paused or not) | **Sending to PC** / **Sending {n} items to PC** |
| Not connected, queue on or off (shared items always queue) | **PC unreachable. Queued.** |
| Waiting for Wi-Fi | **Waiting for Wi-Fi. Queued.** |
| An item can't be read | **Couldn't read this item** |
| Not paired | No toast; opens Pair. |

No confirmation dialog: sharing is already deliberate. Progress for big files shows in the status notification; the PC's notification is the success signal. Shared text and links are delivered to the PC clipboard (section 4.5).

**PC clipboard arriving on the phone.** Set with `ClipboardManager.setPrimaryClip` from the service. Pneu shows nothing; Android 13+ shows its own copy overlay. Ignored while paused.

### 3.10 Health checks

Run the section 3.3 checks when Home resumes, when the service starts, and once a day from the service. On service start, read `getHistoricalProcessExitReasons()`: an exit while the service should have been running, with a reason other than user request or app update, counts as "stopped in the background". Results feed the Home banner, the Settings row's supporting text, and the Alerts channel.

### 3.11 Phone strings

Sentence case. Use plurals resources for every `{n}`.

| Key | Text |
|---|---|
| `app_name` | Pneu |
| `share_label` | Send to PC |
| `setup_title` | Set up Pneu |
| `permissions_title` | Permissions and battery |
| `perm_notifications` | Notifications |
| `perm_photos` | Photos and videos |
| `perm_photos_limited` | Limited access |
| `perm_battery` | Unrestricted battery use |
| `perm_never_sleeping` | Never sleeping apps |
| `perm_never_sleeping_hint` | Add Pneu to the list |
| `perm_denied` | Turned off in settings |
| `action_allow` | Allow |
| `action_allow_all` | Allow all |
| `action_open` | Open |
| `action_settings` | Settings |
| `action_continue` | Continue |
| `pair_title` | Pair with PC |
| `pair_instruction` | Scan the QR code on your PC |
| `pair_allow_camera` | Allow camera |
| `pair_camera_off` | Camera access is off |
| `action_open_settings` | Open settings |
| `pair_not_pneu` | That isn't a Pneu code |
| `pair_expired` | This code expired. Scan the new one. |
| `pair_pairing` | Pairing… |
| `pair_success` | Paired with %1$s |
| `pair_unreachable_title` | Can't reach your PC |
| `pair_unreachable_text` | Make sure both are on the same Wi-Fi, or that Tailscale is connected. |
| `action_try_again` | Try again |
| `action_cancel` | Cancel |
| `home_title` | Pneu |
| `cd_settings` | Settings |
| `status_connecting` | Connecting… |
| `status_connected` | Connected |
| `status_unreachable` | PC unreachable |
| `status_waiting_wifi` | Waiting for Wi-Fi |
| `status_no_network` | No network |
| `status_paused` | Paused |
| `status_pairing_lost` | Pairing lost |
| `detail_wifi` | %1$s · Wi-Fi |
| `detail_tailscale` | %1$s · Tailscale |
| `detail_last_connected` | Last connected %1$s |
| `detail_tailscale_off` | Tailscale isn't connected |
| `detail_mobile_data_off` | Mobile data is off in settings |
| `detail_paused` | Automatic sending is off |
| `detail_pairing_lost` | %1$s doesn't recognize this phone |
| `time_yesterday` | Yesterday |
| `action_send_clipboard` | Send clipboard |
| `action_pause` | Pause |
| `action_resume` | Resume |
| `action_open_tailscale` | Open Tailscale |
| `action_pair_again` | Pair again |
| `action_fix` | Fix |
| `banner_photos` | Pneu can't see new screenshots |
| `banner_killed` | Pneu was stopped in the background |
| `banner_battery` | Battery optimization can stop Pneu |
| `banner_never_sleeping` | Pneu isn't in Never sleeping apps |
| `banner_notifications` | Notifications are off |
| `banner_many` (plural) | %d settings need attention |
| `recent_title` | Recent |
| `recent_waiting` (plural) | %d waiting |
| `recent_send_now` | Send now |
| `recent_dont_send_any` | Don't send any |
| `recent_empty` | Nothing sent yet |
| `kind_screenshot` | Screenshot |
| `kind_photo` | Photo |
| `kind_recording` | Screen recording |
| `item_clipboard_time` | Clipboard · %1$s |
| `item_waiting` | Waiting |
| `item_waiting_wifi` | Waiting for Wi-Fi |
| `item_sending` | Sending · %1$s of %2$s |
| `item_failed_disk` | Failed · PC disk is full |
| `item_failed_write` | Failed · PC can't save files |
| `cd_dont_send` | Don't send |
| `cd_cancel` | Cancel |
| `cd_retry` | Retry |
| `cd_remove` | Remove |
| `settings_title` | Settings |
| `group_auto` | Send automatically |
| `pref_screenshots` | Screenshots |
| `pref_photos` | Camera photos |
| `pref_recordings` | Screen recordings |
| `group_connection` | Connection |
| `pref_wifi` | Direct on home Wi-Fi |
| `pref_tailscale` | Tailscale when away |
| `pref_tailscale_missing` | Tailscale app isn't installed |
| `pref_mobile_data` | Use mobile data |
| `pref_originals` | Send originals on mobile data |
| `pref_queue` | Queue while PC is unreachable |
| `snackbar_need_route` | Pneu needs Wi-Fi or Tailscale |
| `group_clipboard` | Clipboard |
| `pref_clipboard_button` | Send button in notification |
| `group_pc` | Paired PC |
| `pref_unpair` | Unpair |
| `group_phone` | This phone |
| `pref_permissions` | Permissions and battery |
| `pref_permissions_ok` | All set |
| `pref_permissions_issues` (plural) | one: %d needs attention · other: %d need attention |
| `version` | Version %1$s |
| `unpair_title` | Unpair from %1$s? |
| `unpair_text` (plural) | %d waiting items won't be sent. |
| `action_unpair` | Unpair |
| `channel_status` | Status |
| `channel_alerts` | Alerts |
| `notif_sending_screenshot` | Sending screenshot |
| `notif_sending_photo` | Sending photo |
| `notif_sending_recording` | Sending screen recording |
| `notif_sending_file` | Sending %1$s |
| `notif_sending_many` (plural) | Sending %d items |
| `notif_progress` | %1$s of %2$s |
| `notif_waiting` (plural) | %d waiting |
| `alert_battery_title` | Pneu may stop in the background |
| `alert_battery_text` | Battery use is set to Optimized |
| `alert_killed_title` | Pneu was stopped in the background |
| `alert_killed_text` | Add Pneu to Never sleeping apps |
| `alert_photos_title` | Pneu can't see new screenshots |
| `alert_photos_text` | Photo access was turned off |
| `tile_pneu` | Pneu |
| `tile_send_clipboard` | Send clipboard |
| `tile_wifi` | Wi-Fi |
| `tile_tailscale` | Tailscale |
| `tile_offline` | Offline |
| `tile_not_paired` | Not paired |
| `toast_clipboard_sent` | Clipboard sent |
| `toast_clipboard_empty` | Clipboard is empty |
| `toast_not_sent` | PC unreachable. Not sent. |
| `toast_sending` | Sending to PC |
| `toast_sending_many` (plural) | Sending %d items to PC |
| `toast_queued` | PC unreachable. Queued. |
| `toast_queued_wifi` | Waiting for Wi-Fi. Queued. |
| `toast_cant_read` | Couldn't read this item |

---

## 4. PC (Rust daemon, KDE Plasma 6.6, Wayland)

### 4.1 States and tray icon

| State | Tray icon | SNI status | Tooltip text (title is always **Pneu**) |
|---|---|---|---|
| No phone paired | `pneu-tray-disconnected` | Active | **No phone paired** |
| Pairing in progress | `pneu-tray-disconnected` | Active | **Pairing with {phone}…** |
| Paired, phone not connected | `pneu-tray-disconnected` | Active | **{phone} isn't connected** / second line **Last seen {time}** |
| Connected | `pneu-tray-connected` | Active | **{phone} connected over Wi-Fi** or **…over Tailscale** |
| Receiving | `pneu-tray-receiving` (shown at least 1.5 s) | Active | **Receiving from {phone}…** |
| Phone paused | `pneu-tray-paused` | Active | **{phone} paused sending** |
| Error | `pneu-tray-error` | NeedsAttention | **Can't receive: port {port} is in use**, **Can't save to {folder}** or **The disk with {folder} is full** |

- Route: a peer address in `100.64.0.0/10` (or Tailscale's IPv6 range) is **Tailscale**; anything else is **Wi-Fi**.
- Error state lasts until the cause clears: port errors when the listener binds, save errors on the next successful save or a folder change.
- `{time}` as on the phone; older than yesterday shows the date.
- Left click (SNI `Activate`) opens the window. Middle click and scroll do nothing. Category `ApplicationStatus`, id `pneu`, title **Pneu**.

### 4.2 Tray menu

DBusMenu via `ksni`. Checkable items use `ToggleType::CheckMark`; changes save to `config.toml` immediately and take effect on the next arrival.

```
{status line}                       disabled item, same text as the tooltip's first line
──────────────
Open Folder                         icon folder-open; opens the save folder
──────────────
✓ Save to Folder
✓ Show Notifications
✓ Copy Images to Clipboard
  Open in Image Viewer
──────────────
✓ Send Clipboard to Phone
  Include Images                    disabled while Send Clipboard to Phone is off
──────────────
Configure Pneu…                     icon configure; when unpaired: "Pair Phone…", icon smartphone
Quit                                icon application-exit
```

| Item | Default | Effect |
|---|---|---|
| Save to Folder | On | Save arrivals to the save folder. Off: keep them in `~/.cache/pneu/received/` for 24 h so notifications can still open them. |
| Show Notifications | On | Section 4.4 receive notifications. Errors always notify. |
| Copy Images to Clipboard | On | Images (screenshots, photos, shared images) go on the clipboard. Text always goes on the clipboard. |
| Open in Image Viewer | Off | Open images with the default app (`xdg-open`, or the OpenURI portal). In a burst only the last one opens, 1 s after the burst ends. |
| Send Clipboard to Phone | On | PC clipboard text goes to the phone (section 1 rules). |
| Include Images | Off | Also send clipboard images (PNG, max 10 MB). |

- Quit exits with status 0 so the systemd unit (`Restart=on-failure`) stays stopped until the next login.
- Open Folder creates the folder if it's missing.
- The status line is the tooltip's first line, for example **Galaxy S25+ connected over Wi-Fi**, **Galaxy S25+ isn't connected**, **No phone paired**, **Can't save to ~/Pictures/Pneu**.

### 4.3 Window (Slint)

One window, title **Pneu**, icon `pneu`, 400 logical px wide, height fits the content, not resizable. Opens from the tray (left click, **Configure Pneu…**, **Pair Phone…**) and on its own when the daemon starts unpaired. Closing it hides it; the daemon keeps running. Only one instance; opening it again raises it.

Style: Slint's **Qt backend with the `native` style**, so widgets are real Breeze widgets that follow the light/dark colour scheme and accent. Colours come from Slint's `Palette` (no hard-coded colours) except the QR code (black on white, always) and the status dot. Fall back to the `fluent` style only if the Qt backend can't be used.

Layout tokens (logical px): content margins 18; vertical spacing between rows 12; label-to-control spacing 6; separators are Breeze 1 px lines (`Palette.border`). Text uses the system font (Noto Sans 10 pt by default); the phone name is 1.2× and semibold; secondary text is `Palette.foreground` at 70 % opacity.

Status dot colours (8 px circle): connected `#27AE60` (Breeze positive), paused `#F67400` (Breeze neutral), not connected: the secondary text colour, error `#DA4453` (Breeze negative).

**Unpaired**

```
┌ Pneu ────────────────────────────────── ─ ✕ ┐
│                                              │
│              ┌────────────────┐              │
│              │    QR  232px   │              │
│              └────────────────┘              │
│         Scan with Pneu on your phone         │
│ ──────────────────────────────────────────── │
│ Save to:  ~/Pictures/Pneu          [Change…] │
│                                     [Close]  │
└──────────────────────────────────────────────┘
```

- QR: 232 px, black modules on a white rounded (6 px) card with a 4-module quiet zone, error correction M. It encodes the pairing payload from `PROTOCOL.md`. The code (and its one-time token) refreshes every 5 minutes while the window is open, without any visible countdown.
- No usable address (no LAN IPv4 and no Tailscale address): the QR area shows a 48 px `network-disconnect` icon and **Connect this PC to a network to pair** instead.
- When the phone connects with the token: the QR area shows a busy indicator and **Pairing with {phone}…**. On success the window switches to the paired layout; on failure it returns to a fresh QR.

**Paired**

```
┌ Pneu ────────────────────────────────── ─ ✕ ┐
│ [phone icon 48]  Galaxy S25+                 │
│                  ● Connected over Wi-Fi      │
│ ──────────────────────────────────────────── │
│ Save to:  ~/Pictures/Pneu          [Change…] │
│ ──────────────────────────────────────────── │
│ [Unpair…]                           [Close]  │
└──────────────────────────────────────────────┘
```

Status line text: **Connected over Wi-Fi**, **Connected over Tailscale**, **Not connected · last seen {time}**, **Paused on phone**, **Receiving…**.

- Phone icon: `smartphone` from the icon theme, 48 px.
- Path label: `~` for home, middle-elided if it doesn't fit.
- **Change…** opens the xdg-desktop-portal FileChooser (`OpenFile` with `directory: true`, title **Choose Folder**, current folder preselected), which is KDE's own dialog. Existing files are not moved.
- **Unpair…** opens a modal Slint `PopupWindow` centred over the window (no title bar, window content dimmed): text **Unpair {phone}?**, buttons **Cancel** (default) and **Unpair**. Unpair sends `unpair` if connected, forgets the phone and shows the Unpaired layout with a fresh QR (that is also how to pair a different phone).
- **Close** is the default button; Escape closes.

**Inline error banner** (KMessageWidget look: rounded 4 px, background `#DA4453` at 15 % over the window colour, 1 px border `#DA4453` at 50 %, `dialog-error` icon 16 px, text, optional button), shown above everything else while the error lasts:

| Error | Text | Button |
|---|---|---|
| Port in use | **Port {port} is in use by another program.** | none (the daemon retries every 30 s) |
| Folder not writable or missing | **Can't write to {folder}.** | none (the Save to row's **Change…** is right below) |
| Disk full | **The disk with {folder} is full.** | none |

### 4.4 Notifications

Sent over `org.freedesktop.Notifications` with `zbus`. Every notification sets `app_name` **Pneu**, `app_icon` `pneu`, hint `desktop-entry` = `pneu` (install `pneu.desktop`, `NoDisplay=true`, so Plasma shows the right name and icon and the user can configure Pneu under System Settings → Notifications).

**Image and file previews** use KDE's `x-kde-urls` hint (array of `file://` URLs). Plasma then shows a large thumbnail that can be dragged straight into another app, the same way Spectacle's notifications work. Don't also set `image-data`/`image-path` for these, or the image shows twice. When saving is off, the URL points at the cache copy.

| Event | Summary | Body | Actions (`id` → label) | Hints |
|---|---|---|---|---|
| Screenshot | **Screenshot** | **Copied to clipboard** if it was copied, else empty | `default` → open; `open` → **Open**; `show` → **Show in Folder** (only if saved) | `x-kde-urls`, `category` `transfer.complete` |
| Camera photo | **Photo** | as above | as above | as above |
| Screen recording | **Screen recording** | empty | `default`, **Open**, **Show in Folder** | as above |
| Shared file | file name | **Copied to clipboard** if it's an image that was copied, else empty | `default`, **Open**, **Show in Folder** | as above |
| Burst (next arrival within 4 s while the previous notification is still showing; reuse its id via `replaces_id`) | **{n} screenshots** / **{n} photos** / **{n} items** | **Latest copied to clipboard** if applicable | `show` → **Show in Folder** | `x-kde-urls` with all items, latest first (Plasma previews the first URL) |
| Text from phone (Send clipboard or a shared text/link) | **Copied from phone** | the text, first 3 lines, max 200 characters | `link` → **Open Link** if the text is a single URL | `transient` = true, `urgency` = 0 (low) |
| Image clipboard from phone | **Image copied from phone** | empty | none | `image-data` (preview, max 256 px), `transient` = true, `urgency` 0 |

- **Open** and `default` open the file with the default app (OpenURI portal, or `xdg-open`). **Show in Folder** calls `org.freedesktop.FileManager1.ShowItems` so Dolphin opens with the file selected (all files in a burst).
- Show Notifications off → no receive notifications. Errors below are always shown.
- Expiry: `-1` (server default) for everything except `transient` ones, which use 4000 ms.
- No sound hint.

**Errors** (`urgency` 1, not transient):

| Event | Summary | Body | Actions |
|---|---|---|---|
| Couldn't save, disk full | the item's normal summary (for example **Screenshot**) | **Not saved: the disk is full** | **Open** (opens the cache copy) |
| Couldn't save, folder unwritable or missing | normal summary | **Not saved: can't write to {folder}** | **Open**, `folder` → **Choose Folder…** (opens the window and the folder picker) |
| Couldn't put it on the clipboard | normal summary | **Couldn't copy to clipboard** | as for success |
| Unpaired phone tried to connect (pin mismatch). At most once an hour. | **Blocked an unpaired phone** | **{address} tried to connect** | `pair` → **Pair Phone…** |
| Phone unpaired this PC | **{phone} unpaired this PC** | empty | `pair` → **Pair Phone…** |
| Can't listen on the port (once per daemon start) | **Pneu can't receive** | **Port {port} is in use by another program** | none |

A save error keeps the preview and clipboard behaviour: the item still arrives (from the cache), only the body changes. These also send the error back to the phone (section 1, Save errors).

### 4.5 Clipboard behaviour

- Phone → PC text: set the Wayland CLIPBOARD (`wl-clipboard-rs`), MIME `text/plain;charset=utf-8`; show the **Copied from phone** notification. Shared links behave the same way and also offer **Open Link**.
- Phone → PC image: set the clipboard with the image's MIME type (and `image/png`); **Image copied from phone** notification. Not saved to the folder.
- Received screenshots and photos with Copy Images to Clipboard on: set the clipboard to the image after saving; mark it so the watcher doesn't send it back.
- PC → phone: watch CLIPBOARD; on change, skip content Pneu set, skip `x-kde-passwordManagerHint` = `secret`, skip images unless Include Images is on, skip if not connected or the phone is paused; otherwise send. No notification on either side.

### 4.6 PC strings

Tooltip and menu status line:
- No phone paired
- Pairing with {phone}…
- {phone} isn't connected (second tooltip line: Last seen {time})
- {phone} connected over Wi-Fi
- {phone} connected over Tailscale
- Receiving from {phone}…
- {phone} paused sending
- Can't receive: port {port} is in use
- Can't save to {folder}
- The disk with {folder} is full

Menu: Open Folder, Save to Folder, Show Notifications, Copy Images to Clipboard, Open in Image Viewer, Send Clipboard to Phone, Include Images, Configure Pneu…, Pair Phone…, Quit.

Window:
- Title: Pneu
- Unpaired: Scan with Pneu on your phone / Connect this PC to a network to pair / Pairing with {phone}…
- Paired status: Connected over Wi-Fi / Connected over Tailscale / Not connected · last seen {time} / Paused on phone / Receiving…
- Rows and buttons: Save to: / Change… / Close / Unpair…
- Folder picker title: Choose Folder
- Unpair popup: Unpair {phone}? / Cancel / Unpair
- Banners: Port {port} is in use by another program. / Can't write to {folder}. / The disk with {folder} is full.

Notifications:
- Summaries: Screenshot / Photo / Screen recording / {file name} / {n} screenshots / {n} photos / {n} items / Copied from phone / Image copied from phone / Blocked an unpaired phone / {phone} unpaired this PC / Pneu can't receive
- Bodies: Copied to clipboard / Latest copied to clipboard / Not saved: the disk is full / Not saved: can't write to {folder} / Couldn't copy to clipboard / {address} tried to connect / Port {port} is in use by another program
- Actions: Open / Show in Folder / Open Link / Choose Folder… / Pair Phone…

---

## 5. For the spike and the implementers

1. **Plasma tray recolouring** from an app-supplied `IconThemePath` (section 2). Fallback is described there.
2. **Samsung "Never sleeping apps" intent** and whether "Unrestricted" battery already adds Pneu to that list on One UI 8. If it does, drop the separate row.
3. **`x-kde-urls` rendering** in Plasma 6.6 notifications from a non-KNotification client (the thumbnail and drag-out). If it doesn't render, use `image-path` with the file URL instead and keep the same actions.
4. **Status notification importance.** `IMPORTANCE_LOW` keeps the **Send clipboard** button one swipe away. If its status-bar icon is unwanted, the user can set the channel to minimised in One UI; don't use `IMPORTANCE_MIN` by default.
5. **Clipboard read from the translucent activity** on One UI: confirm `onWindowFocusChanged` gets focus when launched from the tile with the shade collapsing.
6. Protocol fields this design needs are marked **protocol** in section 1.
