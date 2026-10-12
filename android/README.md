# Pneu Android

Kotlin/Compose app, API 34+, application ID `io.github.stantonmatt.pneu`.
JDK 21, SDK 37.2, AGP 9.4.1 and the checked-in Gradle wrapper are required.
Run commands below from `android/` with `ANDROID_HOME=$HOME/Android/Sdk`.

```sh
/home/mjstanton/.local/bin/heavy ./gradlew assembleDebug testDebugUnitTest lintDebug assembleDebugAndroidTest
$ANDROID_HOME/platform-tools/adb -s emulator-5554 install -r app/build/outputs/apk/debug/app-debug.apk
```

## Architecture

- `protocol/`: bounded framing, strict JSON including integer tokens, all v1
  messages, UTF-8 limits and binary chunks; strict invitation parsing. Gradle
  copies protocol vectors into generated test resources. `protocolVectorsPath`
  defaults to the final repo `../docs/protocol-vectors/v1.json`; optional
  `-PprotocolVectorsPath=/absolute/path/v1.json` selects another read-only fixture.
- `connection/TlsIdentity`: Android Keystore P-256 key and self-signed leaf,
  exact DER pin verification, TLS 1.3 mutual authentication with normal TLS
  CertificateVerify. Conscrypt's Keystore upcall requires authorized SHA-256 and
  NONE digests; the TLS signature scheme remains ecdsa_secp256r1_sha256. Each dial
  uses a fresh TLS context without session resumption. Callback-based Android DNS
  is cancellable and capped at five seconds. DNS, sequential TCP attempts (at most
  five seconds each), and TLS (at most ten seconds each) share a 15-second overall
  endpoint budget. Address fallback continues through TLS/pin failures, with fresh
  sockets and per-attempt pin evidence. Pairing UI feedback never resets dialing.
- `ConnectionManager`: one connection, last-good LAN/mDNS/QR/Tailscale candidates,
  transport toggles, network-change retry, jittered backoff, liveness/deadline
  timers, frame-atomic writes, independent files and latest-text delivery. Live
  delivery retries/deferrals use elapsedRealtime. Saved wall-clock nextAttempt
  values are projected once per snapshot into monotonic deadlines on read,
  clamped to 72 seconds (maximum jittered backoff); permanent/manual-retry
  markers stay blocked. Every lane wait selects work signals or timeout, and
  fresh ready rows bypass a failed row's storage cooldown. Pending
  pairing is persisted before PAIR_REQUEST and survives loss of PAIR_RESULT and
  invitation expiry. A rejected replacement leaves the previous binding intact.
  Pending delivery-reset journals are retried before each dialing pass/endpoint
  through the same jittered backoff as connection failure. Transient startup
  identity errors retry rather than permanently failing the initialization latch.
  LAN discovery belongs to the running manager and current transport eligibility;
  disabling LAN stops NSD/watches and releases its multicast lock. Generation and
  watch identity checks reject callbacks from stopped/replaced discovery sessions.
  One connectivity helper distinguishes no network, metered and unmetered access.
  Active-network NOT_METERED determines cost for every mobile-data policy;
  missing capabilities mean no network and follow the offline queue setting.
  Only an available metered network with mobile data disabled queues automatic
  items as Waiting for Wi-Fi even when offline queueing is disabled. VPNs inherit their
  underlying network's meteredness: unmetered Tailscale-over-Wi-Fi retains
  originals and recording eligibility; metered Wi-Fi hotspots use mobile rules.
  Labels use the same cost snapshot (Wi-Fi/Mobile data), or Tailscale for a
  Tailscale endpoint. VPN presence only controls the Open Tailscale hint.
- `PneuService`: foreground `connectedDevice`, qualified by normal
  `CHANGE_NETWORK_STATE`, for the ongoing authenticated device connection. This
  avoids Android 15's time-limited `dataSync` type. Boot/update receivers only request service startup; the service reads preferences
  with a finite deadline and stops itself when unpaired. Failed initialization retries
  with 1–30 second backoff; service start commands wake that retry. Observation and
  connection start once initialization succeeds. Notification clipboard action opens the focused
  activity directly; Pause/Resume persists policy and sends STATUS.
