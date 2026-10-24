# Pneu Android

Kotlin/Compose app, version 2.1.0, API 34+, application ID `io.github.stantonmatt.pneu`.
JDK 21, SDK 37.2, AGP 9.4.1 and the checked-in Gradle wrapper are required.
Run commands below from `android/` with `ANDROID_HOME=$HOME/Android/Sdk`.

```sh
/home/mjstanton/.local/bin/heavy ./gradlew assembleDebug testDebugUnitTest lintDebug assembleDebugAndroidTest
$ANDROID_HOME/platform-tools/adb -s emulator-5554 install -r app/build/outputs/apk/debug/app-debug.apk
```

## Architecture

- `protocol/`: bounded v1 framing, strict JSON, UTF-8 limits, binary chunks and
  invitation parsing. The `mesh` capability adds per-pair settings and device-wide
  clipboard ordering without changing the protocol version. Gradle copies shared
  vectors from `../docs/protocol-vectors/v1.json`; optional
  `-PprotocolVectorsPath=/absolute/path/v1.json` selects another read-only fixture.
- `connection/TlsIdentity`: Android Keystore P-256 identity, exact DER pin
  verification and TLS 1.3 mutual authentication. Each dial uses a fresh TLS
  context without session resumption. DNS, sequential TCP attempts and TLS share
  a 15-second endpoint budget; individual DNS/TCP attempts take at most five
  seconds and TLS at most ten seconds.
- `connection/Connections` runs independent per-computer `ConnectionManager`
  sessions, with LAN/mDNS/QR/Tailscale candidates, network-change retry, jittered
  backoff, liveness timers and frame-atomic writes. Files and clipboard have
  separate delivery lanes. Trust, capabilities and pending work belong to each
  computer. Unpairing one preserves the others.
- Pairing attempts are app-scoped and in memory: Activity/service recreation,
  lost PAIR_RESULT and invitation expiry do not abandon an admitted attempt;
  process death requires a re-scan. Frozen target generations reject stale
  outcomes; cancellation is local, without UNPAIR. Same-identity re-scans retain
  delivery authority. A changed certificate can repair only a lost binding;
  otherwise the user must unpair it first.
- `PneuService`: foreground data-sync service, MediaStore observation, transport
  sessions and notification controls. Boot/update requests foreground startup
  with a finite deadline; an unpaired service stops itself. Notification/tile
  clipboard actions open a focused activity because Android blocks background
  clipboard reads.
