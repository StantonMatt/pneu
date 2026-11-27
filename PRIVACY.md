# Pneu privacy policy

Effective date: 2026-10-30. Pneu is developed by Matthew Stanton.

Pneu transfers clipboard content, photos, files and optionally new Android SMS between devices you pair. This policy covers Android, Linux, macOS and Windows. There are no Pneu accounts or developer-operated servers. Pneu processes and stores data only on your devices; the developer collects none. It has no ads, analytics or telemetry, and sells no data.

## Data Pneu uses

- On Android, clipboard content when you choose to send it through a button, Quick Settings tile, notification action or share sheet. Android-marked sensitive clipboard items are not sent.
- Desktop apps share clipboard content automatically with paired, connected devices by default. Settings offers Automatically, Manually or Off; each device's Send clipboard and Receive clipboard switches control directions. Include images is on by default. Supported exclusion and sensitivity markers are skipped, including on Windows, but Pneu cannot detect every sensitive item.
- On Android, new screenshots, photos and screen recordings for the automatic-send categories you enable, after granting media access. Screenshots are enabled by default; photos and recordings are off by default. Automatic sending can work while the app is in the background. You can pause it or disable categories or individual computers.
- On Android, if you turn on SMS messages (off by default): the sender and text of each new SMS, sent only to the paired computers you allow. SMS waiting for an offline computer stay in memory for at most two minutes. Pneu does not save SMS on either device. Computers show notifications with detected codes masked, and can copy one-time codes with clipboard-history exclusion markers and clear them after a minute if unchanged. On macOS, clearing is best-effort because checking for changes and clearing are separate system operations. SMS codes are never shared back to other devices.
- Files, images, text and links you share to Pneu, and content received from paired devices.
- On Android and macOS, camera frames only while the QR pairing scanner is open. Windows does not use the camera.
- Device names, pairing certificates, network addresses and local connection information needed to find and authenticate your devices over LAN or Tailscale.

Transfer content goes directly between your paired devices, encrypted with TLS 1.3 and pinned mutual authentication. The developer cannot access your transfers. Local discovery (mDNS) is unencrypted: it exposes device/service identifiers, addresses, a port and a public certificate fingerprint to the local network, never transfer content or private keys. The Windows package adds inbound firewall rules for TCP 47821 and UDP 5353 on all network profiles. If you use Tailscale, its separate app and service have their own privacy policy.

## Android QR scanning and Google Play Data safety

The Android app uses ZXing core, an Apache-2.0 pure Java library with no network access or telemetry. Camera frames and decoded QR contents are processed only on your phone.

[Google Play defines collection](https://support.google.com/googleplay/android-developer/answer/10787469#collection) as transmitting data off the device, but excludes end-to-end encrypted data readable only by its sender and recipient. Pneu transfers data only between your own paired devices over encrypted, mutually authenticated TLS 1.3 connections. No transfer content reaches the developer or a third party, so these transfers do not count as collection or sharing in Play's Data safety form. Local processing and storage are also outside collection.

## Storage and deletion

Pneu stores pairing keys, settings, transfer records, thumbnails and queued outgoing files locally. Sent payload snapshots are cleaned up; pending transfers can retain local copies until completed or cancelled. Private desktop state uses your Linux data/config/cache folders, macOS Library/Application Support and Library/Caches, or Windows Local AppData (LocalCache for the Store package).

Received files default to Downloads/Pneu on Windows and macOS, Pictures/Pneu on Linux. Change the folder in Settings. Android saves to Pictures/Pneu, Movies/Pneu or Download/Pneu.

Clearing Android app storage or uninstalling removes its private data. Uninstalling the Windows package normally removes private package data. macOS keeps app data unless you uninstall with `--purge`; Linux app data needs separate deletion. Received files and copies delivered to other devices remain until you delete them there. Removing a pairing stops future transfers; it does not erase delivered copies.

## Desktop background operation and notifications

Pneu runs in the tray or menu bar; closing the window keeps it running. Choose Quit Pneu to stop it. macOS and packaged Windows start at sign-in by default after first launch; turn off Open at login in Settings or use system startup settings (Windows Settings → Apps → Startup). Linux's installer enables a user service; disable it with `systemctl --user disable --now pneud.service`.

Notifications may show file names, received text previews and SMS. SMS notifications remain enabled by the per-phone Receive SMS switch; Show notifications controls received files and clipboard previews. Turning off Show notifications in Settings stops those routine notifications, but alerts about failed transfers or saves can still name the file; use your system's notification settings to hide all of them.

For privacy questions, [open a GitHub issue](https://github.com/StantonMatt/pneu/issues). Issues are public: do not include private content, pairing codes or keys.