- `data/`: DataStore settings/trust/identity/clipboard ownership and observation admission floors;
  Room queue, permanent MediaStore mappings and incoming hashed text receipts.
  Schema 7 has migrations from schemas 1–6 and a separate durable clipboard timestamp. Java Room declarations use its
  Java annotation processor; behaviour is Kotlin. Room uses FULL synchronous
  writes. Recent is bounded, while dedupe mappings persist until unpairing.
- `media/ScreenshotObserver`: observes Images and Video, both screenshot
  folders, `DCIM/Camera`, and `DCIM/Screen recordings`/`Movies/Screen recordings`.
  Includes pending rows, waits for publication and tracks both added and modified
  generations in independent collection checkpoints. Catalog versions isolate rebuilt ID namespaces.
  Enabling photos/recordings captures an ID boundary to avoid old-media backlog.
  Observation stops while paused. Resume atomically advances image/video scan
  checkpoints and fixed admission floors past a bounded, consistent catalog
  generation. Added generations enforce the floor even if publication later updates
  modification generation. One Preferences policy lock serializes pause/resume,
  media/transport/queue toggles, pairing/identity changes and automatic publication.
  Each policy change advances a durable epoch (the legacy `mediaEpoch` key is retained).
  Snapshot and ID-boundary reads run inside that lock. Scan stamps carry only the
  epoch and catalog version: publication checks both the epoch and the live MediaStore
  namespace under the lock, drops stale bytes/skips without a dedupe tombstone, and
  requests a rescan. Stale automatic capture returns a normal rejection after
  deleting unpublished bytes, logs at debug and leaves the checkpoint unchanged;
  no exception or dedupe tombstone escapes that admission boundary. Routine generation checkpoints do not invalidate a stamp;
  baselines/floor changes do. Floors and checkpoints are monotonic within a catalog.
  Full permissions are required for reliable resume.
  Observer restart and permission restoration baseline observation without backlog.
- `features/Staging`: immutable private bytes/hash/fsync before queue insertion;
  streams originals without whole-file buffering. Provider open/read/close work
  runs in a separate four-worker abandonable pool without shared locks; a ten-second
  inactivity deadline closes/unlinks private temp bytes even if FUSE ignores
  cancellation. Progress permits long healthy copies. Cleanup excludes active
  temps; admission rechecks the captured binding under the short staging lock.
  One reserving OutputStream protects the quota across concurrent copies and JPEG
  compression. Optimization decodes/encodes outside the staging lock and publishes
  under a short lock, retaining the original until the queue swap commits. 512 MiB staging quota, 16 MiB
  free reserve and 2048 pending-operation cap. Local-capacity banner exposes
  Clear queue. On metered networks with originals off, automatic images and clipboard
  images become JPEG quality 85, long edge ≤2560, with EXIF orientation applied;
  recordings wait. Re-encoding gets a new ID before its first attempt; retries
  retain exact bytes/metadata. Shared files always remain unchanged.
  Queue admission and ACK commits are separate from best-effort history/source
  housekeeping: pruning or banner-write failures never delete admitted bytes.
- `features/ClipboardDelivery`: apply incoming text, then atomically commit its timestamp and bounded history
  of 64 applied IDs (hash/timestamp/origin) before ACK. Failed, stale and ignored
  applications create no receipt. Newer timestamps apply independently of older pending receipts; stale/equal timestamps and completed IDs ACK without reapplication. Paused/disabled receive is
  ignored and acknowledged without application. Incoming protocol has no sensitivity flag.