- `data/`: DataStore preferences, per-computer trust/settings and clipboard
  clocks; Room schema 11 stores outgoing artifacts and per-recipient deliveries,
  receipts, automatic-media admission journals and received-file records.
  Settings synchronize both clipboard directions and automatic phone media for
  each pair. See the [main README](../README.md#settings) for defaults.
- `media/ScreenshotObserver`: publication-complete screenshots/photos/recordings
  since the observation baseline. It tracks added/modified image and video
  generations, pending rows, per-kind floors and catalog namespaces. Resume,
  observer restart and permission restoration baseline without a backlog;
  unadmitted media in those gaps may be missed. Automatic discovery freezes its
  recipients and ordering stamp before provider access.
- `features/Staging`: immutable private bytes/hash/fsync before queue insertion.
  A separate four-worker pool bounds provider open/read/close work; ten seconds
  without progress fails the copy. Outgoing staging has a 512 MiB quota, 16 MiB
  free-space reserve and 2048 pending-operation cap. With originals off on
  metered networks, automatic images are JPEG quality 85, long edge ≤2560,
  with EXIF orientation applied; recordings wait. Shared files stay unchanged.
- `features/ClipboardDelivery`: incoming text and image effects share one
  device-wide mesh ordering register and recent echo history. Text OS denial
  NACKs; optional image-copy failure does not undo a saved-file receipt.
  Stale/replayed operations do not reapply. Outgoing clipboard text,
  shared text/links and clipboard images capture live sessions and never migrate
  across a disconnect. A new text supersedes older unsent text for that recipient.
- `features/ManualEntries`: focused clipboard activity, SEND/SEND_MULTIPLE share
  target and Quick Settings tiles. Text/link shares use CLIPBOARD `origin=share`;
  other shares use `file`. Manual sends work while paused. Default file shares
  target connected computers; with offline queuing on and none connected, they
  capture eligible offline computers. Explicitly selected offline recipients can
  queue regardless of that default. Android-marked
  sensitive clips are rejected. External `content://` shares use bounded provider
  access, including chooser-relayed grants and Pneu's own media permission, on
  Android 14+. Own-provider/private/path-traversal URIs are refused. No Direct
  Share publication or persistable source grants are used. Recipient capture
  remains fixed throughout a multi-item share; external callers cannot select it.
- `features/ReceivedFiles`: streams files from paired computers into MediaStore,
  verifies size/hash, records durable receipts and publishes completed rows.
  Images go to `Pictures/Pneu`, videos to `Movies/Pneu`, other files to
  `Download/Pneu`. Received files can be opened from Recent or notifications and
  are excluded from automatic outbound capture. Clipboard images can also be
  received and copied subject to the pair's receive switch and current policy.
- Recent projects per-recipient progress and outcomes, with bounded thumbnails.
  Terminal outgoing history is limited to 50 operations; incoming Recent also
  shows up to 50 files. Completed automatic sends can open an existing original;
  completed manual sends cannot reopen their source URI. Received MediaStore
  files remain available independently of Recent.
- Compose uses the fixed indigo palette, true-black dark mode and Link component.
  Home lists computers and per-device status; device pages expose synchronized
  switches and send actions. **Choose devices each time** opens the send picker.
  Disabling share-sheet visibility removes the target; disabling Quick Settings
  leaves installed tiles unavailable. Settings writes keep saved state visible
  on failure and offer retry. Samsung setup treats a live battery-optimization
  exemption as satisfying **Never sleeping apps**; background-kill evidence is
  still shown when present.

## Pairing and test peer

On the computer, use **Add Device… → Phone**. In the phone's Devices card,
choose **Add computer**, scan the QR and confirm. Repeat to add more computers;
the phone supports up to 16, including lost pairings. The phone can send to one
or all eligible computers and receive files and clipboard from each.

For emulator testing, scan a computer invitation or start a fresh deep link:

```sh
$ANDROID_HOME/platform-tools/adb -s emulator-5554 shell am start -W \
  -a android.intent.action.VIEW \
  -d 'pneu://pair?v=1&name=Test%20PC&fp=<DER_SHA256>&addr=10.0.2.2%3A47839&token=<TOKEN>&exp=<EXPIRY_MS>' \
  io.github.stantonmatt.pneu
```

`10.0.2.2` is the emulator host; physical phones use PC LAN/Tailscale addresses.
Every deep link/QR requires explicit confirmation in Pneu. An identical
certificate/device-ID re-scan is idempotent. A new certificate for a known device
ID repairs only a lost pairing; otherwise Pneu asks the user to unpair it first.
A known certificate claiming a different device ID, or the phone's own device
ID, is rejected. The capacity check uses the authenticated identity, so repairs
reuse their slot. API 37 requires local-network permission. Tokens, keys and
user content are not logged; there is no production authentication bypass.

`tools/protocol_peer.py` is an independent Python/OpenSSL test peer. Supply
`--cert`, `--key`, `--client-ca` (the phone's exported public leaf), `--port`, and
`--events`. It requires real TLS 1.3 client authentication and validates streamed
metadata/size/hash. `--drop-first-pair-result` plus the fixture witness expiry tests recovery past expiry. It
accepts synthetic tokens, provides synthetic incoming clipboard fixtures, and
lacks durable receipts; never use it as the real PC daemon. Type `quit` to stop.

## Checks

Build both APKs and run the JVM/lint gate with the first command above. Host-only
runner checks need no emulator. Every instrumentation execution must report a
positive test count; method filters must execute exactly one test. Count failures
are reported while the runner continues with later cases and suites:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -v
```

Then start
an API 37 `pneu-test` emulator and run the **whole** device runner through `heavy`:

```sh
export ANDROID_HOME=$HOME/Android/Sdk
export ANDROID_SERIAL=emulator-5580
case "$ANDROID_SERIAL" in emulator-*) ;; *) exit 1;; esac
$ANDROID_HOME/emulator/emulator -avd pneu-test -port 5580 -memory 2048 -no-window -no-audio -no-snapshot-save -gpu swiftshader_indirect -no-boot-anim &
emulator_pid=$!
timeout 180 "$ANDROID_HOME/platform-tools/adb" -s "$ANDROID_SERIAL" wait-for-device
"$ANDROID_HOME/platform-tools/adb" -s "$ANDROID_SERIAL" emu avd name # Must say pneu-test
/home/mjstanton/.local/bin/heavy python3 tools/run_device_tests.py
# Once the runner completes:
case "$ANDROID_SERIAL" in emulator-*) ;; *) exit 1;; esac
$ANDROID_HOME/platform-tools/adb -s "$ANDROID_SERIAL" emu kill
wait "$emulator_pid"
```

Wait for the emulator to finish booting before running the tests. The canonical
runner checks the `emulator-*` serial and `pneu-test` AVD before any fixture work,
and checks the explicit serial and `ANDROID_SERIAL` before every adb command.
No suite arguments runs all suites. `--cases` selects comma-separated named cases
within the selected suites without changing the full inventory. Select one or
more suites by name, for example:

```sh
/home/mjstanton/.local/bin/heavy python3 tools/run_device_tests.py local cold
/home/mjstanton/.local/bin/heavy python3 tools/run_device_tests.py cold --cases cold-text
/home/mjstanton/.local/bin/heavy python3 tools/run_device_tests.py policy upload daemon \
  --scratch "$HOME/.cache/agent-scratch/pneu/device-check"
