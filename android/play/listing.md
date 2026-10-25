# Play listing draft

English (United States). Copy only each field's text, without the Markdown labels.

## App name

Pneu

## Short description

Send clipboard, photos and files directly between your phone and computers.

## Full description

Pneu connects your Android phone to your own computers so you can send clipboard content, photos and files between them.

Requires the free Pneu desktop app on Linux and macOS, Windows coming. Download it and find setup instructions at https://github.com/StantonMatt/pneu. Android 14 or later is required.

Pair each computer by scanning its QR code and confirming in Pneu. Connections go directly over your local network or Tailscale, with TLS 1.3 encryption and pinned mutual authentication. There is no Pneu server or account.

Send your phone's clipboard with a button, Quick Settings tile, notification action or the Android share sheet. Phone clipboard sending always requires your action. Receive clipboard content and files from paired computers.

Automatically send new screenshots, photos and screen recordings to selected paired computers. Screenshots are on by default; photos and recordings are off until you enable them. Automatic sending needs photo/video access and runs in the background with a foreground-service notification. Pause sending or change the settings for each computer at any time.

Share files from other Android apps to Pneu. File transfers can queue for offline computers when configured. Clipboard sending requires a live connection. Received files are saved in Pictures/Pneu, Movies/Pneu or Download/Pneu.

Pneu collects no data and shares no data with the developer or third parties. It has no ads, analytics or telemetry. QR scanning uses ZXing core entirely on your phone. Transfers stay between your own paired devices and are encrypted in transit.

## Console fields

- App or game: App.
- Category suggestion: Tools.
- Price: Free.
- Developer name suggestion: Matthew Stanton (must match verified account details).
- Website: https://github.com/StantonMatt/pneu
- Support email: OWNER MUST PROVIDE a monitored public support address; GitHub issues cannot replace Play's required email field.
- Support URL / feedback URL where accepted: https://github.com/StantonMatt/pneu/issues
- Phone: optional; supply only if you want it public or Console requires it for your account.
- Privacy policy: https://github.com/StantonMatt/pneu/blob/main/PRIVACY.md (publish it first).

## First closed-test release

Release name: 2.1.0 (6) — first closed test

Paste into the release-notes field for en-US:

```text
First Pneu closed test.
Pair your phone with the free Pneu desktop app for Linux or macOS. Send clipboard content, screenshots, photos, recordings and files directly over LAN or Tailscale.
Please test pairing, background sending, notifications and file transfers. Report problems through GitHub issues without including private content or pairing codes.
```