- `features/ManualEntries`: focused translucent clipboard activity, exported
  SEND/SEND_MULTIPLE share target, Pneu toggle
  tile and Send clipboard tile. Text/link shares use CLIPBOARD `origin=share`;
  non-text shares use normative `file`. Manual sends work while paused. Clipboard
  admission requires a live connection and negotiated capability; admitted work
  remains durable through ACK loss. Sensitive Android clips are rejected.
  URI shares require an OS-recorded sender read grant (API 35+); API 34 URI shares fail closed because ordinary activity launches expose no equivalent caller proof. Text shares work on API 34. All admissions carry the same phone/PC binding from entry through metadata, staging
  and enqueue; multi-item shares never acquire a new recipient between items.
  URI shares are copied immediately. No Direct Share publication or FileProvider
  exists. Startup removes legacy dynamic/long-lived shortcuts and full-file Recent
  copies; every incoming EXTRA_SHORTCUT_ID is rejected.
  No persistable source grants are acquired. Startup releases legacy read/write
  grants from older builds; manual source URIs are cleared after admission and
  scrubbed from legacy queue/history. Delivery uses staged bytes. Recent keeps metadata and a ≤64 KiB thumbnail
  only; sent automatic items can open an existing original via ACTION_VIEW with
  a temporary read grant, the original MIME and a matching catalog namespace.
  Missing originals and completed manual files do nothing when tapped.
- Terminal payload history is bounded to 50 operations across sent/cancelled/skipped
  outcomes. Schema 7 keeps outgoing text time and send order in a separate durable
  watermark, and compact automatic-media ID tombstones survive Recent eviction.
  Text timestamps are reserved before admission; older/equal concurrent rows are
  superseded without promotion or replacement of newer text. DataStore/Room
  collection failures retry while preserving the last authoritative value.
  Pause/resume and photo/recording enable boundary failures leave saved policy
  intact. Media startup registration/cleanup share scan recovery and retry.
  Automatic health/share-visibility/clipboard-feedback maintenance has a failure boundary.
  Provider/private-file/encoding/database failures are row-level failures with a
  retry deadline; malformed local payloads fail permanently. An interrupted local
  file read finishes the partial transfer with END/NACK before the next file.
  Only wire errors or a receipt timeout tear down the transport.
  Admitted files are snapshots: delivery never reopens or checks the original URI.
  Media catalog checks only guard observer admission; original deletion does not
  cancel delivery.
  Retrying a row and replacing unsent text never disconnect the file lane;
  in-flight text retains its receipt slot. Ready work bypasses retry-delayed rows.
  Before every selection round each serial sender transactionally recovers its
  own abandoned `sending` rows, preserving the other lane's live receipt slot.
  Failed completion plus failed fallback writes get a bounded per-row storage cooldown;
  recovery retains snapshot IDs/bytes and supersedes interrupted older text.
  A rejected final eligibility check defers that row for one second, allowing
  ready siblings to proceed without repeated hashing or a tight selection loop.
  After each session has joined its cancelled senders, and before every endpoint
  dial, transactional recovery keeps only the newest pending text (including
  interrupted `sending` text). A failed recovery prevents dialing; file retries
  retain their IDs/bytes and the newest text retains its retry deadline.
- Device ID and Keystore certificate alias are committed together. Existing fixed
  aliases migrate without invalidating trust; forget/loss prepares a fresh key
  before publishing the new ID and retires the old key after commit. Local unpair
  commits removal before best-effort UNPAIR on the captured authenticated socket.
- Recent previews and completed thumbnails apply the same EXIF rotation/mirroring
  transform as optimisation (all eight orientations), before display/JPEG encoding.
  They decode bounded private staging/cache bytes; no third-party URI
  thumbnail or stream reads run from the UI. ACK completion and preview cleanup
  share the queue lock, so unpair cannot leave a late preview behind.
