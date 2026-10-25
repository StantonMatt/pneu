# Pneu privacy policy

Effective date: 2026-10-25. Pneu is developed by Matthew Stanton.

Pneu transfers clipboard content, photos and files between devices you pair. There are no Pneu accounts or developer-operated servers. Pneu collects no data and shares no data with the developer or third parties. It has no ads, analytics or telemetry, and sells no data.

## Data Pneu uses

- Clipboard content when you choose to send it through a button, Quick Settings tile, notification action or share sheet. Android-marked sensitive clipboard items are not sent.
- New screenshots, photos and screen recordings for the automatic-send categories you enable, after granting media access. Screenshots are enabled by default; photos and recordings are off by default. Automatic sending can work while the app is in the background. You can pause it or disable categories or individual computers.
- Files, images, text and links you share to Pneu, and content received from paired devices.
- Camera frames only while the QR pairing scanner is open, to read a computer's pairing code.
- Device names, pairing certificates, network addresses and local connection information needed to find and authenticate your computers over LAN or Tailscale.

Transfer content goes directly between your paired devices, encrypted with TLS 1.3 and pinned mutual authentication. Only those devices hold the keys needed to read it. The developer cannot access your transfers. Pneu does not upload clipboard content, photos or files to a developer server. Network discovery also uses local network information. If you use Tailscale, its separate app and service have their own privacy policy.

## QR scanning and Data safety

The Android app uses ZXing core, an Apache-2.0 pure Java library with no network access or telemetry. Camera frames and decoded QR contents are processed only on your phone.

[Google Play defines collection](https://support.google.com/googleplay/android-developer/answer/10787469#collection) as transmitting data off the device, but excludes end-to-end encrypted data readable only by its sender and recipient. Pneu transfers data only between your own paired devices over encrypted, mutually authenticated TLS 1.3 connections. No transfer content reaches the developer or a third party, so these transfers do not count as collection or sharing in Play's Data safety form. Local processing and storage are also outside collection.

## Storage and deletion

Pneu stores pairing keys, settings, transfer records, thumbnails and queued outgoing files locally to operate transfers and show their status. Sent payload snapshots are cleaned up; pending transfers can retain local copies until completed or cancelled. Clearing Android app storage or uninstalling removes its private data. Received files in Pictures/Pneu, Movies/Pneu or Download/Pneu, and copies already delivered to other devices, remain until you delete them there. Removing a pairing stops future transfers to that device; it does not erase delivered copies.

For privacy questions, [open a GitHub issue](https://github.com/StantonMatt/pneu/issues). Issues are public: do not include private content, pairing codes or keys.
