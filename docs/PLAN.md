# Pneu — plan

Send things from a Samsung S25+ to this PC (Kubuntu 26.04, Plasma 6.6, Wayland) instantly. Personal use, installed over USB. Performance first.

## Version 1 scope

### Phone → PC
| Feature | Default | Notes |
|---|---|---|
| Send screenshots | On | Main feature |
| Send new camera photos | Off | Toggle |
| Send screen recordings | Off | Toggle; large files, so off by default |
| Share sheet "Send to PC" | Always | Any file, text or link, from any app |
| Clipboard → PC | On | Needs a tap (see Android limits below) |
| Queue while PC is unreachable | On | Sends the backlog when the connection returns |

### PC → phone
| Feature | Default | Notes |
|---|---|---|
| Clipboard → phone | On | Text automatically; images too when enabled |

### What the PC does when something arrives (each a toggle)
| Action | Default |
|---|---|
| Save to folder (default `~/Pictures/Pneu`, changeable) | On |
| Notification with preview, click to open | On |
| Copy image to clipboard | On |
| Open in image viewer | Off |

### Connection options (phone)
| Option | Default |
|---|---|
| Home Wi-Fi (direct) | On |
| Tailscale when away from home | On |
| Allow on mobile data | On |
| Send originals on mobile data (otherwise compressed) | Off |
| Pause | Quick Settings tile + notification button |

The phone tries the direct Wi-Fi address first and falls back to the PC's Tailscale address (`100.67.144.28`). The phone needs the Tailscale app connected for the fallback.

Each side keeps its own settings: the phone decides what it sends, the PC decides what it does with what it receives.

## Tech stack

### Phone (`android/`)
- Kotlin, Jetpack Compose, coroutines. No cross-platform framework.
- Detects new media with a `MediaStore` content observer inside a foreground service.
- Pairing by scanning a QR code shown on the PC (CameraX + ML Kit barcode scanning, both on-device).
- Sends original bytes; no re-encode except the optional mobile-data compression.

### PC (`desktop/`)
- Rust: `tokio`, `rustls`, `mdns-sd`, `zbus` (notifications with image previews), `wl-clipboard-rs` (Wayland clipboard), `ksni` (tray icon).
- Runs as a systemd user service.
- Settings in `~/.config/pneu/config.toml`; toggles in the tray menu; a small window for pairing (QR code) and choosing the save folder.

### Protocol (`docs/PROTOCOL.md`, written before any code)
- One long-lived TCP connection with TLS 1.3. Each side pins the other's self-signed certificate, exchanged through the QR code.
- Length-prefixed binary frames: hello, ping, file (streamed in chunks with an ID and SHA-256), clipboard (text or image), ack.
- The phone dials the PC. Addresses are cached: LAN address from mDNS, then the Tailscale address.
- Files are deduplicated by their media ID, so nothing is sent twice after a reconnect.

## Known Android and Samsung limits

1. **Background clipboard reading is blocked** on Android 10 and later. The phone can't watch your clipboard silently. Sending the phone's clipboard to the PC takes one tap: a Quick Settings tile, a button on Pneu's notification, or Share → Pneu. PC → phone clipboard can be automatic.
2. **Samsung puts background apps to sleep.** Onboarding walks you through "Never sleeping apps" and turning off battery optimization, and the app warns if they get switched back on.
3. **Foreground service type.** Android 15 limits the `dataSync` type to 6 hours a day, so Pneu uses `connectedDevice` or `specialUse`. To be confirmed in the first spike.

## Milestones

0. **Setup.** `git init`; install JDK 21, Android SDK command-line tools and `adb`; turn on USB debugging on the phone (you do this once).
1. **Design.** The designer drafts the phone screens (pairing, home/status, settings) and the PC tray menu, pairing window and notification. You approve before building starts.
2. **Protocol spec and spike.** Write `PROTOCOL.md`. Check the three risky parts on the real devices: the foreground service type, the Wayland clipboard on KWin, and LAN→Tailscale fallback.
3. **Core, end to end.** Pairing, connection, and screenshots arriving with all three PC actions. The phone and PC sides are built in parallel against the spec.
4. **Features, in parallel by feature.** (a) Photos, recordings and the offline queue; (b) clipboard both ways; (c) connection options, Tailscale and mobile data; (d) share sheet and Quick Settings tile.
5. **Real-device testing on the S25+.** The phone locked for an hour; switching from Wi-Fi to mobile data to Tailscale; PC sleep and resume; reboot of both devices; battery use over a normal day. Then a designer review of the finished flows.
6. **Review and commit.** Review loop, then commit.

## Later (not v1)
- Sending files from PC → phone
- Notification mirroring
- Screen mirroring (bundling scrcpy)
- Play Store release