- Compose UI retains the approved dynamic-colour/light/dark shell; all toggles
  now affect behaviour. Share-sheet visibility removes its ordinary target;
  Quick Settings preference gates both installed tiles. Existing Android tiles
  remain installed but unavailable when disabled.
  Lifecycle/settings writes share a failure boundary; saved state remains visible,
  navigation follows durable success, and errors offer retry when available.
  Pair/replacement retries require confirmation again. Camera and deep-link
  confirmations retain their last attempted invitation across activity recreation;
  Try again after rejection re-presents it before issuing another pairing request.
  An unpair cleanup failure
  after durable revocation retries cleanup without another identity rotation.
  Notification and tile mutations use the same boundary with toast feedback.
  Daily health-alert throttles also project saved timestamps into bounded monotonic
  deadlines; clock rollback cannot extend the wait beyond a day. Pairing expiry,
  certificate validity and recorded event times continue to use wall time.

## Pairing and test peer

Scan a PC invitation, or start a fresh deep link:

```sh
$ANDROID_HOME/platform-tools/adb -s emulator-5554 shell am start -W \
  -a android.intent.action.VIEW \
  -d 'pneu://pair?v=1&name=Test%20PC&fp=<DER_SHA256>&addr=10.0.2.2%3A47839&token=<TOKEN>&exp=<EXPIRY_MS>' \
  io.github.stantonmatt.pneu
```

`10.0.2.2` is the emulator host; physical phones use PC LAN/Tailscale addresses.
Every deep link/QR requires explicit confirmation in Pneu, naming both PCs for replacement. API 37 requires local-network permission. Tokens, keys and user content are not
logged. There is no production authentication bypass.

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
$ANDROID_HOME/emulator/emulator -avd pneu-test -memory 2048 -no-window -no-audio -no-snapshot-save -gpu swiftshader_indirect -no-boot-anim &
timeout 180 "$ANDROID_HOME/platform-tools/adb" -s emulator-5554 wait-for-device
/home/mjstanton/.local/bin/heavy python3 tools/run_device_tests.py
# Once the runner completes:
$ANDROID_HOME/platform-tools/adb -s emulator-5554 emu kill
```

Wait for the emulator to finish booting before running the tests.
No suite arguments runs all suites. Select one or more by name, for example:

```sh
/home/mjstanton/.local/bin/heavy python3 tools/run_device_tests.py local cold
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
| `features` | Complete Features class: pairing recovery after expiry, silent TLS candidates, share/clipboard, SystemUI tiles, pause/observation boundaries, mid-copy policy invalidation/rescan delivery and latest-text delivery. |
| `policy` | Complete Policy class: actual metered route, mobile encoding, optimized recovery, recordings, mid-copy policy invalidation/rescan delivery, multiple shares, clipboard sensitivity/echo/receipts and staging capacity. |
| `upload` | Hash-verified 160 MiB upload lasting over 180 seconds with no PC traffic before ACK. |
| `daemon` | Builds this worktree's real `pneud`, pairs with private XDG state, unpairs while phone is offline, and verifies exactly three unauthorized frames followed by Pairing lost. |

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
build target directory. `--serial` selects a device (default `emulator-5554`).
The suites change app state, and `cold` clears app data; use a test device.
Each device test resets preferences, queue, identity and injected faults through
`DeviceFixture` in `@Before`. Prepared mTLS fixtures preserve the exported identity;
host-witnessed cold verification phases validate their recorded state without
resetting it. Test permission grants/denials remain fixture-specific. Transport
changes enable the destination before disabling the source; disabling the last
transport is rejected atomically.
Repeated executions of identical classes/scenarios from historical review runners
are consolidated, retaining every distinct fixture configuration and phased test.

## Physical acceptance and interpretation

- Validate Samsung sleep/Doze/reboot, Never sleeping guidance and permissions on
  the S25+; real screenshot latency, HEIF decoding, recording paths, URI grants
  from real share providers, clipboard focus from notification/tile, real
  Wi-Fi/cellular/Tailscale transitions and battery use need physical acceptance.
- The emulator has Wi-Fi enabled but its active default network is metered;
  mobile policy is tested on that observed cost, not a simulated policy flag.
  PolicyDeviceTest also checks Android capability fixtures for Wi-Fi, cellular,
  VPN over each, metered Wi-Fi and missing capabilities, plus the active network's
  actual NetworkCapabilities, without changing routing.