```

| Suite | Coverage |
|---|---|
| `local` | Shared fixture-reset/transport invariants; Main/Core onboarding/service/queue; complete Full/Second/Eighth/Advisory classes; pass 4–7 storage/UI, advisory storage, pass 10/12/14–16/20–22 classes; offline queue policy and all EXIF preview/thumbnail transforms; original Recent opening; first-load failure; pairing-loss tile/Home recovery; immediate Home/task removal/rotation shares; host-prepared permanent permission denial. |
| `cold` | Fresh-install deep-link confirmation witnessed before instrumentation; force-stopped single-file, multiple-file and text SENDs with queued Room rows witnessed before verification. |
| `connection` | Deep-link rejection/confirmation retry plus complete pass 4/5/6 real mTLS classes: DNS fallback, staged snapshots, partial-file NACK, receipt/storage faults, interrupted/latest text and binding-reset recovery. |
| `faults` | All 24 ConnectionFault scenarios with their required peer flags: completion/fallback storage recovery, ineligible-selection backoff, both 200 MiB transfers, capabilities, stalled writes, absolute deadlines, source revocation, unauthorized and generic TLS failures. |
| `tls` | Keystore mutual TLS, wrong-pin/plaintext address fallback, TLS 1.2 rejection and exact pinned leaf with invalid TLS 1.3 CertificateVerify. |
| `features` | Complete Features class: pairing recovery after expiry, silent TLS candidates, share/clipboard (including chooser-granted bytes verified at the authenticated peer), SystemUI tiles, pause/observation boundaries, mid-copy policy invalidation/rescan delivery and latest-text delivery. |
| `policy` | Complete Policy class: actual metered route, mobile encoding, optimized recovery, recordings, mid-copy policy invalidation/rescan delivery, multiple shares, clipboard sensitivity/echo/receipts and staging capacity. |
| `upload` | Hash-verified 160 MiB upload lasting over 180 seconds with no PC traffic before ACK. |
| `daemon` | Builds this worktree's real `pneud`, pairs with private XDG state, unpairs while phone is offline, and verifies exactly three unauthorized frames followed by Pairing lost. |

Features waits report their step names in instrumentation output and timeout
assertions. Chooser sharing follows its activity operation to the exact Room row,
ACK and peer FILE bytes/hash. Its sender stays alive for the real system chooser's
result, and the package-filtered single target auto-launches through that chooser,
without driving its animated sheet. Diagnostics include fixture launch status,
provider-open counts and fixed Pneu rejection codes without source URIs or content.
The permission-denial case saves failure screenshots and UI hierarchy under
`permission-denial-screens/`, including when its instrumentation assertions fail.

The outer scheduler admits Python before any fixture, invitation, listener or
control-socket deadline starts. There are no nested `heavy` calls. APKs are
reinstalled before instrumentation; device and host commands have finite timeouts.
Only explicit automation interference is retried once; assertion failures and app
crashes fail the suite. Failed classes/scenarios clean up their fixtures and the runner continues with
the next independent case in the same suite, then every selected suite. Dependent
cold-share verification runs only if its preparation succeeded. It prints a
pass/fail summary, writes
`results.json`, and exits non-zero if any suite failed. Output must contain JUnit
`OK`, since adb can exit zero on failure. `results.json`, screenshots, cold-start witnesses and peer events remain
in the scratch directory (default: a timestamped directory under
`~/.cache/agent-scratch/pneu/`). Temporary peers/listeners, private keys, daemon
state and app/test processes are cleaned on exit. The runner leaves the emulator
running so the caller can stop it with `adb emu kill`.

Every peer receives a fresh exported public phone leaf with
`prepareIdentity=true`; tests retain that identity using `identityPrepared=true`.
The peer witnesses real TLS 1.3 client authentication and file size/hash, receipt,
partial-file and session counts. Never reuse a peer after identity rotation.
Features has two silent TLS candidates; the 15-second UI feedback deadline must
not reset their ten-second handshake deadlines. Its invitation witness proves the
first PAIR precedes expiry and recovery follows it. Clipboard tests wait for
correlated `PHONE_ACK` before checking unchanged clipboard/receipt state. These
witness sockets are test tooling only.

The emulator's default route must be metered for Policy; the runner does not
change routing. The daemon build defaults to
`~/.cache/agent-scratch/pneu/android10-target`; `--daemon` selects its binary and
build target directory. `--serial` selects the `pneu-test` emulator (default
`ANDROID_SERIAL`, falling back to `emulator-5554`). The suites change app state,
and `cold` clears app data. Physical devices and other AVDs are refused. Cold
offline text must complete with `no_recipient` and no queued delivery; the host
witnesses that rejection before launching verification instrumentation.
Each device test resets preferences, queue, identity and injected faults through
`DeviceFixture` in `@Before`. Prepared mTLS fixtures preserve the exported identity;
host-witnessed cold verification phases validate their recorded state without
resetting it. Test permission grants/denials remain fixture-specific. Transport
changes enable the destination before disabling the source; disabling the last
transport is rejected atomically.
Repeated executions of identical classes/scenarios from historical review runners
are consolidated, retaining every distinct fixture configuration and phased test.

## Physical acceptance and interpretation

- Physical acceptance for 2.1.0 was completed on a Galaxy S25+, a Kubuntu 26.04 PC
  and a MacBook Air (macOS 26.6, arm64), with all three paired over Tailscale.
  This does not establish every sleep/Doze/reboot condition, provider/HEIF path,
  Wi-Fi/cellular transition or full-day battery-use scenario.
- The emulator has Wi-Fi enabled but its active default network is metered;
  mobile policy is tested on that observed cost, not a simulated policy flag.
  PolicyDeviceTest also checks Android capability fixtures for Wi-Fi, cellular,
  VPN over each, metered Wi-Fi and missing capabilities, plus the active network's
  actual NetworkCapabilities, without changing routing.
- Normative protocol wins over obsolete design `shared`/`text` kinds and save
  errors described as ACKs: non-text shares are `file`, text is CLIPBOARD, failed
  saves are NACKs. The phone advertises `files`, `clipboard_text`, `clipboard_image` and `mesh`.
- Incoming clipboard messages carry no sensitivity metadata. Android-marked
  sensitive outgoing clips are rejected with `Sensitive clipboard. Not sent.`.
- Clipboard is connected-only, including shared text and images. Disconnect
  supersedes deliveries for that session; resend after reconnecting. A new text
  supersedes older unsent text. File deliveries keep their independent queue.
- Outgoing Recent is bounded to 50 terminal operations with metadata and small
  thumbnails; sent payload snapshots are cleaned up. Received files remain in
  MediaStore. Missing thumbnails use type icons.
  Pending previews use staged bytes; completed automatic items open the existing
  original, while completed shares cannot reopen the sender URI.
  Persistent dedupe records are never evicted to reclaim capacity.
- Battery-exemption and custom-trust-manager lint warnings are
  intentional for this sideloaded pinned device connection. Unused approved
  design assets/strings remain for consistent UI copy; dependency/style/API
  suggestions are informational. Versions remain pinned for reproducibility.

## Device test setup

Run the required
full gate with `assembleDebugAndroidTest` in addition to the first command.
`AdvisoryDeviceTest` needs no peer. `ConnectionFaultDeviceTest` methods require
a fresh peer configured with the flag described by each method (no-files,
reject-file-kind, stall-file-after-begin, hello-drip, pair-drip, hello-delay 3,
ready-close). Supply pcPin/pcPort; pin fallback also takes rejectPort.
`TlsDeviceTest.rejectedHandshake` requires pcPin, base64 public DER pcCert, and
rejectPort. Run it against TLS1.2 and `tools/invalid_certificate_verify_peer.py`.
The latter implements test-only TLS1.3 records to send the exact pinned leaf with
a signature from a different key; platform rejection happens after pin acceptance.
Unconfigured optional TLS tests are explicitly skipped by JUnit, never silently
reported as successful.

## Pairing loss

Three consecutive `ERROR unauthorized` responses from the authenticated pinned PC
mark the pairing as lost. The PC checks fingerprint admission after TLS, before
reading HELLO. Generic network failures and all TLS alerts preserve trust; a TLS
alert can describe a cryptographic fault and is not a revocation signal.

## Accepted behaviours

- Resume skips everything already inserted into MediaStore at its baseline,
  including pending items published later. New insertions after resume are eligible;
  an earlier recording first inserted after resume can therefore send. If Pneu
  was not running, unseen media from just before a pause can be skipped too.
  Observer restart, permission restoration and catalog rebuild baseline without
  backlog. Unadmitted media across those gaps may be missed; queued snapshots stay
  durable. A transfer whose first frame has begun may finish after Pause. Automatic files still preparing or waiting for the frame writer remain unattempted until Resume; manual sends remain eligible.
- Shares received by the activity survive immediate Home and removal of its task.
  Force-stop/process death can interrupt items still being admitted; queued
  snapshots remain durable, and restored intents never replay admitted items.
- Pneu reads shared items with whatever access it has, including its own photo
  permission. Another app on the phone can ask Pneu to send a readable media item,
  only ever to the user's paired computers captured when sharing starts. External
  callers cannot choose the recipient. Android 14+ and system-chooser shares use
  the same bounded open/read path. Share failures log fixed reason codes under
  the `Pneu` tag, without URIs, filenames, content or provider exception messages.
- A pairing change during sharing or sending can drop an item; it never retargets
  admitted work to a replacement computer. Share activities begin recipient
  capture in `onCreate`: loaded snapshots
  are captured synchronously before suspension; cold entries wait up to five
  seconds for the first authoritative load and
  retain that first recipient set. Failure to load settings is visible. Later
  authoritative reads only validate that recipient set. Clipboard activities
  retain this capture across recreation before focus, and internal button, tile
  and notification actions retain their original recipient authority.
- An image optimized for mobile data keeps that representation on retries. Shared
  files are never re-encoded. Deleting an original leaves queued snapshot bytes
  intact; **Don't send** cancels.
- Staging is bounded to 512 MiB, 16 MiB free reserve and 2048 pending operations.
  Full staging does not evict admitted work. External disk use can consume the
  reserve; Pneu reserves every outgoing payload write before making it visible.
- A provider with no progress for ten seconds fails admission; at most four stalled
  copy workers can remain outstanding. Persistent stalls can exhaust that pool
  until process restart, but cannot hold shared publication locks.
- Clipboard text is latest-writer-wins; files keep their independent delivery lane.
- Boot/update can briefly show the service notification on an unpaired phone before
  it stops. Persistent preference failure shows storage failure or requires opening
  Pneu; it never invents default trust settings.
- Samsung **Never sleeping** is satisfied by the live battery-optimisation
  exemption. Confirmed on the S25+ with One UI on Android 16: **Unrestricted** apps
  are hidden from the Never sleeping picker. Without an exemption, state is
  inferred from guidance/rechecks, since no API exposes the sleeping list.
  OEM restrictions and background-kill evidence still require attention.

## v2 presentation

`ui/` supplies the fixed palette, type, shapes, motion, Link, custom switches,
buttons, scanner frame and floating dialogs. The system animator scale disables
continuous Link motion. Pair keeps its accessibility pane title without drawing
it. Home lists paired computers; each device page has its own send/receive
switches. Global Settings covers automatic media, sharing, clipboard controls,
connection policy and phone preferences.

After an approved icon revision, re-import the vector paths without rasterizing:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 tools/import_v2_icons.py
```

