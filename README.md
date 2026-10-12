# <img src="docs/design/assets/pneu.svg" width="64" height="64" alt="Pneu icon"> Pneu

Send phone screenshots and files to your Linux PC, and sync clipboard text both ways.

Pronounced “new”, after pneumatic tubes.

## What it does

Pneu aims to put a new phone screenshot on your PC in about a second while connected. You can also enable automatic camera photos and screen recordings, or use **Share → Pneu / Send to PC** for any file, text or link. Clipboard text works both ways: PC → phone is automatic; phone → PC needs a tap because Android blocks background clipboard reading.

It connects directly over home Wi-Fi or through Tailscale. Phone settings and PC tray toggles let you choose which features run. On arrival, the PC can:

- Save files to `~/Pictures/Pneu` or a folder you choose.
- Show a notification with a preview and an action to open the file.
- Copy supported images to the clipboard.
- Open images in the default viewer (off by default).

## How it works

The Kotlin/Compose Android app connects to `pneud`, a Rust daemon with a KDE tray UI. QR pairing establishes certificate pins; one long-lived, mutually authenticated TLS 1.3 connection carries transfers in both directions. LAN discovery uses mDNS, with Tailscale addresses as a fallback. A durable phone queue and transfer receipts deduplicate retries across reconnects.

```text
Android app  ←── pinned mutual TLS 1.3 ──→  pneud + KDE tray
                    LAN / Tailscale
```

See the [wire protocol](docs/PROTOCOL.md) for pairing, discovery and delivery rules.

## Requirements

- **PC:** Linux with KDE Plasma 6 on Wayland; built and tested on Kubuntu 26.04. Building needs Rust 1.93+ and Qt 6 development tools, including `qmake6` for the Slint UI (`QMAKE=qmake6`). See the [desktop README](desktop/README.md) for build and runtime details.
- **Phone:** Android 14+ (`minSdk 34`). Tested on an emulator; the target device is a Samsung Galaxy S25+, with physical acceptance still pending. Building needs JDK 21, Android SDK 37.2 and Build Tools 37.0.0; use the checked-in Gradle wrapper.
- **Optional:** Tailscale installed and connected on both devices for access away from the LAN.

## Install

On the PC, from the repo root:

```sh
cd desktop
./install.sh
```

The installer builds `pneud`, installs it into `~/.local/bin`, installs icons and a desktop entry, and enables a systemd user service tied to the graphical session. It starts or restarts the daemon immediately if that session is active.

**Installer note:** [desktop/install.sh](desktop/install.sh) currently hard-codes `/home/mjstanton/.local/bin/heavy` for the release build. Other users need to provide that wrapper or adapt the build invocation before using the installer.

On the phone side, from the repo root, with the Android SDK configured and `adb` on your PATH:

```sh
cd android
./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Connect the phone with USB debugging enabled and approve its debugging prompt before sideloading. There is no Play Store release. See the [Android README](android/README.md) for SDK setup and checks.

## First run

1. Open Pneu on the phone and complete **Set up Pneu**: allow notifications, grant photos and videos **Allow all**, and allow unrestricted battery use. Allow **Local network** if that row appears.
2. On Samsung, add Pneu to **Never sleeping apps**. The usual path is **Settings → Battery → Background usage limits → Never sleeping apps** (older One UI versions put Battery under **Battery and device care**). Setup's **Open** button attempts to take you there; the exact S25+ path still needs physical verification.
3. In the PC tray, choose **Pair Phone…**. On the phone, continue to **Pair with PC**, allow the camera, scan the PC's QR code and confirm any pairing prompt. The PC also opens the pairing window automatically when unpaired.
4. Optionally add the **Pneu** and **Send clipboard** Quick Settings tiles for Pause/Resume and sending the phone clipboard.

## Settings

Defaults below match the current implementation. Phone settings control sending and receiving; PC tray settings control arrival actions and outgoing clipboard sync.

| Phone toggle | Default |
| --- | --- |
| Screenshots | On |
| Camera photos | Off |
| Screen recordings | Off |
| Direct on home Wi-Fi | On |
| Tailscale when away | On |
| Use mobile data | On |
| Send originals on mobile data | Off |
| Queue while PC is unreachable | On |
| Send button in notification | On |
| Receive clipboard from PC | On |
| Show in share sheet | On |
| Quick Settings tiles | On |

| PC tray toggle | Default |
| --- | --- |
| Save to Folder | On (`~/Pictures/Pneu`) |
| Show Notifications | On |
| Copy Images to Clipboard | On |
| Open in Image Viewer | Off |
| Send Clipboard to Phone | On |
| Include Images | Off |

At least one of Wi-Fi or Tailscale must stay enabled. With originals off on metered networks, automatic images are compressed and recordings wait; shared files stay unchanged. Pause suspends automatic capture; manual shares still work. The Android app currently receives clipboard text only, so the PC's **Include Images** requires a peer that advertises image support. PC configuration lives in `~/.config/pneu/config.toml`.

## Development

- [docs/](docs/): [scope](docs/PLAN.md), [protocol](docs/PROTOCOL.md), [design](docs/design/DESIGN.md) and [shared protocol vectors](docs/protocol-vectors/v1.json).
- [desktop/](desktop/): Rust crates `pneu-proto` (wire codec), `pneud` (daemon and UI) and `pneu-sim` (phone transport simulator). See the [desktop README](desktop/README.md) for CLI commands and configuration.
- [android/](android/): Kotlin/Compose app, Gradle wrapper and device-test tools. See the [Android README](android/README.md#checks) for the full checks and suite selection.

From `desktop/`, run:

```sh
QMAKE=qmake6 cargo test
```

From `android/`, run JVM unit tests with:

```sh
./gradlew testDebugUnitTest
```

Device suites use [android/tools/run_device_tests.py](android/tools/run_device_tests.py); follow the Android README to build both APKs, start the emulator and run them. On the owner's machine, prefix builds and full test suites with `heavy`, including `heavy python3 tools/run_device_tests.py` from `android/`. Rust and Kotlin tests consume the same protocol vectors.

## Status and limitations

Personal project, v0.1 (`0.1.0`). One paired phone per PC; pairing a replacement forgets the old pairing. Linux/KDE only, sideloaded Android app, no Play Store release. Screenshot latency, Samsung background behaviour and real Wi-Fi/mobile/Tailscale transitions still need physical-device acceptance.

Android 14 supports text shares, but URI/file shares require Android 15+ for sender-grant verification. Supported clipboard image formats and desktop actions have limits; a crash in the short window before a file receipt is committed can leave a duplicate visible file after retry. Read the accepted behaviours in the [protocol's transfer section](docs/PROTOCOL.md#transfer-and-delivery-state-machines) and the [Android README](android/README.md#accepted-behaviours).

## License

No license file yet. The [Material Symbols license](android/MATERIAL_SYMBOLS_LICENSE) covers those bundled assets.