- Normative protocol wins over obsolete design `shared`/`text` kinds and save
  errors described as ACKs: non-text shares are `file`, text is CLIPBOARD, failed
  saves are NACKs. Phone advertises incoming `clipboard_text`, not reverse files.
- Design gives no phone incoming sensitivity metadata or copy; Android-marked
  sensitive outgoing clips are rejected with `Sensitive clipboard. Not sent.`.
  Receive/share/tile settings and local-capacity rows use necessary new copy.
- Clipboard is online-only at admission, but accepted operations persist through
  network/ACK loss as required by protocol. A new text supersedes any older pending text, including failed text. Files keep sending independently.
- Recent is bounded to 50 terminal operations with metadata and small thumbnails;
  full delivered files are not retained. Missing thumbnails use type icons.
  Pending previews use staged bytes; completed automatic items open the existing
  original, while completed shares cannot reopen the sender URI.
  Persistent dedupe records are never evicted to reclaim capacity.
- Lint has no errors. Battery-exemption and custom-trust-manager warnings are
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
- A pairing change during sharing or sending can drop an item; it never retargets
  admitted work to a replacement PC. Share activities begin recipient capture in `onCreate`: loaded snapshots
  are captured synchronously before suspension; cold entries wait up to five
  seconds for the first authoritative load and
  retain that first recipient. Failure to load settings is visible. Later
  authoritative reads only validate that recipient. Clipboard activities
  retain this capture across recreation before focus, and internal button, tile
  and notification actions embed their original binding.
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
- Samsung **Never sleeping** state is inferred from guidance/rechecks, not read
  from a system API. OEM restrictions may require user intervention.

Pass 8 adds `EighthReviewDeviceTest` for resume floors and late captures, shared
JPEG/capture quotas, synchronous boot requests, unpaired-service stop, stale shortcut
rejection, failed share preference reads and thumbnail/original Recent behaviour.
`EighthReviewTest` covers output reservation release/error propagation, competing
writers and observation-token invalidation. Removed tests for pause intervals,
source-existence cancellation, full-file Recent retention and Direct Share
publication were replaced with the accepted contracts above.

Pass 9 adds `ServiceInitializationTest` (backoff, command wakeup, cancellation)
and `NinthReviewDeviceTest` (storage recovery in the live service, Home during a
slow share, actual rotation mid multi-share, temporary grant lifetime, stale
permission-restoration baseline retry). Share and clipboard entry activities
retain one admission across configuration changes and Home/Back;
completed items and the original recipient are never replayed or retargeted.
Shares run in the process scope. `ShareAdmissionService` receives its own temporary
read grants synchronously in the entry activity's `onCreate`, before admission
suspends. It remains foreground only until all active admissions finish, then stops
and releases those grants. Immediate Home or removal of the entry task cannot
cancel sharing or revoke source access. Provider opens/metadata/reads remain off
Main with the same bounded workers and deadlines; no persistable grant is taken.
Clipboard access still requires activity focus; its retained ViewModel keeps
the recipient capture initiated before focus.
A process-death restore with no retained operation reports interruption instead
of replaying already-admitted items; those queued snapshots remain durable.
Media baseline flags clear only after a checkpoint reports committed.

Pass 10 covers actual five-second DataStore deadlines and observer recovery,
caller cancellation, QR callbacks throughout pairing completion, and Recent's
catalog/MIME/temporary-viewer-grant checks. READY has no phone receive-idle
timeout: advancing file writes, PING/PONG and receipt deadlines govern liveness.
Only pinned `ERROR unauthorized` counts toward three consecutive pairing-loss
rejections. Before closing a pre-READY broken write, a bounded 200 ms read can
recover a complete, validated unauthorized frame sent before HELLO; socket errors
and fatal TLS alerts never count. The real-daemon regression asserts exactly three
received unauthorized frames, trust retention before each, and Pairing lost with
identity rotation afterward. Share regression coverage includes a 200 MiB SEND
with Home requested at 100 ms and task removal before preferences complete,
checking every item, original binding, service/grant cleanup and rotation dedupe.