Each generated Crossing vector records the source SVG hash. Launcher foreground
and monochrome retain the 108-unit canvas and the source's 66 dp safe zone. The
device runner's `local` suite runs the complete `TenthReviewUiDeviceTest`, including the
v2 pane-title and custom-switch semantics checks. Run the complete device runner
before installation acceptance, then visually check light/dark, font scales 1.0
and 1.3 (and above 1.3 for stacked buttons), and animations on/off on the phone.

`local` also runs `V2ScreensDeviceTest`, a controlled presentation fixture using
the production Home, scanner, permission and dialog components. Its `v2-*.png`
files are exported into the runner's `local/screens/`: dark connected Home,
sending (capsule and progress), connecting, failed/recording rows, waiting menu,
unpair/pair confirmations, permissions/battery, plus light first-run setup. Real
pairing and transfer behavior is validated separately by the existing peer suites.
The fixture also checks that the Recent header stays put when waiting appears.

## Multi-computer resource limits

The phone supports at most 16 paired computers (matching the desktop session
cap), including lost pairings. A repair uses its existing slot. Automatic media
sends to connected eligible computers by default; the explicit offline-queue
setting adds offline paired computers to the frozen recipient set.

Clipboard text (including shared text) and clipboard images capture each
recipient's authenticated session before settings, focus, pickers or provider
work. Admission and first-frame emission revalidate that session. Disconnect
supersedes its clipboard deliveries; a delayed capture cannot enter a newer
session. Shared files retain their deliberate queue behavior and frozen pairing
identities across reconnects.

