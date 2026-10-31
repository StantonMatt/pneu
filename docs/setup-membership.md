# Setup membership: implementation notes and test plan

The [protocol](PROTOCOL.md#members-22-decimal-34) is normative. This document
records integration boundaries and deterministic acceptance cases for the Rust
desktop and Kotlin Android implementations. This is a specification for later
implementation, not a claim that current releases implement `setup`.

## Scope and design choices

The existing token/pin pairing state machines remain authoritative for direct
pairing. MEMBERS is a second, explicitly scoped source of **new** bindings. It
cannot repair a changed certificate, refresh an attempt's frozen authority, or
reset an existing peer. Each device has one optional random 128-bit `setup_id`
and one current roster. The ID is a correlation label, not a group key, admin,
signature or admission authority. Pins, mutual scope consent, terminal removals,
pin uniqueness and frozen pairing fences remain authoritative.

HELLO carries local scope and an assigned ID (omitted on disabled links).
After authority/consent: absent/absent founds only for token pairings; migration
waits for an advertised ID. Absent/S joins, S/S synchronizes, S/T stays direct
without roster exchange. This replaces counts and exact-member evidence for
token pairing and existing-link action/offer/Allow. No invitation keys or address
grammar change. The stable ID travels inside pinned TLS, but a
provisional client can see it before token validation; exclude it from invitations,
mDNS and future relay rendezvous identifiers.

For token pairing with absent/absent, the lower authenticated device ID generates
fresh random S, atomically persisting ID/self/first member. The other waits
without MEMBERS; reconnect advertises S, the other joins atomically and
reconnects. Synchronize only at S/S. The proposer has no later role; IDs are never
derived or reused.

Migration founds only via **Link existing device** on an unassigned device with
no pending prompt: enable local consent on the covered bindings, reconnect and
wait briefly for refreshed HELLOs; join any advertised S, else atomically persist
fresh random S/self-only roster, advance setup generation, then reconnect
advertising S. Link
with a pending prompt acts as Allow and joins instead. Allow, bulk enable and
migration offers only join an advertised ID through an authenticated, mutually
enabled link; absent/absent does nothing. There is no migration proposer: any
enabled member advertising S can supply the join. A self-only Link-founded setup
is normally assigned, with no special replaceability; dissolution is unchanged.
These migration prompts and bulk actions exclude re-admission records below.
A token pairing enables only its new link; if it completes while a migration
join commits on the same device, serialized activation and the MEMBERS ID
backstop keep conflicting assignments as a direct link, never a merge.

One short device-wide transaction permits unassigned → S or idempotent S → S;
first durable assignment wins concurrent S/T joins. No gate spans network/prompt
waits. A local setup generation fences prompts, snapshots and receipts across
assignment, dissolution and withdrawal. Mismatched MEMBERS NACKs unsupported_kind
(false retryability), with no merge, clock observation or receipt. A stray
snapshot cannot assign an unjoined receiver.

Last-other-member removal clears ID/positive roster atomically; any retained
active member, even without an enabled direct link, preserves the ID. Withdrawal
also disables scoped links. Keep tombstones/deny history and high-water marks,
never relabel/export retired state under another ID or clear its local barriers.
There is no separate Leave setup UI in v1.

A tombstone is a durable authorization barrier, not just a LWW display value.
Terminal removal defeats even a newer add or rename unless the exact key's
independent `readmits: {hlc, writer_device}` register is at least retained removal
R. Only fresh local token pairing writes a claim, copying the retained removal
tuple (denied or cleared high-water); the new membership stamp is later than all
observed stamps. Allow, inherited follow, gossip and renames preserve the claim,
never mint one. Merge it by tuple max independently of membership/reach; absence
preserves a cached claim, removal drops it. Validate but ignore claims on the
authenticated sender's own row, without erasing independently learned claims.
A sender cannot vouch for itself. Claim differences do not make equal membership
stamps malformed; receipt metadata includes the claim, even on a sender's own
row. Canonical claim encoding is `{"hlc":<unsigned decimal>,"writer_device":"<Id>"}`,
without whitespace/leading zeroes; absence is omitted, never null. Strict nested
JSON, Id and HLC rules apply; removed entries omit it. The `setup` capability
has not shipped: no version negotiation, and peers without the field treat
values as unclaimed.

Persist local `authored` and `revoked_binding` bits atomically with a barrier.
The former means this installation committed Remove from all devices for the
key; the latter means applying removal deleted a trust row or active roster
membership here. Never infer either from `writer_device`; replay cannot change
origin. Carry facts across a newer removal while denied; clearing the deny ends
that episode. Old persisted barriers with no origin count as authored, and old
member values without claims are unclaimed.

An eligible knowing re-admission (claim ≥ R) asks only at an **authored** barrier:
one durable question per `(id, pin, R tuple)`, authenticated sender as first
introducer, tuple-max member with independent reach/readmits. At an inherited
barrier (`authored=false`, including one with `revoked_binding=true`), follow
silently through the ordinary introduction path: clear deny, keep R as high-water,
install once with sender provenance and **{X} joined · Added via {Y}** (subject
to the joining-device notification exception). All attempt/conflict/capacity
checks still apply atomically. Without a sufficient claim, never install or
prompt regardless of member stamp. Own key and retained receipt replays never
prompt. Pending actions are setup-generation fenced; decisions survive restart
and retirement/rejoin. Dismissal stays pending. A newer removal drops the record;
fresh pairing drops that key's record; retirement discards pending prompts only.

Authored-barrier copy: **{X} was removed from this {phone/computer}. {Y} added it
again · Keep removed · Allow** (Keep removed initially focused on computers).
Coalesce the barrier's card/notification across gossip/restart; close obsolete
notifications on decision, replacement or retirement. Allow is per key, never
bulk migration consent: recheck the same denied R, pending record, generation,
assignment, claim ≥ R, steps 5/6 and capacity at commit. For exact-key Allow or inherited
follow, exempt R and historical barriers for other pins of this ID from the
step-5 deny check; leave those other denies intact. Clear only this deny, keep R
as high-water and install the retained value without restamping or minting a
claim. Allow has no joined notice. Commit/publication/recovery is atomic.

Keep removed stores a durable local decision, authors/propagates nothing and
never re-asks for R, even on newer members from other senders. Only a newer
removal followed by a sufficient claim can ask again; direct pairing remains
available. The decision leaves R in ordinary snapshots and schedules no resend.
Holders of an accepted member whose claim ≥ R ignore the keeper's R; inherited
holders follow the claimed value. A departed keeper cannot keep them split.
A member lacking its own denied barrier, under an ID with a still-denied barrier
for another pin, cannot auto-install or prompt even with a claim: show **{device}
has a different certificate · Pair with it directly**. Direct pairing is required.
When the exact key is denied, other pins' historical barriers do not veto its
prompt, Allow or inherited follow.

An incoming removal at/below an accepted member's claim, even first learned
after that member, only advances removal high-water metadata: no revoke, readiness
reset or prompt. A removal greater than the claim still revokes and drops it.
A fresh pairing without knowledge of R has no claim for R and still loses to
that unseen removal; the member stamp is never a removal high-water mark.
Scoped UNPAIR and an effective own-key tombstone cause only local withdrawal, never a
reciprocal global removal. A withdrawn device stops sending membership, including
pending snapshots; peers that already deny its exact key ignore its MEMBERS.
Historical removed pins for an ID may coexist with its current member, including
when that ID is the sender or receiver. They never withdraw a different-pin
current identity. Retain at most four fingerprints per ID and 64 total entries;
never evict tombstones to make room. Reject an overflowing replacement or merge
atomically rather than losing a resurrection barrier.

Self-written reach is a separate stamped register so forwarded address refreshes
do not masquerade as another device's membership edit or erase a removal. Its
writer equals the subject ID. Intermediaries preserve it; in v1 an authorized
malicious intermediary could forge this unsigned assertion. Normal TLS pin
checks still gate application traffic. Future relay descriptors need a separate
capability/spec; the currently required empty opaque array reserves their place.

Introduced `participating`/`negotiated` flags remain protection flags. UI uses a
separate current-binding-generation HELLO-seen signal for participation and
capabilities, and includes **Also linked** only after direct READY in that
generation. Silent/never-connected rows use neutral copy such as **Hasn't
connected · Open Pneu on {device}, or pair with it directly**. Silence cannot
establish Keep removed, refusal or a need to update. Authenticated `unauthorized`
from an introduced peer shows
**{device} doesn't recognize this {phone/computer} · Pair with it directly**,
keeping trust; timeouts/TLS/route failures do not. Status is generation-fenced
and clears on direct READY. These status signals are local state/UI, not wire
refusal attestations.

## Current implementation boundaries

Source inspected at `eddff3f4341cf1cffed5c222d5f8e13bf671f6fa`:

| Surface | Current behavior and required implementation |
| --- | --- |
| `desktop/crates/pneud/src/store.rs`, `pair_then_hello` | Increments generation, deletes/reinserts the peer, preserves settings but resets dependent delivery state. Add a transactional reconcile path; never replay a roster through this primitive. Preserve the unique fingerprint constraint. |
| Same file, `remember_peer_hello` / `remember_route` | Keeps up to 16 routes, with last-good first. Membership supplies another authenticated control-state hint source in the HELLO category; retain last-good/invitation caches, grammar and total bounds. Do not label a forwarded hint last-good. |
| `core.rs`, `connection`; `tls.rs`, `ClientVerifier::admit` | Unknown certificate admission fails before HELLO. Keep this boundary; learn both sides through the introducer before direct connection. No new provisional setup session. |
| `core.rs`, `finish_pairing_hello` / READY dispatch | Token consumption, known-binding recovery and local self-ID checks already exist. Add gated MEMBERS dispatch/receipts and scoped UNPAIR local withdrawal without reciprocal tombstones or broadening recovery. A tombstoned absent binding cannot use known-certificate recovery. |
| `mesh.rs`, join install fence and `may_revoke` | Direct attempts already capture authority once; reverse dial denial is gated by durable readiness. Introductions must respect live targets and start with no evidence. Setup links suppress destructive unauthorized handling even after evidence. READY ERROR handling in `core.rs` also needs that suppression. |
| `pneu-proto/src/message.rs`, `pairing.rs` | Registry currently ends new control messages at `0x21`; add `0x22`, HELLO setup fields and nested validation. Keep URI and endpoint grammar unchanged. |
| Android `data/PeerRegistry.kt` | `MAX_PEERS=16`, atomic DataStore trust writes. Match the protocol's 16 direct-peer bound; hold phone membership metadata separately from connection peers. Preserve idempotence and pin uniqueness. |
| Android `connection/PairingAttempt.kt`, `AppGraph.kt` | Process-local attempt, scan/HELLO frozen fences, binding lock, local-only cancellation, idempotent same-pin repair and reset journal already exist. Add scope choice to the attempt without refreshing its fence. Reconcile under the binding lock; journal cross-store effects. |
| Android `connection/ConnectionManager.kt` | Only computer HELLOs are accepted, advertises `mesh`, pinned unauthorized can drive pairing loss, settings have a separate pending receipt. Add setup gating, introduced-generation readiness, MEMBERS sender/receiver/receipt lane and recovery buffering. Do not spawn phone workers. |
| Android `data/MeshState.kt`, `protocol/Protocol.kt`, `protocol/Invitation.kt` | Reuse exact unsigned HLC validation and atomic clock authority. Add strict nested MEMBERS/HELLO parsing, including duplicate fields; do not parse u64 through Double/Long. Keep invitation validation unchanged. |

Keep terminal membership storage outside peer-cascade deletion, which currently
removes receipts and pair settings. Extend transaction/reset intents rather than
adding an invitation checkpoint, pairing cancel protocol or time-based trust
expiry. Reconciliation notifications and worker publication need crash-safe
once-only identities, scoped to the new generation.

## Product boundaries requiring orchestration

* The approved mockup says a removed device forgets everyone on reconnect and
  returns to first-run. That cannot be guaranteed with pre-HELLO rejection of
  removed pins. The protocol permits withdrawal when a tombstone reaches a
  still-authenticated session; otherwise the removed device retains its local
  list and receives refusals. Unscoped links and identity survive withdrawal.
* The mockup's “Removing · waits for {device}” cannot prove completion everywhere
  from a single intermediary ACK. Track direct ACKs conservatively; the protocol
  deliberately adds no transitive receipt attestation. A phone cannot directly
  obtain an ACK from another phone. Do not show all-devices-complete on inference.
* Both migrated endpoints start `ask`, so neither sends MEMBERS automatically.
  Product must expose an explicit **Link existing device** action: enable every
  local `ask` binding (and a selected `disabled` one) and reconnect; when
  unassigned with no pending prompt, join an ID a refreshed peer HELLO
  advertises, else found S. With a pending prompt, Link acts as Allow and joins instead. A
  compatible enabled/ask HELLO offers one durable prompt without a roster;
  Allow refreshes HELLO, rechecks compatibility, enables every local `ask`
  binding and resolves the device's other prompts as allowed. Each link activates
  separately; `disabled` links require their own explicit local opt-in and are
  never bulk-enabled. Separate IDs block the offer/Allow on that link.
  Capability negotiation grants no consent.
* The remote setup ID is unavailable from today's URI; late detection may keep
  a pair direct. No group-merge approval, admin prompt or new invitation key is
  introduced.

## Three-device migration walk-through

A is the owner's phone, B the Linux PC, C the Mac. All three direct pairs
already exist; after upgrade every local scope is `ask`, every setup ID absent.
Preserve pins, binding generations, switches, queues and unrelated receipts.

Link and Allow are device-wide, so this takes three user actions:

1. On A, choose **Link existing device**. A enables both its `ask` scopes (A–B,
   A–C) and reconnects; with no pending prompt and no refreshed peer advertising
   an ID, A atomically founds random S with a self-only roster and advances setup
   generation. It reconnects advertising S; B and C each get
   one prompt, without a roster. A's device ID need not be the lowest.
2. On B, Allow. It enables B–A and B–C and resolves B's other prompts. B joins S
   via A's authenticated, mutually enabled S/absent link, atomically persisting
   S/self/A, then reconnects. B–C is enabled on B only, so C's prompts remain.
3. On C, Allow. It enables C–A and C–B and resolves C's other prompt. C joins S
   via the phone or the PC, whichever enabled member advertises S first, then
   reconnects. B–C sees S/S without waiting for A's roster gossip. All scopes
   end enabled; links synchronize only as S/S.

Concurrent Allows on B and C are safe even with device IDs ordered A<C<B and
B–C HELLOs delivered first: absent/absent cannot found; either may join via A,
then supply S to the other. If A disappears after B allows but before B assigns,
B can join through C once C advertises S. A roster alone never opts in an
existing `ask` or disabled link.

Independent Link taps on two unassigned devices with no pending prompts found
two setups; this is the intended no-merge boundary.

Independent A–B and C–D bootstraps instead found S and T. An A–C action sees S/T
and keeps the direct link, with no offer or roster exchange. If these assignments
arise while a prompt is pending, Allow reaches the same result after refreshing
HELLO. This is the intended no-merge boundary: independently founded setups stay
separate, with **Direct link kept; separate setups not linked**.

## Cross-language JSON vectors

The snapshot blocks below are literal single-line MEMBERS JSON payloads, no BOM
or newline in the wire payload; responses and invalid nested objects are labeled
separately. A=32 `1`s (Phone), B=32 `2`s (Office PC), C=32 `3`s
(Laptop); pins are respectively 64 `a`s, `b`s and `c`s. Test clock is fixed at
`1800000000100`. The authenticated sender is A, receiver B; both HELLOs advertise
`mesh,setup`, enabled scope, and have already passed direct binding checks.
Both are assigned S=32 `0`s; T=32 `f`s denotes a separate setup.
B initially has only A's direct binding and its own identity. Snapshot order is
for readability; comparison sorts entries by `(device_id, fingerprint)`, ignores
unknown fields, compares known values and preserves address order. These test
serialization choices are not a JSON signing/canonicalization scheme.

The supplemental scratch `readmit-vectors-v2.json` sequence snippet starts from
the current shared implementation vectors; this branch has neither shared file. It
contains literal MEMBERS payloads plus labeled local actions/expected state
(test metadata, never wire fields). Its additional D=32 `4`s, pin 64 `d`s (Other PC), is an independently
authenticated enabled sender already bound at B; `readmission-from-D` proves
that another sender updates the retained value without replacing introducer A.
Run the same payloads and sequence assertions on both platforms when importing
into their shared vector files.

### setup-id-compatibility

These are nested `setup` HELLO objects, not MEMBERS frames. Both sides evaluate
the same pair; after authority/consent the ID cases below decide assignment
or synchronization, with absent/absent founding restricted to token pairing.
An enabled/ask offer checks compatibility before prompting; Allow refreshes
HELLO and rechecks. `expected` is test metadata, not a wire field.
S/T values are deterministic test fixtures, never a recipe for founding IDs.

```json
[
  {"stage":"A-B token pairing mutual opt-in","left":{"scope":"enabled"},"right":{"scope":"enabled"},"expected":"absent/absent: lower authenticated ID founds random S; other waits for HELLO"},
  {"stage":"B-C migration mutual opt-in before assignment","left":{"scope":"enabled"},"right":{"scope":"enabled"},"expected":"absent/absent: no founding; wait for either endpoint to advertise an ID"},
  {"stage":"A-C mutual opt-in","left":{"scope":"enabled","setup_id":"00000000000000000000000000000000"},"right":{"scope":"enabled"},"expected":"S/absent: C joins S, reconnects before synchronization"},
  {"stage":"C-A mutual opt-in","left":{"scope":"enabled"},"right":{"scope":"enabled","setup_id":"00000000000000000000000000000000"},"expected":"absent/S: C joins S, reconnects before synchronization"},
  {"stage":"B-C before roster catch-up","left":{"scope":"enabled","setup_id":"00000000000000000000000000000000"},"right":{"scope":"enabled","setup_id":"00000000000000000000000000000000"},"expected":"S/S: synchronize without exact-member evidence"},
  {"stage":"A-C separate bootstraps","left":{"scope":"ask","setup_id":"00000000000000000000000000000000"},"right":{"scope":"ask","setup_id":"ffffffffffffffffffffffffffffffff"},"expected":"S/T: keep direct; no offer or roster exchange"},
  {"stage":"Allow after separate assignments while prompt pending","left":{"scope":"enabled","setup_id":"ffffffffffffffffffffffffffffffff"},"right":{"scope":"ask","setup_id":"00000000000000000000000000000000"},"expected":"T/S: resolve prompt; keep direct without enabling"}
]
```

### join

Install C exactly once, enabled scope, readiness zero, introducer A. B's own row
is observation only. No settings/content move from A. Queue PEER_SETTINGS
bootstrap for B↔C upon their later direct session.

```json
{"transfer_id":"44444444444444444444444444444444","setup_id":"00000000000000000000000000000000","members":[{"device_id":"11111111111111111111111111111111","fingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","role":"phone","name":"Phone","status":"member","hlc":1800000000001,"writer_device":"11111111111111111111111111111111"},{"device_id":"22222222222222222222222222222222","fingerprint":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","role":"computer","name":"Office PC","status":"member","hlc":1800000000002,"writer_device":"11111111111111111111111111111111"},{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"member","hlc":1800000000003,"writer_device":"11111111111111111111111111111111","reach":{"addresses":["192.168.1.12:47821"],"relay":[],"hlc":1800000000004,"writer_device":"33333333333333333333333333333333"}}]}
```

### remove

After join, this removes C through B's forget lifecycle and leaves a terminal
tombstone, including when received after a newer concurrent member value.
A relays B's original writer; writer need not equal transport sender.

```json
{"transfer_id":"55555555555555555555555555555555","setup_id":"00000000000000000000000000000000","members":[{"device_id":"11111111111111111111111111111111","fingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","role":"phone","name":"Phone","status":"member","hlc":1800000000001,"writer_device":"11111111111111111111111111111111"},{"device_id":"22222222222222222222222222222222","fingerprint":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","role":"computer","name":"Office PC","status":"member","hlc":1800000000002,"writer_device":"11111111111111111111111111111111"},{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"removed","hlc":1800000000010,"writer_device":"22222222222222222222222222222222"}]}
```

### later-introduction-unclaimed

After remove, this higher-HLC add MUST NOT recreate C, advance its binding
generation, reset unrelated state or clear the terminal barrier. Record a new
membership receipt (ACK false), retain/gossip removed C at stamp 1800000000010.
There is no claim: never record a question, even if this is a renamed stale
member. Receiving remove after this vector still removes C without a question.

```json
{"transfer_id":"66666666666666666666666666666666","setup_id":"00000000000000000000000000000000","members":[{"device_id":"11111111111111111111111111111111","fingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","role":"phone","name":"Phone","status":"member","hlc":1800000000001,"writer_device":"11111111111111111111111111111111"},{"device_id":"22222222222222222222222222222222","fingerprint":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","role":"computer","name":"Office PC","status":"member","hlc":1800000000002,"writer_device":"11111111111111111111111111111111"},{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"member","hlc":1800000000020,"writer_device":"11111111111111111111111111111111","reach":{"addresses":["192.168.1.12:47821"],"relay":[],"hlc":1800000000004,"writer_device":"33333333333333333333333333333333"}}]}
```

### later-introduction-claimed

The same member carries a claim equal to R. At an authored barrier, record one
pending question with introducer A; no install or joined notice until Allow.
At an inherited barrier, install once with enabled scope, readiness zero,
introducer A and the ordinary joined notice; keep R as high-water. Duplicate
gossip/restart cannot repeat the install/question. If this arrives before R,
R cannot revoke the claimed member or reset a READY link. A removal newer than
the claim still revokes. The claim does not weaken the new-pin guard.

```json
{"transfer_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","setup_id":"00000000000000000000000000000000","members":[{"device_id":"11111111111111111111111111111111","fingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","role":"phone","name":"Phone","status":"member","hlc":1800000000001,"writer_device":"11111111111111111111111111111111"},{"device_id":"22222222222222222222222222222222","fingerprint":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","role":"computer","name":"Office PC","status":"member","hlc":1800000000002,"writer_device":"11111111111111111111111111111111"},{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"member","hlc":1800000000020,"writer_device":"11111111111111111111111111111111","reach":{"addresses":["192.168.1.12:47821"],"relay":[],"hlc":1800000000004,"writer_device":"33333333333333333333333333333333"},"readmits":{"hlc":1800000000010,"writer_device":"22222222222222222222222222222222"}}]}
```

### reach-refresh

From join state without removal, only C's independent reach changes. Preserve
C's generation, provenance, queues, receipts and pair settings. Import the hint
in HELLO priority after last-good, never as proof it works. From removed state
this does not restore C or its dial worker.

```json
{"transfer_id":"77777777777777777777777777777777","setup_id":"00000000000000000000000000000000","members":[{"device_id":"11111111111111111111111111111111","fingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","role":"phone","name":"Phone","status":"member","hlc":1800000000001,"writer_device":"11111111111111111111111111111111"},{"device_id":"22222222222222222222222222222222","fingerprint":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","role":"computer","name":"Office PC","status":"member","hlc":1800000000002,"writer_device":"11111111111111111111111111111111"},{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"member","hlc":1800000000003,"writer_device":"11111111111111111111111111111111","reach":{"addresses":["192.168.1.13:47821"],"relay":[],"hlc":1800000000030,"writer_device":"33333333333333333333333333333333"}}]}
```

### targeted-withdrawal

From join state, A removes B, the receiver. B retains its own-key tombstone and
deny/high-water history, clears ID/positive roster, disables scoped links and
sends neither removals against A/C nor pending MEMBERS. Scoped UNPAIR from A
has the same withdrawal effects without inventing a tombstone. C may receive A's removal later; before then B's receipt
of this notification must not cause C to revoke A. Once C knows B's tombstone,
it ignores every MEMBERS from B, even a captured in-flight removal of A.

```json
{"transfer_id":"88888888888888888888888888888888","setup_id":"00000000000000000000000000000000","members":[{"device_id":"11111111111111111111111111111111","fingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","role":"phone","name":"Phone","status":"member","hlc":1800000000001,"writer_device":"11111111111111111111111111111111"},{"device_id":"22222222222222222222222222222222","fingerprint":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","role":"computer","name":"Office PC","status":"removed","hlc":1800000000010,"writer_device":"11111111111111111111111111111111"},{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"member","hlc":1800000000003,"writer_device":"11111111111111111111111111111111","reach":{"addresses":["192.168.1.12:47821"],"relay":[],"hlc":1800000000004,"writer_device":"33333333333333333333333333333333"}}]}
```

### receiver-certificate-replacement

Override the default receiver identity: B has successfully replaced its local
certificate and now uses pin 64 `d`s. A already paired directly with that new
certificate, retaining B's old-pin removal. This full snapshot is valid at B:
retain the `b` tombstone, preserve B's current `d` identity, and do not withdraw.
A different-pin `member` for B remains malformed. Historical removed metadata
need not match the current incarnation's role/name. Apply the same allowance
when a sender includes its own old-pin tombstone beside its authenticated member.

```json
{"transfer_id":"99999999999999999999999999999999","setup_id":"00000000000000000000000000000000","members":[{"device_id":"11111111111111111111111111111111","fingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","role":"phone","name":"Phone","status":"member","hlc":1800000000001,"writer_device":"11111111111111111111111111111111"},{"device_id":"22222222222222222222222222222222","fingerprint":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","role":"computer","name":"Office PC","status":"member","hlc":1800000000020,"writer_device":"11111111111111111111111111111111"},{"device_id":"22222222222222222222222222222222","fingerprint":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","role":"computer","name":"Office PC","status":"removed","hlc":1800000000010,"writer_device":"11111111111111111111111111111111"},{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"member","hlc":1800000000003,"writer_device":"11111111111111111111111111111111","reach":{"addresses":["192.168.1.12:47821"],"relay":[],"hlc":1800000000004,"writer_device":"33333333333333333333333333333333"}}]}
```

Exact wire frame prefixes for those payload bytes (length includes type):

| Vector | JSON bytes | L | Four-byte length then type |
| --- | --- | --- | --- |
| join | 993 | 994 | `000003e222` |
| remove | 867 | 868 | `0000036422` |
| later-introduction-unclaimed | 993 | 994 | `000003e222` |
| later-introduction-claimed | 1077 | 1078 | `0000043622` |
| reach-refresh | 993 | 994 | `000003e222` |
| targeted-withdrawal | 994 | 995 | `000003e322` |
| receiver-certificate-replacement | 1250 | 1251 | `000004e322` |

Common ACK for first join; the same receipt with true is expected on retained
identical replay. ACK's `deduplicated` describes receipt history, not whether an
introduction was already known or blocked by a tombstone.

```json
{"transfer_id":"44444444444444444444444444444444","deduplicated":false}
```

Unscoped/pending-consent or setup-ID-mismatch response to join (no receipt):

```json
{"transfer_id":"44444444444444444444444444444444","code":"unsupported_kind","retryable":false}
```

Capacity/transaction failure response to join:

```json
{"transfer_id":"44444444444444444444444444444444","code":"storage_failed","retryable":true}
```

These are literal invalid entry/HELLO objects usable as nested parser vectors;
embed entry objects in a snapshot with a new transfer ID and the valid A self
entry. Duplicate known fields MUST be detected before a map parser loses them.

```json
{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"member","hlc":1,"hlc":2,"writer_device":"11111111111111111111111111111111"}
```

```json
{"device_id":"33333333333333333333333333333333","fingerprint":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","role":"computer","name":"Laptop","status":"member","hlc":1e0,"writer_device":"11111111111111111111111111111111"}
```

```json
{"scope":"enabled","setup_id":null}
```

## Required tests on both platforms

Use deterministic clocks, transaction fault injection and explicit completion
barriers. Implement codec/merge tests on JVM and Rust; add daemon integration
and Android instrumented DataStore/Room recovery tests for lifecycle cases. Each
row needs assertions on durable state, admission/worker effects, messages and
unrelated peer isolation. A static codec pass is not runtime pairing proof.

| Case | Setup/action | Required result |
| --- | --- | --- |
| Full flow | A paired with B; new C pairs with A using switch on | A exports C only after reciprocal enabled scope; C and B learn each other through A; direct B↔C eventually READY with PEER_SETTINGS defaults. Content still direct. |
| Stale replay after removal | join → remove → join (retained and evicted receipts) | C stays removed; retained replay ACK true, evicted replay ACK false; no install, settings bootstrap or notice. |
| Concurrent add/remove | Apply remove and later-introduction-unclaimed in both orders, across restart | Deny/removed roster in both orders, no prompt regardless of member stamp. An unclaimed fresh pair still loses to unseen R. Competing removals use tuple max; equal tuple/different status malformed atomically, even with a claim. |
| Fresh re-pair | remove → fresh local token pair C → replay old remove; repeat with cleared removal high-water | Fresh fenced pairing drops the record, clears deny and mints member > observed stamps with readmits=R. Old removal ≤ claim cannot revoke; removal > claim revokes and drops the claim. No retained R means no claim for an unseen removal. |
| Re-admission prompt once | Locally authored remove → later-introduction-claimed; duplicate/new-ID gossip from A/D, restart, dismiss; unclaimed or insufficient claim | One durable question for authored R, first authenticated introducer retained, tuple-max membership/reach/readmits. No install/reset/joined notice/roster resend. Unclaimed newer members never prompt. Receipt replay has no effects; coalesce notifications across restart. |
| Barrier origin and inherited follow | Same claim ≥ R at local remover, former holder and never-holder; restart/replay with writer equal to local ID | Authored asks; inherited revoked_binding=true and false both install once, enabled, joined notice (except initial members on joining device), no prompt. Origin committed atomically, not inferred from writer or changed by replay; carry facts while denied, reset on clear. |
| Claim/member delivery orders | later-introduction-claimed before/after R; direct READY before delayed R; then removal > claim | Same accepted member, retained R high-water, no READY flap or second install; newer removal revokes and drops claim. |
| Independent claim register | Equal membership stamp with/without claim, higher/lower claim, self-row claim then third-party forwarding; renamed unclaimed self value | Claim-only merge is valid, tuple max independent of membership/reach; retain across rename/absence. Strip incoming sender self claim without erasing independent evidence. Removal drops claim; later insufficient claim cannot restore it. Receipt includes claim: same ID changed claim is id_conflict; fresh ID can merge. |
| Inherited commit guards | Claimed follow races live attempt, conflicting ID/pin, capacity/storage failure and retirement | Ordinary introduction guards still hold; no partial deny clear or generation/worker/notice changes. Retain blocked candidate for reconciliation after attempt; stale generation cannot install. |
| Departed keeper | Remover keeps R then departs; new holder receives R and claimed M in both orders | No inherited prompt, no permanent split or revoke of claimed M; keeper's local refusal is not global removal or remote refusal evidence. |
| Migration and claim validation | Origin absent in old barrier; member without claim; malformed readmits (null/missing/duplicate fields, invalid Id/u64/future HLC, on removed) | Missing origin counts as authored; no claim stays unclaimed and cannot prompt. Valid claim asks old barrier. Strict recursive decode even for ignored sender self claims; rejection atomic. Canonical object/omission and frame sizes agree across codecs; no new version negotiation. |
| Re-admission Allow | Authored pending → Allow twice; replay members and R; removal greater than claim; fail commit/recovery stages | Install once as introduced, enabled, one generation, retained stamp/claim/provenance and settings defaults, no joined notice. Keep R high-water; old R replays no-op, removal > claim revokes/drops claim and record. No duplicate publication after recovery. |
| Allow commit guards | Capacity full; ID/pin conflict; live attempt by ID or pin; action/notification races newer removal or leave/rejoin (including same setup ID) | No partial deny clear/install; capacity/storage failure visible and pending retained; conflict diagnostic; live attempt defers until completion. Stale key/R/generation actions do nothing; notification and in-app actions use identical fences. Phone subjects remain metadata only. |
| Keep removed | Authored pending → Keep removed; claimed members from multiple senders; restart/retire/rejoin; fresh pair; newer removal then claim ≥ it | Decision never re-asks for R, keeps deny/gossiped R; no authored value or resend from decision. Fresh pair drops record. Newer removal drops decision; sufficient claim can ask for new authored barrier. Holders of accepted claimed values ignore keeper R, even after keeper departs. |
| New-pin guard | Denied C/old pin; claimed member C/new pin without its own barrier/binding; restart/replay; direct pair | Claim cannot bypass guard: no install/dial/question; retain old barrier, diagnose different certificate and require direct pairing. Already-cleared historical barrier alone does not block introduction. |
| Exact-key Allow after replacement | Join old pin → direct replacement → locally remove new pin → claimed member for new pin → restart → Allow | Old-pin deny remains. Authored new-pin barrier asks once; Allow installs only new pin, clears only its deny, retains its high-water/claim and preserves old barrier; no joined notice. |
| Own key never prompts | Own effective removal then fresh rejoin; own claimed member; historical own pins | No re-admission record/prompt or identity mutation. Effective current-key removal withdraws; removal ≤ independently retained claim does not. Different-pin own member remains malformed. |
| Re-admission retirement | Pending and decided records; dissolution/withdrawal then join another or same ID | Pending work discarded/fenced; decisions retained with barriers without retired roster export. New gossip cannot reopen a decided R. |
| Honest introduced status | Introduce without HELLO; silent keeper; HELLO/READY; authenticated unauthorized; timeout/TLS failure; replacement | HELLO-seen gates participation/capabilities; current-generation READY gates Also linked. Silent/kept peers get neutral Hasn't connected copy, never inferred refusal/update. Only authenticated unauthorized gives direct-pair diagnostic; preserve trust; direct READY clears it; old-generation events fenced. |
| Recovery is not fresh consent | Cancelled/lost-result old attempt, known-cert recovery, HELLO after removal | Old attempts/reconnects cannot clear a tombstone or recreate a removed binding; a fresh post-removal locally confirmed token attempt can; no recovery UNPAIR or refreshed install fence. |
| Reused ID | C exists; incoming same ID/different pin, then removed old pin with newer replacement present | Introduction cannot replace; old-key removal cannot delete replacement. No queue, generation or pin change. |
| Historical self pins | receiver-certificate-replacement at B; replay/reorder across restart; sender also includes its own old-pin tombstone | Full snapshot accepted, exact old-key barriers retained, current identity/binding unchanged and no withdrawal. Historical role/name may differ. Different-pin member for receiver or mismatched sender member still malformed. Current-key tombstone still withdraws. |
| Unique pin | Incoming new ID claims existing C pin, local pin, or sender pin | No second binding for the pin; local/sender impersonation malformed; other retained-ID conflict diagnosed without replacement. |
| Self-introduction | Sender describes itself, spoofs self pin, or sends receiver member row | Matching self description only refreshes existing metadata; no installation. Spoofed sender/local identity rejects full snapshot. Receiver identity never overwritten. |
| Unchanged roster | Replay join with same ID, new ID, reordered entries/fields and irrelevant unknown fields | No generation/reset/provenance/notice change; preserve queued files, file/text/settings receipts, pending settings, switches, readiness and last-good route. Only appropriate receipt/clock state may advance. |
| Receipt conflict | Reuse join ID with changed C name, reach, stamp, or operation type | Retained ID conflict NACK false retryability; no partial merge. Unknown fields/order alone are equivalent; address order change conflicts. |
| Reach authority | Direct C update; forwarded unchanged C reach; wrong reach writer; changed addresses at same reach tuple | Original self-writer maintained; wrong writer/equal-tuple conflict malformed; intermediary cannot rewrite/stamp C's reach. Phone endpoints/nonempty relay rejected. |
| Partial knowledge | Deliver C to B, delay B to C; B dials C with no invitation open, then with one open | Pre-HELLO unauthorized/provisional refusal retries; introduced trust survives. No unsolicited PAIR_REQUEST. Deliver B to C, direct link succeeds. |
| Unauthorized after readiness | Introduced/scoped link reaches READY, then peer sends unauthorized | Connection problem only, no trust deletion or tombstone; delayed outcomes cannot affect a replacement generation. Plain direct-link loss handling still works. |
| Live attempt | Attempt targets C ID; unresolved invitation targets C pin; unrelated attempt targets D | Block C install by either matching key, even before HELLO; preserve fence/attempt. D unaffected. Attempt completion/cancel re-runs reconciliation without requiring a new receipt. |
| Tombstone vs attempt | Tombstone C arrives while direct attempt targets same incarnation, or targets known different replacement pin | Same-incarnation attempt cancelled/fenced; replacement attempt not cancelled by old-key removal; no UNPAIR from cancellation. |
| Crash before commit | Fail after each staged member/reset/receipt/clock write | No partial state/admission/success event/ACK. Durable old binding remains authoritative. |
| Crash after commit | Kill after authoritative commit, before Room cleanup, worker publication, notice or ACK | Startup resumes intents before affected workers/content; no second generation/install or duplicated notice; eligible same-generation replay ACK true after cleanup. Apply intents to multi-peer removal and own withdrawal too; retired-generation replay never mutates/ACKs a new setup. |
| Offline catch-up | C offline; A introduces/removes D; A later offline; B has state | B supplies full stamped state to C; no dependence on original writer online. Missing entries never delete C's local winners. |
| Legacy | Peer lacks setup or lacks mesh, unknown capability ignored | No MEMBERS sent; old direct file/clipboard/settings behavior preserved, no trust/generation change. Setup-without-mesh/object malformed. |
| Switch off | Dialer or inviter chooses off; other endpoint enabled | New pair stays direct; neither introduces it to other members. Unexpected scoped traffic on disabled link NACKs unsupported_kind without mutation or prompt. |
| Migration | Upgrade to ask, explicit local action, deny/allow, restart; absent or compatible IDs | Enabled/ask HELLO gives one durable prompt without MEMBERS; both-ask sends nothing. No roster/clock/receipt import before Allow; refresh HELLO and recheck assignment, then reconnect with matching persisted IDs to synchronize. Deny/disabled never prompts automatically. |
| Setup ID compatibility | Run setup-id-compatibility in both directions, token pairing and existing-link action/offer/Allow; disabled local scope | Absent/absent founds only for token pairings; migration waits for an advertised ID. Link without a pending prompt founds locally if unassigned. Absent/S joins, S/S synchronizes, S/T keeps direct with no roster. Mutual enabled scope and authority required; disabled HELLO omits ID. IDs never bypass pins, tombstones or fences. Ordinary same-ID reconnect preserves consent. |
| Three-device full-mesh migration | A phone, B Linux PC, C Mac, all pairwise ask; Link on A, Allow on B, Allow on C; restart/replay of prompts; delay B/C gossip | A's Link atomically founds S/self-only roster and advances setup generation; B joins via A; C joins via A or B, whichever advertises S first. B–C sees S/S without waiting for gossip. Three user actions total; each device prompts at most once per initiator and one Allow resolves its other prompts; `disabled` links are never bulk-enabled. All scopes end enabled without binding/queue/settings resets; ask links never supply export authority. |
| Reordered migration HELLOs | IDs A<C<B; Link on A; Allow on B/C; deliver B–C HELLOs before A's assignment HELLOs, then reverse order; vary A–B/A–C delivery order | Only A's Link founds S. B–C absent/absent never founds T, even though C<B; B/C join the first enabled member advertising S and finish S/S. |
| Migration founder loss | A's Link founds S; B allows but remains unassigned; C joins S; A disappears before B commits; deliver C's S HELLO to B | B joins S via authenticated, mutually enabled C, reconnects and synchronizes at S/S; no designated-proposer wait or second founding ID. |
| Concurrent migration Allows | After A's Link, Allow on B and C concurrently; race absent/absent B–C HELLOs against A's S HELLOs and both join commits | B/C only join advertised S, with one durable assignment each; absent/absent cannot found. All three end in S, other prompts resolve and links synchronize only at S/S. |
| Link joins before founding | A advertises S; unassigned B has A's link `disabled` (no prompt); user taps Link for A on B; separately, a peer unreachable during the window later advertises T | B enables, reconnects, sees A's refreshed S HELLO and joins S without founding. The late T peer yields S/T and a direct link, no merge. |
| Link defers to an advertised ID | A is assigned S with A–B `ask`; unassigned B taps Link (no prompt); A allows only after B's discovery window | B's refreshed HELLO from A advertises S, so B never founds; it keeps its pending-Link record, shows waiting for A, and joins S when A–B becomes enabled. If A discovers B first (B founded before A's HELLO), the later Link joins instead; covered separately from the two-setup case. |
| Link survives restart | Kill B after Link persisted consent and the pending-Link record but before founding or joining; restart | Discovery resumes from the start; B joins an advertised S or founds after the window. Never left enabled and unassigned with no pending record. |
| Link with a pending prompt | A advertises S; unassigned B has A's prompt; tap B's Link banner instead of Allow | Link acts as Allow: enable B's ask bindings, resolve its other prompts, join advertised S and reconnect; no new setup founded. |
| Independent Link taps | Unassigned A/B have no pending prompts; tap Link on both; hold each side's assigned HELLO until both founding commits have happened, then deliver in both orders | Each atomically founds its own S/T with self-only roster. They remain normal assigned setups, not replaceable; S/T keeps the direct link with no merge or roster exchange. |
| Token pairing racing migration join | Unassigned B completes a token pairing with unassigned D while B's enabled migration join to S commits; arrange competing token founding T, swap the token proposer and test both commit orders with delayed HELLO/MEMBERS | Token pairing enables only B–D. First durable assignment wins; the conflicting link stays direct, with serialized activation and MEMBERS ID checks preventing a merge or cross-setup effects. |
| Independent bootstraps stay separate | A–B found S, C–D found T; A–C ask action in either direction; also assign S/T while prompt pending | Block offer/Allow on S/T, disable evaluated scope, resolve prompt with Direct link kept; separate setups not linked. Preserve direct queues/settings; no cross-group roster, clock or receipt mutation. |
| Concurrent joins to different IDs | Unassigned E races mutually enabled joins to S and T in both orders; fault before/after durable assignment | Device-wide local transaction admits exactly one ID; first durable assignment wins. Other link stays direct, no cross-group merge. No gate held over network/prompt; restart preserves assignment; mismatched late MEMBERS NACKs without effects. |
| Token founding on both ends and crash recovery | Token pairing, both absent/enabled; swap lower ID and initiator; crash lower-ID proposer after assignment commit before reconnect and crash adopter before/after commit | Only lower authenticated ID generates random S; ID/self/first member atomic. Other waits unassigned without MEMBERS; restart advertises same persisted S, other adopts once and reconnects. Synchronize only at S/S; no second founding ID or repeated install. Retired IDs never reused or derived from identities/roster. |
| MEMBERS ID mismatch | Joined S receives valid T snapshot, including retained transfer ID; unassigned device receives stray S snapshot | NACK unsupported_kind, retryable=false; no roster, clock, receipt or worker change. Report separate setups for mismatch; unassigned cannot adopt without mutually enabled founding/join context. Missing/null/invalid ID malformed. |
| Last-member dissolution | Remove last other active member locally or via MEMBERS; repeat with another retained active member whose direct scope is ask/offline | Last removal atomically clears ID and positive roster, advances setup generation, retains all tombstones/deny/high-water marks. Another active member preserves S. Later join/founding exports no retired state and cannot resurrect denied keys; retained history still counts toward local tombstone budgets. |
| Stale work after leave/rejoin | Capture prompt, snapshot, pending send and receipt in generation g; dissolve/withdraw and join another ID, then also retry same-ID rejoin | All g work fails generation recheck at commit/use, including same-ID work; cannot enable scope, merge, observe clocks, replay receipt or send retired state. Fresh-generation consent/work required. |
| S5 retained-member bridge | A–B–C share S; B learns C through A while B–C stays ask; B removes A, then pairs with D in D–E setup T | C remains active in B's roster, so B retains S despite no enabled direct bindings; S/T keeps B–D direct, no C export into T. B's UI offers Remove from all devices for C even though B–C is still ask; if B removes C that way, B dissolves first; joining T carries no retired positive roster/tombstones, while local deny barriers remain. |
| Phone–phone | A and D phones in same roster introduced through B | Both retain membership metadata/removals; neither dials/listens to the other or creates phone settings/content delivery. Desktop still serves both phones. |
| UNPAIR/removal | A removes B; deliver scoped UNPAIR or targeted-withdrawal to B before C learns B's removal; ordinary unscoped UNPAIR; MEMBERS removal replay | A authors only B's removal. B withdraws locally without authoring/propagating removals against A/C, UNPAIR echoes or pending MEMBERS; C keeps A. Unscoped clears only that pair; replay does not restamp. Saved files unchanged. |
| Legitimate reverse removal | B is still authorized/not withdrawn; user on B chooses Remove from all devices for A | B authors A's tombstone and gossips to C; B's scoped UNPAIR notification causes A's local withdrawal without reciprocal tombstones. C removes only A. |
| Removed sender | C knows B's exact-key tombstone, then B sends MEMBERS removing A, including an in-flight message, receipt replay or racing local removal | Ignore before any merge/clock/receipt/worker effects and close stale session; transaction recheck fences race. A unaffected. A fresh local token pair clearing that exact deny restores normal eligibility. |
| Own withdrawal | Current exact-key tombstone or scoped UNPAIR on authorized session; then fresh pairing/join | Clear ID/positive roster, advance setup generation, disable/forget scoped links; retain deny/high-water history, identity, unscoped links and files. No reciprocal removals, echoes or pending exports. Old-pin tombstone does not withdraw. Fresh pairing plus mutually consented assignment required; retired roster never exported under new ID. |
| Size/cap | 17/18 active, 64/65 total, 4/5 fingerprints per ID, 16/17 local peers, union overflow, exactly 65536/65537 encoded bytes | Boundaries accepted when all other checks pass; structural array/per-ID overflow malformed, frame overflow frame_too_large before allocation, merged capacity NACK storage_failed with no partial merge/eviction. At cap, local replacement fails visibly before commit, tombstones survive restart/replay, removal still fits by replacing an entry; no resurrection through eviction. |
| Strings/types | 128/129-byte name, Unicode byte length, NUL/unpaired surrogate, address max/grammar, duplicate ID/pin key, null/unknown enums, missing field, negative/fraction/exponent HLC | Match strict common rules on both codecs, no partial snapshot. Unknown valid object fields ignored. Unknown URI keys and relay-shaped addresses still rejected. |
| Clocks | Exact u64 limits, now+60000 boundary/+1, rollback/restart, overflow, older forwarded writer | Exact unsigned parse; future skew rejects before any mutation; tuple tie uses device ID; old writer need not be sender/current member; clock never grants trust. |
| Control lanes | MEMBERS and PEER_SETTINGS in flight with files/PING; lost ACK; receipt eviction; pre-install immediate snapshot | One pending per control kind, shared unique IDs, correct receipt matching, keepalive responsive; exact pending snapshot first on retry. Buffer bounded before pairing persistence; no new handshake or fence refresh. |
| Revocation limits | A removed while B offline; C ACKs forwarded removal | B still trusts A until catch-up; C ACK proves C receipt only. UI never infers global completion or guaranteed reset of removed A. |

## Implementation estimate

Budget roughly **1,500–2,500 production Rust lines** plus **1,200–2,000 test
lines** for codec/schema, durable roster/receipts/reconcile, core/mesh admission
publication, scope migration and IPC/UI wiring. Android is roughly
**1,700–2,800 production Kotlin lines** plus **1,300–2,200 test lines** for the
same feature, additional DataStore/Room recovery and phone-only roster metadata.
These are planning ranges, excluding future relay, group merging, broad visual
redesign, native packaging and physical multi-device acceptance. The largest
risk is lifecycle integration, not the new codec.