The `upload` suite throttles a hash-verified 160 MiB upload to
over 180 seconds without intervening PC traffic. The `daemon` suite pairs,
stops the phone connection, executes `pneud unpair` against private XDG state,
and verifies automatic Pairing lost on reconnect. It removes its daemon state
and keys and stops its children, leaving emulator shutdown to the caller.

Pass 12 extends the same local runner with delayed initial-settings-read races for
single/multiple/text shares and replacement mid multi-share. Pairing navigation
uses a durable successful-install revision (including same-PC repair), independent
of READY markers; disconnect ends the 800 ms success transition immediately.
Cancelled replacement leaves the old binding intact and the scanner available.
`TwelfthReviewDeviceTest` checks binding replacement over Tailscale with Wi-Fi
still enabled, same-pin/new-ID replacement, same-binding repair and forget.
Replacement clears the previous LAN endpoint, connection time and clipboard hashes
in its durable edit; cache writes verify the session binding inside that edit.
Discovery endpoints remain fingerprint-filtered, and per-session capabilities,
labels and rejection streak reset on binding changes. Phone-wide disk capacity
and monotonic outgoing timestamps remain phone state.

Pass 13 distinguishes the initial unloaded projection (`loaded=false`) from a
loaded unpaired phone. The first authoritative snapshot is retained separately so
replacement during a cold capture cannot redirect admission. Shares transfer their
temporary grants to the process-owned service before waiting for settings.
Clipboard entries retain the same bounded capture across recreation; tiles and
notification actions wait for loaded policy, and notifications publish no actions
while loading. UI renders loading or a retryable settings error before navigation
or permission/health policy decisions. Boot remains a synchronous service-start
request; the service reads authoritative policy before stopping or observing media.

The `cold` suite force-stops both packages before each single-file, multi-file and text
SEND, proves the target has no process, and witnesses queued Room rows from
read-only host copies before any verification instrumentation can warm the target.
The existing warm immediate-Home tests remain in the local suite. Device
verification checks the original paired phone/PC identity, staged bytes/hash and grant-free rows.
JVM tests cover the first-load/replacement race, warm synchronous capture, loaded
unpaired state, five-second failure/recovery and genuine caller cancellation.

Pass 14 adds explicit unsuccessful-unpair, stale pairing-admission and
stale pairing-cancellation outcomes.
The UI keeps the replacement binding, queue and service when the requested
removal loses a race; an already-completed unpair retry preserves the new phone
identity. Thumbnail creation registers its private output before any worker
runs. Startup and delivery-reset sweeps exclude active thumbnails through Room
publication; timeout/cancellation closes and removes the output, so abandoned
workers cannot recreate it. All known protocol strings, including capability
array members, share Unicode-scalar, NUL and UTF-8 byte-limit validation.

The `local` suite covers real Settings unpair/replacement overlap,
stale cancellation, repeated unpair, thumbnail cleanup/publication/reset races,
late cancelled/timed-out preview workers, and the existing lifecycle/write-failure,
FUSE/staging and pairing-transition classes. JVM tests reject escaped lone/misordered
surrogates on read and write while accepting valid pairs and future capabilities
within their UTF-8 bounds.

Pass 15 binds each confirmation to the phone identity, PC pin/ID and durable
pairing revision shown to the user. Unpair and replacement dialogs keep their
original PC name while open. Deferred permission approval retains the same
identity, and stale choices fail visibly; retry shows a fresh confirmation.
Queue removal, bulk clearing and retry capture exact row IDs, transfer IDs and
revisions before launching work. Bulk clear never expands to later arrivals;
stale, completed or replaced rows and replacement-PC queues are left intact.
Connection retry and Send now also validate the captured binding and pending
attempt before restarting transport. The authoritative checks run under the
binding/queue locks.