Automatic media discovery journals its original recipients and ordering stamp
in Room before opening the provider. Provider errors and pending
MediaStore items retry that same operation, including after observer/process
restart. With offline queuing disabled, an unfinished capture loses a recipient
when its original session ends; if none survive, it is marked seen and skipped.
With offline queuing enabled, the frozen paired recipients may reconnect; newly
paired or connected computers are never added to that operation. Already staged
automatic deliveries retain their existing queue behavior. Policy changes can
revoke an unfinished discovery but cannot replace its recipients or ordering.
Uncommitted copy attempts use distinct output/transfer IDs so a losing overlap
cannot replace or delete the payload another observer has already admitted.

Settings receipts live in Room, indexed by sender and operation ID, and are
retained until that binding is reset, without a lifetime count quota. Identical
retained operations replay safely. Legacy receipts are imported without eviction
before admission.
Only one snapshot is outstanding per direction, and registry state holds only
the three settings fields, local pending edits and that outgoing snapshot.

Image optimization and preview/thumbnail decoding share two process-wide permits.
A timed-out preview retains its permit until its actual worker finishes.

Incoming-file records, including unfinished reservations and permanent receipts,
are capped at 4096 per peer and 16384 globally. Hitting that capacity rejects new
files; completed receipts are not evicted to make room. Outgoing staging limits
are separate from those receive limits.
