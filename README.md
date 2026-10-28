# <img src="docs/design/v2/assets/pneu.svg" width="64" height="64" alt="Pneu icon"> Pneu

Send files and share clipboard text between your phone and computers.

[Download Pneu](https://stantonmatt.github.io/pneu/).

Pronounced “new”, after pneumatic tubes.

## What it does

Pneu connects your Android phones and Linux, Mac or Windows computers directly. Pair each phone with several computers, and pair computers with each other.

- Send new phone screenshots to all connected computers automatically. Camera photos and screen recordings are optional.
- Send files to one or all connected devices, including the phone: use **Send with Pneu** in Dolphin, **Send Files…** in the desktop window, **Send Files To** in the Mac menu bar, **Share → Pneu** or dropping files on the window on Windows, `pneud send --to`, or **Share → Pneu** on Android.
- Share clipboard text automatically between computers and from computers to the phone. Phone → computers needs a tap because Android blocks background clipboard reading.
- Save received files, show notifications, copy supported images to the clipboard, or open images in a viewer (off by default).

It connects over the LAN or through Tailscale. Desktop files have a durable per-recipient queue; phone files also survive reconnects once queued. Clipboard sends require a live connection.

## How it works

The Kotlin/Compose Android app and Rust `pneud` daemons use direct, pinned mutual TLS 1.3 connections. The desktop has a Slint window plus a KDE or Windows tray, or Mac menu bar. Each pair has its own trust and settings; received content is not automatically relayed to other devices. LAN discovery uses mDNS for phones, with LAN/Tailscale address hints for computer links. Durable file queues and receipts deduplicate retries across reconnects.

```text
                  Android phone
                   /         \
                  /           \
        Linux computer ←────→ Mac computer

          pinned mutual TLS 1.3 · LAN / Tailscale
```

More phones and computers can join by pairing directly. See the [wire protocol](docs/PROTOCOL.md), including the negotiated [`mesh` capability](docs/PROTOCOL.md#mesh-capability-mesh), for pairing, discovery and delivery rules.

## Requirements

- **Linux:** Linux with KDE Plasma 6 on Wayland; built and tested on Kubuntu 26.04. Building needs Rust 1.93+ and Qt 6 development tools, including `qmake6` for the Slint UI (`QMAKE=qmake6`). See the [desktop README](desktop/README.md) for build and runtime details.
- **Mac:** macOS 15.4+; tested on Apple Silicon. See the [macOS guide](docs/macos.md) for build requirements and platform details.
- **Windows:** x64 Windows 10 version 2004 (build 19041) or later. Works from source and as a sideloaded MSIX; the Microsoft Store listing is coming. See the [Windows guide](docs/windows.md) for building, packaging and platform details.
- **Phone:** Android 14+ (`minSdk 34`); tested on a Samsung Galaxy S25+ and an emulator. Building needs JDK 21, Android SDK 37.2 and Build Tools 37.0.0; use the checked-in Gradle wrapper.
- **Optional:** Tailscale installed and connected on each device for access away from the LAN.

## Install

On Linux, from the repo root:

```sh
cd desktop
./install.sh
```

The installer builds `pneud`, installs it into `~/.local/bin`, installs icons and a desktop entry, and enables a systemd user service tied to the graphical session. It starts or restarts the daemon immediately if that session is active.

On the Mac, download [Pneu-macOS.zip from GitHub Releases](https://github.com/StantonMatt/pneu/releases/latest/download/Pneu-macOS.zip), unzip it and move **Pneu.app** to Applications. The app is ad-hoc signed, without notarization. If macOS blocks it, try opening it, then go to **System Settings → Privacy & Security → Open Anyway** and confirm.

To build and install from source, run from the repo root:

```sh
cd desktop
./install-macos.sh
```

This installs and starts `~/Applications/Pneu.app`, with a menu bar, notifications, native clipboard support and camera QR scanning. **Open at login** defaults to on at first installation/open. See the [macOS guide](docs/macos.md) for upgrades, permissions and uninstalling.

On Windows, build from source or sideload a signed MSIX following the [Windows guide](docs/windows.md). Start **Pneu** to open its window; closing it keeps the tray and transfers running.

On the phone side, from the repo root, with the Android SDK configured and `adb` on your PATH:

```sh
cd android
./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Connect the phone with USB debugging enabled and approve its debugging prompt before sideloading. Closed testing on Google Play is coming; [join the test](https://play.google.com/apps/testing/io.github.stantonmatt.pneu) once it is available. See the [Android README](android/README.md) for SDK setup and checks.

## First run

1. Open Pneu on the phone and complete **Set up Pneu**: allow notifications, grant photos and videos **Allow all**, and allow unrestricted battery use. Allow **Local network** if that row appears.
2. On Samsung, follow the **Never sleeping apps** guidance. On the S25+ with Android 16, **Unrestricted** satisfies this requirement; One UI hides exempt apps from the Never sleeping picker.
3. Open the desktop window from the tray/menu bar or with `pneud window`, then choose **Add Device… → Phone**. On the phone, choose **Add device**, allow the camera, scan the computer's QR code and confirm pairing. Repeat for each computer.
4. To pair computers, choose **Add Device… → Computer → Show Pairing Code** on one. On the other, use **Paste Link** with its `pneu://pair` link, or **Scan QR Code** on a Mac with a camera. Confirm pairing when prompted. Pair every link you want to use directly.
5. Optionally add the **Pneu** and **Send clipboard** Quick Settings tiles for Pause/Resume and sending the phone clipboard.

## Settings

Open **Settings** in the desktop window or Android app. Select a device to change its pair switches; those switches sync to the other side, with pending changes applied when it reconnects. Defaults below are for a new installation and pairing; upgrades preserve saved choices.

| Phone setting | Default |
| --- | --- |
| Screenshots | On |
| Camera photos | Off |
| Screen recordings | Off |
| Show in share sheet | On |
| Choose devices each time | Off |
| Send button in notification | On |
| Direct on home Wi-Fi | On |
| Tailscale when away | On |
| Use mobile data | On |
| Send originals on mobile data | Off |
| Queue for offline devices | Off |
| Quick Settings tiles | On |

| Desktop setting | Default |
| --- | --- |
| Share clipboard | Automatically |
| Include images | Off |
| Choose devices each time | Off |
| Queue for offline devices | Off |
| Save to folder | On (`~/Pictures/Pneu` on Linux, `~/Downloads/Pneu` on Mac, `Downloads\Pneu` on Windows) |
| Show notifications | On |
| Copy images to clipboard | On |
| Open in image viewer / Open in Preview | Off |
| Open at login (Mac / packaged Windows) | On at first installation/open on Mac; enabled in the Windows package |

| Per-device switch | Default |
| --- | --- |
| Send clipboard | On |
| Receive clipboard | On |
| Automatic sending (phone) / Receive automatic photos (computer) | On |

The automatic-media switch applies only to phone → computer pairs and covers the enabled screenshot, photo and recording kinds. Computer pairs have the two clipboard switches. Turning off a pair's clipboard direction also suppresses copying received images to that clipboard; ordinary file saving remains available.

At least one of Wi-Fi or Tailscale must stay enabled on the phone. With originals off on metered networks, automatic images are compressed and recordings wait; shared files stay unchanged. Pause suspends automatic capture; manual shares still work. Clipboard images require **Include images** on the sending desktop and image support on the receiver; Android can receive them too.

By default, file sends target connected eligible devices. **Choose devices each time** opens a recipient picker. **Queue for offline devices** includes offline paired devices for automatic phone media and desktop sends; Android manual shares use offline devices when none are connected and queueing is enabled. Explicit file targets can queue while offline, including `pneud send --to DEVICE_ID FILE`. Clipboard sends stay connected-only regardless of the queue setting. Linux configuration lives in `~/.config/pneu/config.toml`; Mac and Windows paths are in the [macOS guide](docs/macos.md#paths-and-overrides) and [Windows guide](docs/windows.md#paths-and-overrides).

## Development

- [docs/](docs/): [plan and implementation status](docs/PLAN.md), [protocol](docs/PROTOCOL.md), [design](docs/design/DESIGN.md) and [shared protocol vectors](docs/protocol-vectors/v1.json).
- [desktop/](desktop/): Rust crates `pneu-proto` (wire codec), `pneud` (daemon and UI) and `pneu-sim` (phone transport simulator). See the [desktop README](desktop/README.md) for CLI commands and configuration.
- [android/](android/): Kotlin/Compose app, Gradle wrapper and device-test tools. See the [Android README](android/README.md#checks) for the full checks and suite selection.

From `desktop/` on Linux, run (Mac checks are in the [macOS guide](docs/macos.md#native-verification-and-private-acceptance), Windows checks in the [Windows guide](docs/windows.md#native-verification)):

```sh
QMAKE=qmake6 cargo test
```

From `android/`, run JVM unit tests with:

```sh
./gradlew testDebugUnitTest
```

Device suites use [android/tools/run_device_tests.py](android/tools/run_device_tests.py); follow the Android README to build both APKs, start the emulator and run them. On the owner's machine, prefix builds and full test suites with `heavy`, including `heavy python3 tools/run_device_tests.py` from `android/`. Rust and Kotlin tests consume the same protocol vectors.

## Status and limitations

Personal project, version **2.2.0**. Physical acceptance for 2.1.0 was completed with a Kubuntu 26.04 PC, a MacBook Air (macOS 26.6, arm64) and a Galaxy S25+, all three paired over Tailscale. This does not establish every Wi-Fi/mobile transition, OEM background condition or battery-use scenario. The Android app can be sideloaded; Google Play closed testing is coming. The Mac app is ad-hoc signed, without Developer ID signing or notarization. Windows works from source and as a sideloaded MSIX; Store submission and physical-hardware acceptance remain pending.

The phone supports up to 16 paired computers. Each link is direct: pairing a phone with two computers does not pair those computers with each other. Clipboard is connected-only, and phone clipboard sending requires a tap. Supported clipboard image formats and desktop actions have limits; a crash in the short window before a desktop file receipt is committed can leave a duplicate visible file after retry. Read the accepted behaviours in the [protocol's transfer section](docs/PROTOCOL.md#transfer-and-delivery-state-machines), the [desktop README](desktop/README.md) and the [Android README](android/README.md#accepted-behaviours).

## License

[MIT](LICENSE). The bundled Material Symbols assets are covered by their own [license](android/MATERIAL_SYMBOLS_LICENSE).