The complete new class covers frozen unpair/replacement dialogs, delayed unpair
and replacement admission, same-PC repair/phone rotation, exact queue selection,
new arrivals, late ACK/re-encoding/retry and pending-attempt cancellation.
The `local` suite also includes Core and pass 14 classes. The `features` and
`policy` suites cover their updated
explicit queue-selection calls against fresh real mTLS peers.

Pass 16 waits for both loaded settings and the generated phone identity before
rendering navigation or capturing confirmations, including fresh-install pairing
links. The wait has a visible, retryable five-second failure; identity recovery
then resumes the original invitation. Once captured, confirmation identity stays
frozen. An optimized snapshot is reused byte-for-byte even if preparation was
interrupted before its first send. Recent reserves slots for the active file/text
lanes before filling its 50-row limit, retaining pending send order and newest
completed history. Transfer state carries immutable kind/filename metadata for
notifications independently of Recent and publishes progress/Cancel from BEGIN.

The `cold` suite clears app data and witnesses the real deep-link confirmation before
starting instrumentation, then checks an explicitly delayed identity, visible
readiness failure/recovery and frozen confirmation. It also recovers an optimized,
never-attempted snapshot repeatedly, tests two active lanes behind 60 older rows,
checks actual notification metadata without a matching Recent row, and exercises
the active Home progress/Cancel control. It retains confirmed-action, onboarding,
pairing-transition and staging regressions. APK reinstalls happen after `heavy`
admission immediately before each class; device deadlines start inside it.
`PolicyDeviceTest` also recovers an encoded, never-attempted photo before resuming
and checks that its transfer ID/hash survive real mTLS delivery. Run it with the `policy` suite.

Pass 18 checks both delivery lanes at the first-frame write mutex: the selected
row/revision/transfer ID must still be pending for the same phone and PC, with
current transport, capabilities, Pause and metered-data policy. Preparation does
not mark a row attempted. Discarded writer waiters emit nothing; an already-emitted
text retains its receipt slot without resetting the file lane. The `faults` suite
covers removal of both writer waiters, removed emitted text with delayed ACK,
Pause during rehash and JPEG encoding, and manual sends while paused. Its peer
witnesses verify one session and the exact surviving transfers.

Both Quick Settings tiles show Pairing lost ahead of Not paired/Paused and open
Home for Pair again after revocation has cleared the peer. Disabling Quick Settings
keeps both tiles unavailable. Home, Settings and notifications retain the loss
state; the service replaces its ongoing status with the existing recovery alert.
The `local` suite exercises real lost-binding tile clicks and Home recovery.

Pass 19 keeps the full device selection in the runner's executable case table.
Host checks inventory every `@Test` in `app/src/androidTest`, expand whole-class
selectors, and fail on any omitted class/method or unknown selector. Successful
cases must actually run every declared selector. The full `local` suite includes
FullReviewDeviceTest, SecondReviewDeviceTest, all seven EighthReviewDeviceTest
methods, AdvisoryDeviceTest and MainActivityTest. Per-case failure boundaries
cover both instrumentation and host peer witnesses; subsequent independent
classes/method scenarios continue after failure and cleanup. TLS cases export
fresh phone identities independently. All fixture setup/deadlines remain inside
the outer `heavy` job; local classes have finite ten-minute instrumentation caps.

Manual entry Activities expose a nullable operation until common setup finishes.
Activity-created callbacks can run inside `super.onCreate`, before that setup;
regressions wait on explicit operation-ready and admission-started signals before
removing/recreating a task. Clipboard focus received early is deferred until its
operation and captured binding are ready. Lost-pairing tile fixtures set Pause
only after revocation, which deliberately resets pause policy.

Policy interleaving fixtures wait for the live observer to commit both startup
baselines before creating media. Source-specific write and drop latches hold the
copy across the saved policy mutation and verify cleanup without a dedupe tombstone
before releasing the rescan. The local policy regression deliberately stalls
startup and proves the same fresh media is then recaptured under the new policy.
