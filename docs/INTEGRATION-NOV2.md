# November 2 storage and protocol integration

Desktop storage uses `PRAGMA user_version=5`. The startup transaction adds history
pin order, the retained-file catalogue and pending cancels, and local replacement
intents together. It publishes v5 only after every migration succeeds. Membership
and outbox migrations never lower an existing version.

The v4 desktop code at `e5a79cc` checks `version <= 4` before its schema/cleanup
transaction, so it refuses a v5 database. The pre-existing read-only `pairing_epoch`
view also blocks the historical 2.0.2 startup INSERT before its destructive cleanup;
its verbatim startup fixture remains covered by the store tests. Already shipped
binaries cannot be retroactively changed. Arbitrary older builds that ignore both
that version check and that startup fence cannot be guaranteed safe: retain a
backup and do not use an unsupported downgrade. No migration lowers v5 for them.

Android's queue stays on its independent 12 → 13 migration for resume checkpoints,
progress and pending cancels. Battery/ring preferences, notification consent and
replacement intents use DataStore and add no Room version. Saved clipboard history
has its separate 1 → 2 migration. Both Room migrations retain their data/reopen tests.

Desktop HELLO advertises `files`, `file_resume_v1`, `battery_v1`, `ring_v1` and
`notifications_v1` alongside the existing clipboard, mesh and setup capabilities.
Android advertises `files` and `file_resume_v1`, with `ring_v1` only while ringing
is allowed; it does not advertise battery or notification receiver support.
Folder work is outside this integration.

Both codec suites consume `docs/protocol-vectors/v1.json`. The existing Android
real-daemon revocation case remains in `tools/run_device_tests.py`. The new focused
live extension case uses `heavy python3 android/tools/run_nov2_interop.py --serial
emulator-5554 --scratch <private-scratch>` with the disposable `pneu-test` AVD.
It pairs the real Android client with the production desktop Core/TLS/IPC handlers.
The desktop integration harness records OS notification effects in memory so
upserts, replacements and withdrawals can be asserted without a system desktop.
Android uses its real listener, test-app notification posts and ringing service.
The case interrupts an 8 MiB upload after a durable 4 MiB checkpoint, reconnects,
asserts a nonzero READY-based suffix offset and checks the complete received hash.
It also covers battery sharing/clearing, ring start/stop and notification
update/remove/clear. The desktop host test is ignored in normal suites because it
requires that external Android fixture. The driver removes private identities and
pairing readiness material on exit.

Native compile/test runs do not claim live desktop notification/tray rendering,
physical-phone behavior or operating-system permission UX. Those remain separate
from the protocol and fixture evidence.
