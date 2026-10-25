# Play app-content declarations

Drafts for version 2.1.0 (6), checked 2026-10-25. Paste the paragraphs matching the live form prompts. Replace video/contact placeholders before submission.

## Photo and video permissions

Core function / `READ_MEDIA_IMAGES`:

Pneu automatically sends newly created screenshots and, when enabled, photos from the user's phone to their own paired computers. This is a core device-to-device transfer feature. With the user's media permission, Pneu observes MediaStore for newly published images while its foreground service is running, including while the app is in the background. Screenshots are enabled by default; photo sending is optional. The user chooses paired destinations and can pause automatic sending, disable categories or revoke permission. Content goes directly to paired devices over pinned mutual TLS; there is no Pneu upload server.

Core function / `READ_MEDIA_VIDEO`:

Pneu optionally sends new screen recordings to the user's paired computers. It observes newly published MediaStore video entries and sends eligible screen recordings when the user enables this category. It does not record the screen itself. The user can disable this category, pause automatic sending or revoke permission. Videos are transferred directly over pinned mutual TLS.

Why a system picker is insufficient:

The core automatic-send function detects future screenshots, photos and screen recordings as they are created, without requiring the user to pick each new item. A picker grants access to selected existing items and cannot provide this continuing discovery. Manual sharing remains available through Android's share sheet when broad media permission is declined; automatic media sending requires full access. Pneu does not use broad access merely to provide a custom picker or attach an occasional photo.

Select the closest actual transfer/synchronization use case in Console; if none fits, use Other with the explanation above. `READ_MEDIA_VISUAL_USER_SELECTED` supports Android's limited permission state; Pneu does not treat limited access as broad automatic-send access. Approval for broad media permissions remains Google's decision. [Policy and picker guidance](https://support.google.com/googleplay/android-developer/answer/16935362).

## Foreground service: connectedDevice

Permission: `android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE`.
Use case: Continuous data transfer to an external device over a network.
Applies to `PneuService` and the shorter-lived `ShareAdmissionService`.

Functionality:

After the user confirms pairing, Pneu maintains an authenticated connection to their computer over LAN or Tailscale. It sends and receives clipboard content and files, and sends enabled new phone media in the background. Its ongoing notification shows connection/transfer state and offers controls. Sharing a file to Pneu also starts a foreground service while it copies the granted source into its private outgoing queue, allowing the share activity to close without immediately losing the operation. These are network interactions with the user's paired external devices, not periodic cloud backup.

Impact of deferral or interruption:

Deferral delays pairing and delivery; background phone media created while observation is unavailable can be missed because Pneu resumes from a fresh baseline rather than uploading a backlog. Clipboard sends require the captured live session and may need to be sent again after disconnection. An interruption while copying a shared source can lose that not-yet-queued item; the user must share it again. Files already durably queued can retry. Keeping the service visible lets users understand the ongoing connection and control sending.

Video URL: OWNER TO RECORD AND HOST an accessible demo (for example, unlisted YouTube; no sign-in request or private pairing codes left visible).

Shot list for one short, continuous demonstration:

1. Show the free desktop app on a test computer displaying a fresh pairing QR. On Android, open Pneu, complete its setup, choose Add computer, scan and confirm. Show the connected device in Pneu.
2. Open the notification shade and show Pneu's ongoing connected-device notification. Return to Pneu and show the automatic-media controls and selected destination.
3. Create a harmless new screenshot with Pneu in the background. Show it arrive on that test computer; repeat with a photo and enabled screen recording to support both media declarations.
4. Copy harmless text and explicitly send it with Pneu's button, notification action or Quick Settings tile. Show the text arrive on the computer. Show a file sent from the computer arriving on the phone.
5. Share a harmless file from another Android app to Pneu, immediately return Home, and show admission/transfer status and delivery on the computer. This covers ShareAdmissionService as well as PneuService.
6. Pause automatic sending from Pneu or its notification; show the state change and that new automatic items stop being sent. Show Resume. Do not claim Pause disconnects the network or cancels every manual transfer.

The required connectedDevice video must show actual in-app pairing and external-device interaction; a settings-only recording is insufficient. [FGS declaration guidance](https://support.google.com/googleplay/android-developer/answer/13392821).

## Camera, startup and battery notes

- Camera: used only in the visible pairing scanner. No background camera service, face recognition or uploaded frames. ZXing core decodes QR codes entirely on device, with no network access or telemetry. Pneu collects and shares no data; see [data-safety.md](data-safety.md).
- Boot/package update: `RECEIVE_BOOT_COMPLETED` lets Pneu restore the paired-device connection after reboot/update. The service posts a foreground notification and stops when there is no pairing or active pairing attempt. Android/OEM restrictions can still prevent background operation; do not promise recovery after a force-stop.
- Battery: no `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` permission or direct exemption request. The battery row opens app settings so the user can select Unrestricted. Existing exemption/background-restriction status checks remain.
- Network: no location permission. LAN discovery and pinned TLS connections use network access; Tailscale must be installed and configured separately if used.

## Target audience, ads and content rating

- Target audience: select only **18 and over**. The app is intended for adult users managing their own devices, not children. Do not opt into Families or child-directed marketing. This audience choice does not predetermine the IARC age rating.
- Ads: **No**. No advertising SDK or ad placements.
- App category for the IARC questionnaire: utility / other non-game app, whichever exact option the form offers. Provide the owner's monitored contact email.
- Violence, sexual content/nudity, language, drugs/alcohol/tobacco, gambling/simulated gambling, horror, discrimination and criminal activity: **No** for app-provided content. Pneu supplies no such content or public catalog.
- Purchases, paid randomized items, money prizes, financial transactions: **No**.
- Public user-generated content, public profiles, stranger matching, social network or messaging between different users: **No**. Transfers are between the same user's explicitly paired devices. If a question broadly asks about any exchange of text, images or files, explain this private self-transfer instead of denying that transfers exist.
- Location disclosure or location sharing: **No**.
- Web browser / unrestricted web browsing: **No**. Opening documentation in the external browser is not an in-app browser.
- User-provided personal information shared with third parties: **No** for IARC's user-provided-personal-information question; Pneu has no profile or public sharing feature. No SDK telemetry or technical identifiers are collected or shared.

Submit the factual answers and use the rating IARC returns; do not select or promise an “18+” content rating. [IARC guidance](https://support.google.com/googleplay/android-developer/answer/9859655), [personal information rating question](https://support.google.com/googleplay/android-developer/answer/6161127).

## App access / reviewer instructions

Some functionality is restricted by device pairing, not a login or paid membership. Choose the Console option for restricted functionality and provide these instructions (no reusable passwords or pairing secrets):

Pneu requires a separate computer running the free Pneu desktop app. Download the Linux or macOS app from https://github.com/StantonMatt/pneu/releases and follow https://github.com/StantonMatt/pneu#readme. Connect both devices to the same LAN, or configure Tailscale on both. Open Add Device → Phone on the computer. In Android Pneu, choose Add computer, scan the generated code and confirm. Grant photo/video access and notifications to test automatic media. Use only test content. The invitation is generated locally and expires; no developer account, OTP or subscription is required.

If reviewers cannot use their own compatible computer, arrange an approved review setup before submission. Do not publish a permanent pairing token or expose an owner's real computer.
