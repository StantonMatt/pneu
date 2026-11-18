# Data safety draft

For version 2.2.0 (7), using ZXing core 3.5.4 for on-device QR decoding. Sources checked 2026-10-25. These are proposed Console answers for the current implementation, not a Google approval.

## Scope and transfer content

[Google's Data safety guidance](https://support.google.com/googleplay/android-developer/answer/10787469#collection) defines collection as transmitting data off the device. It excludes end-to-end encrypted data readable only by its sender and recipient. Pneu transfers clipboard content, photos, recordings, files and pairing metadata between the user's own paired devices over direct, certificate-pinned mutual TLS 1.3 connections. No transfer content reaches the developer or a third party; only the paired devices hold the keys. These transfers therefore do not count as collection or sharing. Transfer content is encrypted in transit, including over LAN or Tailscale.

Local storage and on-device QR processing are also outside collection. ZXing core is pure Java, Apache-2.0 licensed, and has no network access or telemetry. Pneu has no analytics, advertising or telemetry SDK. User-directed opening/sharing to another app is an expected user action under Google's sharing exception. Tailscale is an optional separate app/service with its own policy.

## Collection and security questions

1. **Does your app collect or share any of the required user data types?** **No** — no data collected / no data shared.
2. **Is all collected user data encrypted in transit?** Not applicable after selecting No; Pneu's device-to-device transfer content is encrypted with pinned TLS 1.3.
3. **Which account-creation methods does the app support?** The app does not allow users to create an account. Pairing devices is not account creation.
4. **Can users request deletion of collected data?** Not applicable; no data is collected. Local deletion behavior is described in [PRIVACY.md](../../PRIVACY.md#storage-and-deletion).
5. **Account-deletion URL?** Not applicable; no Pneu accounts. Do not invent an account-deletion website.
6. **Independent security review / MASA badge?** No; no qualifying assessment has been performed.
7. **Other badges (payments, Families)?** None. This is not a payment app or a child-directed app.
8. **Privacy-policy URL?** https://github.com/StantonMatt/pneu/blob/main/PRIVACY.md — must be live before submission and accessible within the app.

## Data types and usage answers

Select no data types, collection/sharing purposes or retention claims. The No answer should skip those sections. No location, personal/contact/financial/health/calendar information, messages, photos/videos, audio, files/docs, installed-app inventory, browsing/search history, identifiers, diagnostics or app interactions are collected or shared.

## Before submitting

Compare the Console preview against this draft and the privacy policy: **No data collected / No data shared**. Recheck dependencies and their behavior after changes. Closed testing still requires this form.
