//! Setup state has its own lifetime. Neither direct-peer cascades nor receipt
//! pruning can erase a terminal removal or a retired incarnation's high water.
use super::*;
use pneu_proto::{
    DeviceRole, Member, MemberReadmits, MemberStatus, Members, SetupHello, SetupScope,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SetupState {
    pub setup_id: Option<Id>,
    pub generation: u64,
    pub withdrawn: bool,
    pub roster: Vec<Member>,
    pub links: Vec<SetupLink>,
    pub readmissions: Vec<Readmission>,
    pub pending_link: bool,
    pub waiting_for: Option<Id>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SetupLink {
    pub device: Id,
    pub scope: SetupScope,
    pub prompt: bool,
    pub introduced_by: Option<Id>,
    pub introducer_name: Option<String>,
    pub outcome: Option<String>,
    pub negotiated: bool,
    // Reciprocal authenticated consent, or an introduced binding before its
    // first HELLO. Capability support alone never grants setup authority.
    pub participating: bool,
    pub hello_seen: bool,
}
impl SetupLink {
    fn participates(&self) -> bool {
        self.scope == SetupScope::Enabled && self.negotiated && self.participating
    }
}
#[derive(Clone)]
pub(crate) struct SetupFence {
    pub generation: u64,
    pub binding_generation: u64,
    pub device: Id,
    pub fingerprint: Hash,
    pub connection: u64,
    pub current: Arc<AtomicU64>,
}
impl SetupFence {
    fn check(&self, c: &Connection) -> Result<()> {
        ensure!(
            self.current.load(Ordering::SeqCst) == self.connection,
            PairingRevoked
        );
        ensure!(state(c)?.generation == self.generation, PairingRevoked);
        let valid: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE id=? AND fp=? AND generation=?)",
            params![
                self.device.as_str(),
                self.fingerprint.as_str(),
                self.binding_generation as i64
            ],
            |r| r.get(0),
        )?;
        ensure!(valid, PairingRevoked);
        Ok(())
    }
}
#[derive(Default)]
pub(crate) struct SetupDelta {
    pub added: Vec<Id>,
    pub removed: Vec<Id>,
    pub tombstones: Vec<(Id, Hash)>,
    pub(super) settings: Vec<(Id, Vec<pneu_proto::PeerSetting>)>,
    pub reconnect: bool,
    pub reconnect_peer: Option<Id>,
    pub withdrawn: bool,
    pub changed: bool,
    pub roster_changed: bool,
    remote_scope: Option<(Id, SetupScope)>,
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum MembersError {
    #[error("setup is not enabled or IDs differ")]
    Unsupported,
    #[error("malformed membership state")]
    Malformed,
    #[error("removed sender")]
    Denied,
    #[error("setup capacity exceeded")]
    Capacity,
}
/// Local action authority: the exact key, removal tuple and setup incarnation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadmissionKey {
    pub device: Id,
    pub fingerprint: Hash,
    pub removal_hlc: u64,
    pub removal_writer: Id,
    pub setup_generation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Readmission {
    pub key: ReadmissionKey,
    pub removed_name: String,
    pub value: Member,
    pub introducer: Id,
    pub introducer_name: String,
    pub decision: String,
    #[serde(default)]
    pub error: Option<String>,
}
fn merge_member(a: &Member, b: &Member) -> Member {
    let mut value = if stamp(a) >= stamp(b) {
        a.clone()
    } else {
        b.clone()
    };
    value.reach = match (&a.reach, &b.reach) {
        (Some(a), Some(b))
            if (a.hlc, a.writer_device.as_str()) < (b.hlc, b.writer_device.as_str()) =>
        {
            Some(b.clone())
        }
        (Some(a), _) => Some(a.clone()),
        (_, b) => b.clone(),
    };
    value.readmits = match (&a.readmits, &b.readmits) {
        (Some(a), Some(b)) if claim_stamp(a) < claim_stamp(b) => Some(b.clone()),
        (Some(a), _) => Some(a.clone()),
        (_, b) => b.clone(),
    };
    value
}
fn claim_stamp(claim: &MemberReadmits) -> (u64, &str) {
    (claim.hlc, claim.writer_device.as_str())
}
fn claims(member: &Member, removal: &Member) -> bool {
    member
        .readmits
        .as_ref()
        .is_some_and(|c| claim_stamp(c) >= stamp(removal))
}
fn removal_claim(removal: &Member) -> MemberReadmits {
    MemberReadmits {
        hlc: removal.hlc,
        writer_device: removal.writer_device.clone(),
    }
}
fn legacy_authored() -> bool {
    true
}
fn fresh_pair_claim(entry: &Entry) -> Option<MemberReadmits> {
    let retained = entry.value.readmits.clone();
    let removal = entry.removal.as_ref().map(removal_claim);
    match (retained, removal) {
        (Some(a), Some(b)) if claim_stamp(&a) >= claim_stamp(&b) => Some(a),
        (_, Some(b)) => Some(b),
        (a, None) => a,
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    value: Member,
    #[serde(default)]
    readmission: Option<Readmission>,
    #[serde(default)]
    ready_generation: Option<u64>,
    // Local admission stamp: metadata edits must not move the notice baseline.
    #[serde(default)]
    admitted_hlc: Option<u64>,
    // Freeze import classification before deferred candidates can be renamed.
    #[serde(default)]
    joined_notice: Option<bool>,
    removal: Option<Member>,
    denied: bool,
    // Local barrier episode facts, never inferred from the unsigned writer.
    #[serde(default = "legacy_authored")]
    authored: bool,
    #[serde(default)]
    revoked_binding: bool,
    setup_id: Option<Id>,
}
// Called only from the direct READY path. Phones do not use the computer's
// reverse-dial/revocation evidence bit. Keep this local marker out of MEMBERS.
fn phone_ready(tx: &Transaction<'_>, fence: &SetupFence) -> Result<()> {
    tx.execute("UPDATE setup_entries SET payload=json_set(payload,'$.ready_generation',?) WHERE device=? AND fingerprint=?",params![fence.binding_generation as i64,fence.device.as_str(),fence.fingerprint.as_str()])?;
    Ok(())
}
fn stamp(m: &Member) -> (u64, &str) {
    (m.hlc, m.writer_device.as_str())
}
fn membership_eq(a: &Member, b: &Member) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.reach = None;
    b.reach = None;
    a.readmits = None;
    b.readmits = None;
    a == b
}
fn scope_text(scope: SetupScope) -> &'static str {
    match scope {
        SetupScope::Enabled => "enabled",
        SetupScope::Disabled => "disabled",
        SetupScope::Ask => "ask",
    }
}
fn entries(c: &Connection) -> Result<Vec<Entry>> {
    let mut q = c.prepare("SELECT payload FROM setup_entries ORDER BY device,fingerprint")?;
    q.query_map([], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}
fn save_entry(tx: &Transaction<'_>, entry: &Entry) -> Result<()> {
    tx.execute("INSERT INTO setup_entries VALUES(?,?,?) ON CONFLICT(device,fingerprint) DO UPDATE SET payload=excluded.payload",params![entry.value.device_id.as_str(),entry.value.fingerprint.as_str(),serde_json::to_string(entry)?])?;
    Ok(())
}
pub(super) fn state(c: &Connection) -> Result<SetupState> {
    let (id, generation, withdrawn): (Option<String>, i64, bool) = c.query_row(
        "SELECT setup_id,generation,withdrawn FROM setup_state WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let setup_id = id.map(Id::new).transpose()?;
    let roster = entries(c)?
        .into_iter()
        .filter(|e| setup_id.is_some() && e.setup_id == setup_id)
        .map(|e| e.value)
        .collect();
    let mut q=c.prepare("SELECT device,scope,(coalesce(prompt AND prompt_generation=(SELECT generation FROM setup_state) AND prompt_binding=(SELECT generation FROM peers WHERE id=device),0)),introduced_by,introducer_name,outcome,negotiated,participating,last_connection!=0 FROM setup_links ORDER BY device")?;
    let links = q
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        })?
        .map(|r| {
            let (device, scope, prompt, by, name, outcome, negotiated, participating, hello_seen) =
                r?;
            Ok(SetupLink {
                device: Id::new(device)?,
                scope: SetupScope::try_from(scope)?,
                prompt,
                introduced_by: by.map(Id::new).transpose()?,
                introducer_name: name,
                outcome,
                negotiated,
                participating,
                hello_seen,
            })
        })
        .collect::<Result<_>>()?;
    Ok(SetupState {
        setup_id: setup_id.clone(),
        generation: generation as u64,
        withdrawn,
        roster,
        links,
        readmissions: entries(c)?.into_iter().filter(|e| e.denied && e.authored).filter_map(|e| e.readmission.filter(|r| e.removal.as_ref().is_some_and(|removal| claims(&r.value, removal)))).filter(|r| r.decision == "pending" && r.key.setup_generation == generation as u64 && setup_id.is_some() && !withdrawn).collect(),
        pending_link: c.query_row("SELECT EXISTS(SELECT 1 FROM setup_pending_link)", [], |r| r.get(0))?,
        waiting_for: c.query_row("SELECT device FROM setup_links WHERE discovery_id IS NOT NULL ORDER BY device LIMIT 1", [], |r| r.get::<_, String>(0)).optional()?.map(Id::new).transpose()?,
    })
}
pub(super) fn migrate(tx: &Transaction<'_>) -> Result<SetupState> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS setup_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),setup_id TEXT,generation INTEGER NOT NULL,withdrawn INTEGER NOT NULL DEFAULT 0);
        INSERT OR IGNORE INTO setup_state VALUES(1,NULL,0,0);
        CREATE TABLE IF NOT EXISTS setup_links(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,scope TEXT NOT NULL CHECK(scope IN ('enabled','disabled','ask')),prompt INTEGER NOT NULL DEFAULT 0,prompt_generation INTEGER,prompt_binding INTEGER,introduced_by TEXT,introducer_name TEXT,outcome TEXT);
        CREATE TABLE IF NOT EXISTS setup_entries(device TEXT NOT NULL,fingerprint TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(device,fingerprint));
        CREATE TABLE IF NOT EXISTS members_receipts(device TEXT NOT NULL  ,id TEXT NOT NULL,generation INTEGER NOT NULL,meta TEXT NOT NULL,PRIMARY KEY(device,id));
        CREATE TABLE IF NOT EXISTS members_outgoing(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,generation INTEGER NOT NULL,payload TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS members_replay(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,generation INTEGER NOT NULL,payload TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS setup_conflicts(device TEXT NOT NULL,fingerprint TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(device,fingerprint));
        CREATE TABLE IF NOT EXISTS setup_retired(setup_id TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS setup_withdrawn_pins(fingerprint TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS setup_candidates(device TEXT NOT NULL,fingerprint TEXT NOT NULL,introduced_by TEXT NOT NULL,introducer_name TEXT NOT NULL,PRIMARY KEY(device,fingerprint));
        CREATE TABLE IF NOT EXISTS setup_notices(device TEXT NOT NULL,generation INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(device,generation));")?;
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 4 {
        tx.pragma_update(None, "user_version", 4)?;
    }
    let has_routes: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('setup_links') WHERE name='route_proven')",
        [],
        |r| r.get(0),
    )?;
    if !has_routes {
        tx.execute_batch("ALTER TABLE setup_links ADD COLUMN route_proven INTEGER NOT NULL DEFAULT 0 CHECK(route_proven IN (0,1));")?;
    }
    for (name, definition) in [
        ("token_founding", "INTEGER NOT NULL DEFAULT 0"),
        ("negotiated", "INTEGER NOT NULL DEFAULT 0"),
        ("participating", "INTEGER NOT NULL DEFAULT 0"),
        ("last_connection", "INTEGER NOT NULL DEFAULT 0"),
        ("allow_connection", "INTEGER"),
        ("discovery_connection", "INTEGER"),
        ("discovery_id", "TEXT"),
    ] {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('setup_links') WHERE name=?)",
            [name],
            |r| r.get(0),
        )?;
        if !exists {
            tx.execute_batch(&format!(
                "ALTER TABLE setup_links ADD COLUMN {name} {definition};"
            ))?;
        }
    }
    tx.execute_batch("CREATE TABLE IF NOT EXISTS setup_pending_link(singleton INTEGER PRIMARY KEY CHECK(singleton=1),deadline INTEGER NOT NULL);")?;
    // A surviving action starts a fresh discovery window on every open.
    tx.execute(
        "UPDATE setup_pending_link SET deadline=?",
        [crate::now_ms().saturating_add(3000) as i64],
    )?;
    tx.execute("UPDATE setup_links SET last_connection=0,allow_connection=CASE WHEN allow_connection IS NOT NULL THEN 0 END,discovery_id=NULL,discovery_connection=CASE WHEN discovery_connection IS NOT NULL THEN 0 END", [])?;
    // Convert one bounded wire snapshot at a time, including pre-delta databases.
    loop {
        let old: Option<(i64,String)>=tx.query_row("SELECT rowid,meta FROM members_receipts WHERE json_extract(meta,'$.operation')='Members' LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((row, raw)) = old else {
            break;
        };
        if let Metadata::Members(m) = serde_json::from_str(&raw)? {
            tx.execute(
                "UPDATE members_receipts SET meta=? WHERE rowid=?",
                params![serde_json::to_string(&members_metadata(&m))?, row],
            )?;
        }
    }
    // Preserve the old cache's first-route semantics only for migrated peers.
    // Every new binding starts with unverified hints until an actual dial succeeds.
    tx.execute("INSERT OR IGNORE INTO setup_links(device,scope,route_proven) SELECT id,'ask',json_array_length(addresses)>0 FROM peers",[])?;
    // Notices belong to binding incarnations, including ordinary replacement.
    tx.execute_batch("CREATE TRIGGER IF NOT EXISTS setup_notice_binding_end AFTER DELETE ON peers BEGIN DELETE FROM setup_notices WHERE device=OLD.id; END;
        DELETE FROM setup_notices WHERE NOT EXISTS(SELECT 1 FROM peers WHERE id=device AND peers.generation=setup_notices.generation);
        DELETE FROM setup_conflicts WHERE rowid NOT IN (SELECT max(rowid) FROM setup_conflicts GROUP BY device);
        DELETE FROM setup_conflicts WHERE rowid IN (SELECT rowid FROM setup_conflicts ORDER BY rowid DESC LIMIT -1 OFFSET 64);")?;
    clear_resolved_conflicts(tx)?;
    state(tx)
}
pub(super) fn route_proven(c: &Connection, id: &Id) -> Result<bool> {
    Ok(c.query_row(
        "SELECT coalesce((SELECT route_proven FROM setup_links WHERE device=?),0)",
        [id.as_str()],
        |r| r.get(0),
    )?)
}
fn set_scope(tx: &Transaction<'_>, peer: &Id, scope: SetupScope) -> Result<()> {
    tx.execute("INSERT INTO setup_links(device,scope) VALUES(?,?) ON CONFLICT(device) DO UPDATE SET scope=excluded.scope,prompt=0,outcome=NULL",params![peer.as_str(),scope_text(scope)])?;
    Ok(())
}
fn denied(c: &Connection, id: &Id, pin: &Hash) -> Result<bool> {
    Ok(entries(c)?
        .iter()
        .any(|e| e.value.device_id == *id && e.value.fingerprint == *pin && e.denied))
}
fn validate_capacity(c: &Connection) -> Result<()> {
    let all = entries(c)?;
    ensure!(all.len() <= 64, MembersError::Capacity);
    let mut per_id = HashMap::new();
    for e in &all {
        let count = per_id.entry(&e.value.device_id).or_insert(0);
        *count += 1;
        ensure!(*count <= 4, MembersError::Capacity);
    }
    let s = state(c)?;
    if let Some(id) = s.setup_id {
        let m = Members {
            transfer_id: Id::from_bytes([0; 16]),
            setup_id: id,
            members: s.roster,
        };
        pneu_proto::encode_frame(&pneu_proto::Message::Members(m))
            .map_err(|_| MembersError::Capacity)?;
    }
    let count: i64 = c.query_row("SELECT count(*) FROM peers", [], |r| r.get(0))?;
    ensure!(count <= 16, MembersError::Capacity);
    Ok(())
}
fn forget(tx: &Transaction<'_>, id: &Id, pin: &Hash, delta: &mut SetupDelta) -> Result<()> {
    if tx.execute(
        "DELETE FROM peers WHERE id=? AND fp=?",
        params![id.as_str(), pin.as_str()],
    )? != 0
    {
        tx.execute(
            "UPDATE peer_generations SET generation=generation+1 WHERE device=?",
            [id.as_str()],
        )?;
        tx.execute("DELETE FROM members_receipts WHERE device=?", [id.as_str()])?;
        delta.removed.push(id.clone());
    }
    Ok(())
}
fn retire(tx: &Transaction<'_>, withdraw: bool, delta: &mut SetupDelta) -> Result<()> {
    if withdraw {
        delta.withdrawn = true;
        let peers: Vec<(Id, Hash)> = {
            let mut q = tx.prepare(
                "SELECT id,fp FROM peers JOIN setup_links ON device=id WHERE scope='enabled' AND negotiated=1 AND participating=1",
            )?;
            q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .map(|r| {
                    let (id, pin) = r?;
                    Ok((Id::new(id)?, Hash::new(pin)?))
                })
                .collect::<Result<_>>()?
        };
        let roster = state(tx)?.roster;
        // Only the latest retired setup, bounded by the 16 direct bindings.
        tx.execute("DELETE FROM setup_withdrawn_pins", [])?;
        for (id, pin) in peers {
            // Local opt-in alone is not a scoped link: the remote may have
            // chosen off, or be legacy. Keep those direct-only bindings.
            if roster.iter().any(|m| {
                m.device_id == id && m.fingerprint == pin && m.status == MemberStatus::Member
            }) {
                tx.execute(
                    "INSERT OR IGNORE INTO setup_withdrawn_pins VALUES(?)",
                    [pin.as_str()],
                )?;
                forget(tx, &id, &pin, delta)?;
            }
        }
    }
    for mut e in entries(tx)? {
        if let Some(removal) = e.removal.clone() {
            if e.readmission
                .as_ref()
                .is_some_and(|r| r.decision == "pending")
            {
                e.readmission = None;
            }
            e.value = removal;
            e.setup_id = None;
            save_entry(tx, &e)?;
        } else {
            tx.execute(
                "DELETE FROM setup_entries WHERE device=? AND fingerprint=?",
                params![e.value.device_id.as_str(), e.value.fingerprint.as_str()],
            )?;
        }
    }
    tx.execute("INSERT OR IGNORE INTO setup_retired SELECT setup_id FROM setup_state WHERE setup_id IS NOT NULL",[])?;
    tx.execute("UPDATE setup_state SET setup_id=NULL,generation=generation+1,withdrawn=? WHERE singleton=1",[withdraw])?;
    tx.execute_batch("UPDATE setup_links SET prompt=0,prompt_generation=NULL,prompt_binding=NULL;
        DELETE FROM members_outgoing; DELETE FROM members_replay; DELETE FROM members_receipts; DELETE FROM setup_conflicts; DELETE FROM setup_candidates; DELETE FROM setup_pending_link; UPDATE setup_links SET discovery_connection=NULL,discovery_id=NULL,allow_connection=NULL;")?;
    delta.reconnect = true;
    Ok(())
}
fn dissolve_if_last(tx: &Transaction<'_>, local: &Id, delta: &mut SetupDelta) -> Result<()> {
    let s = state(tx)?;
    if s.setup_id.is_some()
        && !s
            .roster
            .iter()
            .any(|m| m.device_id != *local && m.status == MemberStatus::Member)
    {
        retire(tx, false, delta)?;
    }
    Ok(())
}
// One latest diagnostic per device and at most the roster's 64-entry budget.
fn record_conflict(tx: &Transaction<'_>, value: &Member) -> Result<()> {
    tx.execute(
        "DELETE FROM setup_conflicts WHERE device=?",
        [value.device_id.as_str()],
    )?;
    tx.execute(
        "INSERT INTO setup_conflicts VALUES(?,?,?)",
        params![
            value.device_id.as_str(),
            value.fingerprint.as_str(),
            serde_json::to_string(value)?
        ],
    )?;
    tx.execute("DELETE FROM setup_conflicts WHERE rowid IN (SELECT rowid FROM setup_conflicts ORDER BY rowid DESC LIMIT -1 OFFSET 64)", [])?;
    Ok(())
}
fn clear_resolved_conflicts(tx: &Transaction<'_>) -> Result<()> {
    let values: Vec<Member> = {
        let mut q = tx.prepare("SELECT payload FROM setup_conflicts")?;
        q.query_map([], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect::<Result<_>>()?
    };
    let retained = entries(tx)?;
    for m in values {
        let conflict: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE (id=? AND fp!=?) OR (id!=? AND fp=?))",
            params![
                m.device_id.as_str(),
                m.fingerprint.as_str(),
                m.device_id.as_str(),
                m.fingerprint.as_str()
            ],
            |r| r.get(0),
        )?;
        let active_conflict = retained.iter().any(|e| {
            e.value.status == MemberStatus::Member
                && ((e.value.device_id == m.device_id && e.value.fingerprint != m.fingerprint)
                    || (e.value.device_id != m.device_id && e.value.fingerprint == m.fingerprint))
        });
        let different_denied = retained.iter().any(|e| {
            e.denied && e.value.device_id == m.device_id && e.value.fingerprint != m.fingerprint
        });
        let removed = retained.iter().any(|e| {
            e.denied && e.value.device_id == m.device_id && e.value.fingerprint == m.fingerprint
        });
        let bound: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE id=? AND fp=?)",
            params![m.device_id.as_str(), m.fingerprint.as_str()],
            |r| r.get(0),
        )?;
        if bound || removed || (!conflict && !active_conflict && !different_denied) {
            tx.execute(
                "DELETE FROM setup_conflicts WHERE device=?",
                [m.device_id.as_str()],
            )?;
        }
    }
    Ok(())
}
fn local_value(tx: &Transaction<'_>, mut value: Member, setup_id: &Id, fresh: bool) -> Result<()> {
    let old = entries(tx)?
        .into_iter()
        .find(|e| e.value.device_id == value.device_id && e.value.fingerprint == value.fingerprint);
    if let Some(e) = &old {
        if e.denied && !fresh {
            return Ok(());
        }
        if !fresh && e.setup_id.as_ref() == Some(setup_id) && e.value.status == MemberStatus::Member
        {
            return Ok(());
        }
    }
    if entries(tx)?.iter().any(|e| {
        e.setup_id.as_ref() == Some(setup_id)
            && e.value.status == MemberStatus::Member
            && e.value.device_id != value.device_id
            && e.value.fingerprint == value.fingerprint
    }) {
        record_conflict(tx, &value)?;
        return Ok(());
    }
    value.hlc = allocate_stamp(tx)?.1;
    value.readmits = if fresh {
        old.as_ref().and_then(fresh_pair_claim)
    } else {
        old.as_ref().and_then(|e| e.value.readmits.clone())
    };
    if let Some(reach) = &mut value.reach {
        reach.hlc = value.hlc;
    }
    save_entry(
        tx,
        &Entry {
            readmission: if fresh {
                None
            } else {
                old.as_ref().and_then(|e| e.readmission.clone())
            },
            ready_generation: if fresh {
                None
            } else {
                old.as_ref().and_then(|e| e.ready_generation)
            },
            admitted_hlc: Some(value.hlc),
            joined_notice: None,
            value,
            removal: old.and_then(|e| e.removal),
            denied: false,
            authored: false,
            revoked_binding: false,
            setup_id: Some(setup_id.clone()),
        },
    )
}
// The self entry is authored locally on assignment, before the first MEMBERS.
// Keep that original admission stamp even when a rename advances its value HLC.
fn joined_is_new(c: &Connection, local: &Id, member: &Member) -> Result<bool> {
    // A member can opt into linking after this computer's admission, especially
    // during an upgrade. An existing pairing is already known to its owner.
    let already_paired: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM peers WHERE id=? AND fp=?)",
        params![member.device_id.as_str(), member.fingerprint.as_str()],
        |r| r.get(0),
    )?;
    if already_paired {
        return Ok(false);
    }
    let baseline = entries(c)?
        .into_iter()
        .find(|e| {
            e.value.device_id == *local && e.value.status == MemberStatus::Member && !e.denied
        })
        .map(|e| e.admitted_hlc.unwrap_or(e.value.hlc))
        .unwrap_or(0);
    Ok(member.hlc >= baseline)
}
pub(super) fn history_devices(c: &Connection) -> Result<HashMap<Id, Id>> {
    let mut q = c.prepare("SELECT old_device,new_device FROM history_device_replacements")?;
    q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .map(|r| {
            let (old, new) = r?;
            Ok((Id::new(old)?, Id::new(new)?))
        })
        .collect()
}
/// Display attribution only: never migrate receipts, delivery authority or queued work.
pub(super) fn replace_history_device(tx: &Transaction<'_>, old: &Id, new: &Id) -> Result<()> {
    tx.execute(
        "UPDATE history_device_replacements SET new_device=? WHERE new_device=?",
        params![new.as_str(), old.as_str()],
    )?;
    tx.execute("INSERT INTO history_device_replacements VALUES(?,?) ON CONFLICT(old_device) DO UPDATE SET new_device=excluded.new_device", params![old.as_str(), new.as_str()])?;
    Ok(())
}
impl Store {
    pub(crate) fn attribute_finished_transfers(&self, rows: &mut [crate::outbox::Transfer]) {
        let devices = self.history_devices.lock().expect("history devices");
        for row in rows {
            if matches!(row.state.as_str(), "sent" | "received")
                && let Some(device) = devices.get(&row.device)
            {
                row.device = device.clone();
            }
        }
    }

    /// A known former member can race withdrawal with a reconnect. Refuse it
    /// normally, but do not invite the owner to pair an apparent stranger.
    pub(crate) async fn setup_quiet_reconnect(&self, pin: Hash) -> Result<bool> {
        self.call(move |c| {
            let withdrawn: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM setup_withdrawn_pins WHERE fingerprint=?)",
                [pin.as_str()],
                |r| r.get(0),
            )?;
            Ok(withdrawn
                || entries(c)?
                    .iter()
                    .any(|entry| entry.denied && entry.value.fingerprint == pin))
        })
        .await
    }

    /// Every externally visible peer/cache change is published after COMMIT in
    /// this blocking continuation. Canceling its async caller cannot skip it.
    pub(super) async fn setup_write<T: Send + 'static>(
        &self,
        edit: impl FnOnce(&Transaction<'_>, &mut SetupDelta) -> Result<T> + Send + 'static,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<T> {
        self.blocking(move |s| {
            s.locked(|c| {
                let tx = c.transaction()?;
                let mut delta = SetupDelta::default();
                let result = edit(&tx, &mut delta)?;
                clear_resolved_conflicts(&tx)?;
                #[cfg(test)]
                s.fault("members_before_commit")?;
                #[cfg(test)]
                if std::env::var_os("PNEU_SETUP_CRASH_BEFORE_COMMIT").is_some() {
                    std::process::exit(0);
                }
                let setup = state(&tx)?;
                let generations = {
                    let mut q = tx.prepare("SELECT device,generation FROM peer_generations")?;
                    q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                        .map(|r| {
                            let (id, g) = r?;
                            Ok((Id::new(id)?, g as u64))
                        })
                        .collect::<Result<HashMap<_, _>>>()?
                };
                let replacements = replacement::pending(&tx)?;
                let history_devices = history_devices(&tx)?;
                // Lock order matches the ordinary lifecycle (DB -> caches).
                let mut cached = s.setup.lock().expect("setup");
                let mut epochs = s.generations.lock().expect("peer generations");
                let mut policy = s.pair_settings.lock().expect("pair settings");
                let mut remote_scopes = s.setup_remote_scopes.lock().expect("remote setup scopes");
                let mut offers = s.replacements.lock().expect("replacement offers");
                let settings_changed = !delta.settings.is_empty() || *offers != replacements;
                let mut history = s.history_devices.lock().expect("history devices");
                tx.commit()?;
                *history = history_devices;
                drop(history);
                for (id, fields) in delta.settings.drain(..) {
                    policy.insert(id.clone(), fields);
                    let mut revisions =
                        s.settings_generations.lock().expect("settings generations");
                    let revision = revisions.entry(id).or_default();
                    *revision = revision.wrapping_add(1);
                }
                *offers = replacements;
                drop(offers);
                if settings_changed {
                    s.settings_changed.send_modify(|n| *n = n.wrapping_add(1));
                }
                let previous_scopes = remote_scopes.clone();
                if let Some((device, scope)) = delta.remote_scope.take()
                    && let Some(generation) = generations.get(&device)
                {
                    remote_scopes.insert(device, (*generation, scope));
                }
                remote_scopes.retain(|device, (generation, _)| {
                    generations.get(device) == Some(generation)
                        && setup
                            .links
                            .iter()
                            .any(|l| l.device == *device && l.negotiated)
                });
                let roster_changed = cached.as_ref().is_none_or(|old| {
                    old.roster != setup.roster
                        || old.setup_id != setup.setup_id
                        || old.generation != setup.generation
                });
                let changed = cached.as_ref() != Some(&setup) || *remote_scopes != previous_scopes;
                *cached = Some(setup);
                *epochs = generations;
                for id in &delta.removed {
                    policy.remove(id);
                    s.settings_generations
                        .lock()
                        .expect("settings generations")
                        .remove(id);
                }
                drop(remote_scopes);
                drop(policy);
                drop(epochs);
                drop(cached);
                delta.changed = changed;
                delta.roster_changed = roster_changed;
                finish(delta);
                if changed {
                    s.setup_changed.send_modify(|n| *n = n.wrapping_add(1));
                }
                Ok(result)
            })
            .inspect(|_| {
                let _ = s.purge_revoked_partials();
            })
        })
        .await
    }
    pub(crate) async fn setup_binding_ready(&self, device: Id) -> Result<bool> {
        let epoch = self.peer_epoch(&device);
        self.call(move |c| {
            epoch.check(c)?;
            Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM peers p WHERE p.id=? AND (p.established=1 OR EXISTS(SELECT 1 FROM setup_entries e WHERE e.device=p.id AND e.fingerprint=p.fp AND json_extract(e.payload,'$.ready_generation')=p.generation)))",[device.as_str()],|r|r.get(0))?)
        }).await
    }
    pub(crate) fn setup_state(&self) -> SetupState {
        self.setup
            .lock()
            .expect("setup")
            .as_ref()
            .expect("loaded setup")
            .clone()
    }
    pub(crate) fn setup_remote_scopes(&self) -> HashMap<Id, SetupScope> {
        let generations = self.generations.lock().expect("peer generations");
        self.setup_remote_scopes
            .lock()
            .expect("remote setup scopes")
            .iter()
            .filter(|(device, (generation, _))| generations.get(*device) == Some(generation))
            .map(|(device, (_, scope))| (device.clone(), *scope))
            .collect()
    }
    pub(crate) fn setup_hello(&self, device: Option<&Id>, default: SetupScope) -> SetupHello {
        let s = self.setup_state();
        let scope = device
            .and_then(|id| s.links.iter().find(|l| &l.device == id))
            .map(|l| l.scope)
            .unwrap_or(default);
        SetupHello {
            scope,
            setup_id: if scope == SetupScope::Disabled {
                None
            } else {
                s.setup_id
            },
        }
    }
    // pairing_generation also retains revocation epochs. A confirmation must
    // capture the absence of an actual binding separately from that history.
    pub(crate) fn setup_binding_generation(&self, device: &Id) -> Option<u64> {
        self.setup_state()
            .links
            .iter()
            .any(|l| l.device == *device)
            .then(|| self.pairing_generation(device))
            .flatten()
    }
    pub(crate) fn setup_protected(&self, device: &Id) -> bool {
        self.setup_state()
            .links
            .iter()
            .any(|l| l.device == *device && (l.participates() || l.introduced_by.is_some()))
    }
    pub(crate) fn setup_scoped(&self, device: &Id) -> bool {
        let s = self.setup_state();
        s.links
            .iter()
            .any(|l| l.device == *device && l.participates())
            && s.roster
                .iter()
                .any(|m| m.device_id == *device && m.status == MemberStatus::Member)
    }
    #[cfg(test)]
    pub(crate) async fn setup_link(&self, peer: Id, scope: SetupScope) -> Result<()> {
        self.setup_link_then(peer, scope, |_| {}).await
    }
    #[cfg(test)]
    pub(crate) async fn setup_link_then(
        &self,
        peer: Id,
        scope: SetupScope,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        let epoch = self.peer_epoch(&peer);
        self.setup_write(
            move |tx, _| {
                epoch.check(tx)?;
                ensure!(
                    !state(tx)?.withdrawn,
                    "fresh token pairing required after withdrawal"
                );
                set_scope(tx, &peer, scope)?;
                tx.execute(
                    "UPDATE setup_links SET token_founding=0 WHERE device=?",
                    [peer.as_str()],
                )?;
                Ok(())
            },
            finish,
        )
        .await
    }
    #[cfg(test)]
    pub(crate) async fn setup_prompt(&self, fence: SetupFence, remote: SetupHello) -> Result<()> {
        let epoch = self.peer_epoch(&fence.device);
        self.setup_write(move |tx,_| {
            epoch.check(tx)?;fence.check(tx)?;
            let s=state(tx)?;
            let link=s.links.iter().find(|l|l.device==fence.device).context("missing setup link")?;
            if s.setup_id.is_some()&&remote.setup_id.is_some()&&s.setup_id!=remote.setup_id {
                set_scope(tx,&fence.device,SetupScope::Disabled)?;
                tx.execute("UPDATE setup_links SET outcome='Direct link kept; separate setups not linked' WHERE device=?",[fence.device.as_str()])?;
            } else if !s.withdrawn&&link.scope==SetupScope::Ask&&remote.scope==SetupScope::Enabled {
                tx.execute("UPDATE setup_links SET prompt=1,prompt_generation=?,prompt_binding=? WHERE device=? AND NOT (prompt_generation IS ? AND prompt_binding IS ?)",params![s.generation as i64,epoch.generation as i64,fence.device.as_str(),s.generation as i64,epoch.generation as i64])?;
            }
            Ok(())
        },|_|{}).await
    }
    #[cfg(test)]
    pub(crate) async fn setup_decide(
        &self,
        peer: Id,
        generation: u64,
        binding: u64,
        allow: bool,
    ) -> Result<()> {
        self.setup_decide_then(peer, generation, binding, allow, None, |_| {})
            .await
    }
    pub(crate) async fn setup_decide_then(
        &self,
        peer: Id,
        generation: u64,
        binding: u64,
        allow: bool,
        refresh_connection: Option<u64>,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        let epoch = self.peer_epoch(&peer);
        self.setup_write(move |tx,_| {
            epoch.check(tx)?;
            let s=state(tx)?;ensure!(s.generation==generation&&epoch.generation==binding,"stale setup prompt");
            let pending:bool=tx.query_row("SELECT prompt AND prompt_generation=? AND prompt_binding=? FROM setup_links WHERE device=?",params![generation as i64,binding as i64,peer.as_str()],|r|r.get(0))?;
            ensure!(pending,"stale setup prompt");
            // Allow is a request to refresh authenticated HELLO, not consent
            // based on the stale offered setup ID. Core completes it there.
            if allow {tx.execute("UPDATE setup_links SET outcome='allow_pending_refresh',allow_connection=coalesce(?,last_connection) WHERE device=?",params![refresh_connection.map(|v|v as i64),peer.as_str()])?;} else {set_scope(tx,&peer,SetupScope::Disabled)?;}
            Ok(())
        },finish).await
    }
    pub(crate) async fn setup_activate(
        &self,
        fence: SetupFence,
        local: Member,
        remote: Member,
        remote_hello: SetupHello,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<bool> {
        let epoch = self.peer_epoch(&fence.device);
        self.setup_write(move |tx,delta| {
            epoch.check(tx)?;fence.check(tx)?;
            delta.remote_scope=Some((fence.device.clone(),remote_hello.scope));
            let mut s=state(tx)?;
            let link=s.links.iter().find(|l|l.device==fence.device).context("missing setup link")?.clone();
            let (last, allow, discovery, token): (i64,Option<i64>,Option<i64>,bool)=tx.query_row("SELECT last_connection,allow_connection,discovery_connection,token_founding FROM setup_links WHERE device=?",[fence.device.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            let phone=remote.role==DeviceRole::Phone;
            let connection=fence.connection as i64;
            tx.execute("UPDATE setup_links SET last_connection=?,negotiated=1,participating=? WHERE device=?",params![connection,link.scope==SetupScope::Enabled && remote_hello.scope==SetupScope::Enabled,fence.device.as_str()])?;
            if phone { phone_ready(tx,&fence)?; }
            if discovery.is_some_and(|old|old!=connection) && remote_hello.scope!=SetupScope::Disabled {
                tx.execute("UPDATE setup_links SET discovery_id=coalesce(?,discovery_id) WHERE device=?",params![remote_hello.setup_id.as_ref().map(Id::as_str),fence.device.as_str()])?;
            }
            let _ = last;
            if link.scope==SetupScope::Disabled {return Ok(false);}
            if s.setup_id.is_some()&&remote_hello.setup_id.is_some()&&s.setup_id!=remote_hello.setup_id {
                set_scope(tx,&fence.device,SetupScope::Disabled)?;
                tx.execute("UPDATE setup_links SET outcome='Direct link kept; separate setups not linked' WHERE device=?",[fence.device.as_str()])?;return Ok(false);
            }
            let pending_generation:bool=tx.query_row("SELECT prompt_generation IS ? AND prompt_binding IS ? FROM setup_links WHERE device=?",params![s.generation as i64,epoch.generation as i64,fence.device.as_str()],|r|r.get(0))?;
            let allow_pending=link.outcome.as_deref()==Some("allow_pending_refresh")&&pending_generation;
            if link.outcome.as_deref()==Some("allow_pending_refresh")&&!pending_generation {tx.execute("UPDATE setup_links SET outcome=NULL,prompt=0 WHERE device=?",[fence.device.as_str()])?;}
            if !s.withdrawn&&link.scope==SetupScope::Ask&&remote_hello.scope==SetupScope::Enabled&&!allow_pending {
                tx.execute("UPDATE setup_links SET prompt=1,prompt_generation=?,prompt_binding=? WHERE device=? AND NOT (prompt_generation IS ? AND prompt_binding IS ?)",params![s.generation as i64,epoch.generation as i64,fence.device.as_str(),s.generation as i64,epoch.generation as i64])?;
            }
            if allow_pending&&allow!=Some(connection)&&remote_hello.scope==SetupScope::Enabled {
                enable_ask(tx)?;
                delta.reconnect=true;return Ok(false);
            }
            if link.scope!=SetupScope::Enabled||remote_hello.scope!=SetupScope::Enabled||s.withdrawn {return Ok(false);}
            ensure!(!denied(tx,&remote.device_id,&remote.fingerprint)?,MembersError::Denied);
            let matched=s.setup_id.is_some()&&s.setup_id==remote_hello.setup_id;
            if s.setup_id.is_none() {
                let id=match remote_hello.setup_id {
                    Some(id)=>id,
                    None if token && local.device_id.as_str()<remote.device_id.as_str()=>fresh_setup_id(tx)?,
                    None=>return Ok(false),
                };
                tx.execute("UPDATE setup_state SET setup_id=?,generation=generation+1 WHERE singleton=1",[id.as_str()])?;
                clear_pending_link(tx)?;s=state(tx)?;delta.reconnect=true;
            }
            let id=s.setup_id.context("setup assignment")?;
            local_value(tx,local,&id,false)?;
            local_value(tx,remote,&id,false)?;
            if phone { phone_ready(tx,&fence)?; }
            validate_capacity(tx)?;
            Ok(matched)
        },finish).await
    }
    pub(crate) async fn members_outgoing(&self, fence: SetupFence) -> Result<Members> {
        let epoch = self.peer_epoch(&fence.device);
        self.setup_write(move |tx,_| {
            epoch.check(tx)?;fence.check(tx)?;
            let s=state(tx)?;
            ensure!(s.links.iter().any(|l|l.device==fence.device&&l.scope==SetupScope::Enabled),MembersError::Unsupported);
            let id=s.setup_id.context("unassigned setup")?;
            if let Some(raw)=tx.query_row("SELECT payload FROM members_outgoing WHERE device=? AND generation=?",params![fence.device.as_str(),s.generation as i64],|r|r.get::<_,String>(0)).optional()? {return Ok(serde_json::from_str(&raw)?);}
            let replay=tx.query_row("SELECT payload FROM members_replay WHERE device=? AND generation=?",params![fence.device.as_str(),s.generation as i64],|r|r.get::<_,String>(0)).optional()?.map(|raw|serde_json::from_str::<Members>(&raw)).transpose()?;
            let snapshot=match replay {Some(m) if m.setup_id==id&&m.members==s.roster=>m,_=>Members {transfer_id:Id::new(crate::random_hex(16))?,setup_id:id,members:s.roster}};
            pneu_proto::encode_frame(&pneu_proto::Message::Members(snapshot.clone()))?;
            tx.execute("INSERT INTO members_outgoing VALUES(?,?,?) ON CONFLICT(device) DO UPDATE SET generation=excluded.generation,payload=excluded.payload",params![fence.device.as_str(),s.generation as i64,serde_json::to_string(&snapshot)?])?;
            Ok(snapshot)
        },|_|{}).await
    }
    pub(crate) async fn members_ack(&self, fence: SetupFence, id: Id) -> Result<()> {
        let epoch = self.peer_epoch(&fence.device);
        self.setup_write(move |tx,_| {
            epoch.check(tx)?;fence.check(tx)?;
            let raw:String=tx.query_row("SELECT payload FROM members_outgoing WHERE device=? AND generation=?",params![fence.device.as_str(),fence.generation as i64],|r|r.get(0))?;
            let snapshot:Members=serde_json::from_str(&raw)?;ensure!(snapshot.transfer_id==id,"wrong MEMBERS ACK");
            tx.execute("INSERT INTO members_replay VALUES(?,?,?) ON CONFLICT(device) DO UPDATE SET generation=excluded.generation,payload=excluded.payload",params![fence.device.as_str(),fence.generation as i64,raw])?;
            tx.execute("DELETE FROM members_outgoing WHERE device=?",[fence.device.as_str()])?;Ok(())
        },|_|{}).await
    }
}
impl Store {
    pub(crate) async fn members_merge(
        &self,
        fence: SetupFence,
        local: Member,
        snapshot: Members,
        blocked: Vec<(Option<Id>, Hash)>,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<bool> {
        self.members_merge_at(fence, local, snapshot, blocked, crate::now_ms(), finish)
            .await
    }
    pub(crate) async fn members_merge_at(
        &self,
        fence: SetupFence,
        local: Member,
        snapshot: Members,
        blocked: Vec<(Option<Id>, Hash)>,
        now: u64,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<bool> {
        let epoch = self.peer_epoch(&fence.device);
        self.setup_write(move |tx,delta| {
            epoch.check(tx)?;fence.check(tx)?;
            ensure!(!denied(tx,&fence.device,&fence.fingerprint)?,MembersError::Denied);
            let s=state(tx)?;
            ensure!(s.setup_id.as_ref()==Some(&snapshot.setup_id)&&s.links.iter().any(|l|l.device==fence.device&&l.scope==SetupScope::Enabled),MembersError::Unsupported);
            snapshot.validate().map_err(|_|MembersError::Malformed)?;
            let snapshot=snapshot.canonical();
            for m in &snapshot.members {
                validate_stamp(m.hlc,now).map_err(|_|MembersError::Malformed)?;
                if let Some(r)=&m.reach {validate_stamp(r.hlc,now).map_err(|_|MembersError::Malformed)?;}
                if let Some(c)=&m.readmits {validate_stamp(c.hlc,now).map_err(|_|MembersError::Malformed)?;}
            }
            reject_partial_id(tx,&fence.device,&snapshot.transfer_id)?;
            let meta=members_metadata(&snapshot);
            if let Some((old,_))=receipt_row(tx,fence.device.as_str(),snapshot.transfer_id.as_str())? {ensure!(old==meta,ReceiptConflict);return Ok(true);}
            let failed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM failed_cache WHERE device=? AND id=?)",params![fence.device.as_str(),snapshot.transfer_id.as_str()],|r|r.get(0))?;ensure!(!failed,ReceiptConflict);
            let sender_role:String=tx.query_row("SELECT role FROM peers WHERE id=?",[fence.device.as_str()],|r|r.get(0))?;
            ensure!(snapshot.members.iter().any(|m|m.status==MemberStatus::Member&&m.device_id==fence.device&&m.fingerprint==fence.fingerprint&&scope_role(m.role)==sender_role),MembersError::Malformed);
            for m in &snapshot.members {
                if m.status==MemberStatus::Member {
                    ensure!(m.device_id!=local.device_id||(m.fingerprint==local.fingerprint&&m.role==local.role),MembersError::Malformed);
                    ensure!(m.fingerprint!=local.fingerprint||m.device_id==local.device_id,MembersError::Malformed);
                    ensure!(m.device_id!=fence.device||(m.fingerprint==fence.fingerprint&&scope_role(m.role)==sender_role),MembersError::Malformed);
                    ensure!(m.fingerprint!=fence.fingerprint||m.device_id==fence.device,MembersError::Malformed);
                }
            }
            // Validate ties against both the current value and retained removal
            // metadata before terminal eligibility can suppress an introduction.
            let retained=entries(tx)?;
            for m in &snapshot.members {
                if let Some(old)=retained.iter().find(|e|e.value.device_id==m.device_id&&e.value.fingerprint==m.fingerprint) {
                    for v in std::iter::once(&old.value).chain(old.removal.iter()).chain(old.readmission.iter().map(|r| &r.value)) {
                        ensure!(stamp(v)!=stamp(m)||membership_eq(v,m),MembersError::Malformed);
                    }
                    for retained_reach in std::iter::once(&old.value).chain(old.readmission.iter().map(|r|&r.value)) {
                        if let (Some(a),Some(b))=(&retained_reach.reach,&m.reach) {ensure!((a.hlc,&a.writer_device)!=(b.hlc,&b.writer_device)||a==b,MembersError::Malformed);}
                    }
                }
            }
            // Stage every effective removal before any introduction: a new
            // pin must see same-snapshot denials regardless of canonical order.
            for m in snapshot.members.iter().filter(|m|m.status==MemberStatus::Removed)
                .chain(snapshot.members.iter().filter(|m|m.status==MemberStatus::Member)) {
                observe_hlc(tx,m.hlc)?;if let Some(r)=&m.reach {observe_hlc(tx,r.hlc)?;}
                if let Some(c)=&m.readmits {observe_hlc(tx,c.hlc)?;}
                // Receipt equality and validation use the complete wire row. Only
                // after that may a sender self claim be ignored.
                let mut incoming=m.clone();
                if incoming.device_id==fence.device {incoming.readmits=None;}
                let m=&incoming;
                let old=entries(tx)?.into_iter().find(|e|e.value.device_id==m.device_id&&e.value.fingerprint==m.fingerprint);
                if m.status==MemberStatus::Removed {
                    let removal=old.as_ref().and_then(|e|e.removal.as_ref());
                    if removal.is_some_and(|r|stamp(r)>=stamp(m)) {continue;}
                    if let Some(mut e)=old.clone().filter(|e| !e.denied && e.value.status==MemberStatus::Member && claims(&e.value,m)) {
                        e.removal=Some(m.clone());
                        e.authored=false;e.revoked_binding=false;
                        save_entry(tx,&e)?;
                        continue;
                    }
                    let bound:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM peers WHERE id=? AND fp=?)",params![m.device_id.as_str(),m.fingerprint.as_str()],|r|r.get(0))?;
                    let episode=old.as_ref().filter(|e|e.denied);
                    let revoked_binding=bound || old.as_ref().is_some_and(|e| e.value.status==MemberStatus::Member && e.setup_id==s.setup_id) || episode.is_some_and(|e|e.revoked_binding);
                    let e=Entry {authored:episode.is_some_and(|e|e.authored),revoked_binding,ready_generation:None,readmission:None,value:m.clone(),admitted_hlc:None,joined_notice:None,removal:Some(m.clone()),denied:true,setup_id:Some(snapshot.setup_id.clone())};
                    save_entry(tx,&e)?;
                    delta.tombstones.push((m.device_id.clone(),m.fingerprint.clone()));
                    forget(tx,&m.device_id,&m.fingerprint,delta)?;
                    continue;
                }
                if m.device_id==local.device_id {
                    if let Some(mut entry)=old.filter(|e|!e.denied && e.value.status==MemberStatus::Member) {
                        entry.value.readmits=merge_member(&entry.value,m).readmits;
                        save_entry(tx,&entry)?;
                    }
                    continue;
                }
                if let Some(mut entry) = old.clone().filter(|e| e.denied) {
                    let r = entry.removal.as_ref().context("deny without removal")?;
                    let candidate=entry.readmission.as_ref().map(|record|merge_member(&record.value,m)).unwrap_or_else(||m.clone());
                    if claims(&candidate,r) {
                        if !entry.authored && entry.joined_notice.is_none() {
                            entry.joined_notice=Some(joined_is_new(tx,&local.device_id,&candidate)?);
                        }
                        if let Some(record) = &mut entry.readmission {
                            record.value = candidate.clone();
                        } else {
                            let name: String = tx.query_row("SELECT name FROM peers WHERE id=?", [fence.device.as_str()], |r| r.get(0))?;
                            entry.readmission = Some(Readmission {
                                key: ReadmissionKey { device: m.device_id.clone(), fingerprint: m.fingerprint.clone(), removal_hlc: r.hlc, removal_writer: r.writer_device.clone(), setup_generation: s.generation },
                                removed_name: r.name.clone(), value: candidate, introducer: fence.device.clone(), introducer_name: name, decision: "pending".into(), error: None,
                            });
                        }
                        save_entry(tx, &entry)?;
                    }
                    continue;
                }
                let already_bound:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM peers WHERE id=? AND fp=?)",params![m.device_id.as_str(),m.fingerprint.as_str()],|r|r.get(0))?;
                if !already_bound && entries(tx)?.iter().any(|e| e.denied && e.value.device_id == m.device_id && e.value.fingerprint != m.fingerprint) {
                    record_conflict(tx, m)?;
                    continue;
                }
                // An introduction cannot replace a retained identity or bind a
                // pin already owned under another ID, including direct-only pins.
                let conflict:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM peers WHERE (id=? AND fp!=?) OR (id!=? AND fp=?))",params![m.device_id.as_str(),m.fingerprint.as_str(),m.device_id.as_str(),m.fingerprint.as_str()],|r|r.get(0))?;
                let active_conflict=entries(tx)?.iter().any(|e|e.value.status==MemberStatus::Member&&((e.value.device_id==m.device_id&&e.value.fingerprint!=m.fingerprint)||(e.value.device_id!=m.device_id&&e.value.fingerprint==m.fingerprint)));
                if conflict||active_conflict {
                    record_conflict(tx, m)?;continue;
                }
                let value=old.as_ref().filter(|e|e.value.status==MemberStatus::Member).map(|e|merge_member(&e.value,m)).unwrap_or_else(||m.clone());
                let joined_notice=match old.as_ref().filter(|e|e.setup_id.as_ref()==Some(&snapshot.setup_id)).and_then(|e|e.joined_notice) {
                    Some(joined)=>joined,
                    None=>joined_is_new(tx,&local.device_id,&value)?,
                };
                save_entry(tx,&Entry {authored:false,revoked_binding:false,ready_generation:old.as_ref().and_then(|e|e.ready_generation),readmission:old.as_ref().and_then(|e|e.readmission.clone()),value:value.clone(),admitted_hlc:None,joined_notice:Some(joined_notice),removal:old.and_then(|e|e.removal),denied:false,setup_id:Some(snapshot.setup_id.clone())})?;
                let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM peers WHERE id=?)",[m.device_id.as_str()],|r|r.get(0))?;
                if !exists {
                    let name:String=tx.query_row("SELECT name FROM peers WHERE id=?",[fence.device.as_str()],|r|r.get(0))?;
                    tx.execute("INSERT OR IGNORE INTO setup_candidates VALUES(?,?,?,?)",params![m.device_id.as_str(),m.fingerprint.as_str(),fence.device.as_str(),name])?;
                }
                if !exists&&!blocked.iter().any(|(id,pin)|id.as_ref()==Some(&m.device_id)||*pin==m.fingerprint) {
                    let previous:i64=tx.query_row("SELECT coalesce((SELECT generation FROM peer_generations WHERE device=?),0)",[m.device_id.as_str()],|r|r.get(0))?;
                    let generation=previous.checked_add(1).context("pairing generation exhausted")?;
                    tx.execute("INSERT INTO peers(id,fp,generation,role,name,addresses,established) VALUES(?,?,?,?,?,'[]',0)",params![m.device_id.as_str(),m.fingerprint.as_str(),generation,scope_role(m.role),value.name])?;
                    tx.execute("INSERT INTO peer_generations VALUES(?,?) ON CONFLICT(device) DO UPDATE SET generation=excluded.generation",params![m.device_id.as_str(),generation])?;
                    set_scope(tx,&m.device_id,SetupScope::Enabled)?;
                    tx.execute("UPDATE setup_links SET negotiated=1,participating=1 WHERE device=?",[m.device_id.as_str()])?;
                    let introducer_name:String=tx.query_row("SELECT name FROM peers WHERE id=?",[fence.device.as_str()],|r|r.get(0))?;
                    tx.execute("UPDATE setup_links SET introduced_by=?,introducer_name=? WHERE device=?",params![fence.device.as_str(),introducer_name,m.device_id.as_str()])?;
                    if joined_notice {
                        tx.execute("INSERT OR IGNORE INTO setup_notices VALUES(?,?,?)",params![m.device_id.as_str(),generation,serde_json::to_string(&serde_json::json!({"device":m.device_id,"name":value.name,"introduced_by":fence.device,"introducer_name":introducer_name}))?])?;
                    }
                    settings::initialize(tx,&local.device_id,&m.device_id,m.role==DeviceRole::Phone)?;
                    tx.execute("DELETE FROM setup_candidates WHERE device=? AND fingerprint=?",params![m.device_id.as_str(),m.fingerprint.as_str()])?;
                    delta.added.push(m.device_id.clone());
                }
                if m.device_id != fence.device || retained.iter().any(|e|
                    e.value.device_id == m.device_id && e.value.fingerprint == m.fingerprint && stamp(&value) > stamp(&e.value)) {
                    tx.execute("UPDATE peers SET name=? WHERE id=? AND fp=?",params![value.name,m.device_id.as_str(),m.fingerprint.as_str()])?;
                }
                if let Some(r)=&value.reach {merge_routes(tx,&m.device_id,&m.fingerprint,&r.addresses)?;}
            }
            reconcile_candidates(tx,&local.device_id,&blocked,delta)?;
            let own_removed=denied(tx,&local.device_id,&local.fingerprint)?;
            if own_removed {retire(tx,true,delta)?;}else{dissolve_if_last(tx,&local.device_id,delta)?;}
            validate_capacity(tx)?;
            // A withdrawal may forget the sender; receipts use an independent
            // generation-fenced table and are ineligible in a later setup.
            tx.execute("INSERT INTO members_receipts VALUES(?,?,?,?)",params![fence.device.as_str(),snapshot.transfer_id.as_str(),fence.generation as i64,serde_json::to_string(&meta)?])?;
            tx.execute("DELETE FROM members_receipts WHERE rowid IN (SELECT rowid FROM members_receipts WHERE device=? ORDER BY rowid DESC LIMIT -1 OFFSET 4096)",[fence.device.as_str()])?;
            Ok(false)
        },finish).await
    }
    pub(crate) async fn setup_remove(
        &self,
        local: Id,
        device: Id,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        self.setup_remove_inner(local, device, None, finish).await
    }
    #[cfg(test)]
    pub(crate) async fn setup_remove_at(
        &self,
        local: Id,
        device: Id,
        now: u64,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        self.setup_remove_inner(local, device, Some(now), finish)
            .await
    }
    async fn setup_remove_inner(
        &self,
        local: Id,
        device: Id,
        now: Option<u64>,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        self.setup_write(
            move |tx, delta| remove(tx, &local, &device, now, delta),
            finish,
        )
        .await
    }
    pub(crate) async fn setup_withdraw(
        &self,
        fence: SetupFence,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        let epoch = self.peer_epoch(&fence.device);
        self.setup_write(
            move |tx, delta| {
                epoch.check(tx)?;
                fence.check(tx)?;
                retire(tx, true, delta)
            },
            finish,
        )
        .await
    }
}
fn scope_role(role: DeviceRole) -> &'static str {
    match role {
        DeviceRole::Phone => "phone",
        DeviceRole::Computer => "computer",
    }
}
fn merge_routes(tx: &Transaction<'_>, id: &Id, pin: &Hash, hints: &[String]) -> Result<()> {
    let cached = tx
        .query_row(
            "SELECT addresses FROM peers WHERE id=? AND fp=?",
            params![id.as_str(), pin.as_str()],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if let Some(raw) = cached {
        let cached: Vec<String> = serde_json::from_str(&raw)?;
        let proven = route_proven(tx, id)? && !cached.is_empty();
        let mut routes: Vec<_> = if proven {
            cached.first().cloned().into_iter().collect()
        } else {
            Vec::new()
        };
        for a in hints.iter().chain(cached.iter().skip(usize::from(proven))) {
            if routes.len() < 16 && !routes.contains(a) {
                routes.push(a.clone());
            }
        }
        tx.execute(
            "UPDATE peers SET addresses=? WHERE id=?",
            params![serde_json::to_string(&routes)?, id.as_str()],
        )?;
        tx.execute(
            "UPDATE setup_links SET route_proven=? WHERE device=?",
            params![proven, id.as_str()],
        )?;
    }
    Ok(())
}
/// Called in the *binding* transaction, before its installer publishes trust.
/// Only this local token authority may clear a deny/high-water barrier.
pub(super) fn paired(
    tx: &Transaction<'_>,
    local: &Member,
    remote: &pneu_proto::Hello,
    pin: &Hash,
    scope: SetupScope,
    previous: Option<SetupLink>,
) -> Result<()> {
    let scope = previous.as_ref().map(|l| l.scope).unwrap_or(scope);
    set_scope(tx, &remote.device_id, scope)?;
    tx.execute(
        "UPDATE setup_links SET introduced_by=NULL,introducer_name=NULL,token_founding=1,negotiated=?,participating=? WHERE device=?",
        params![
            remote.capabilities.iter().any(|c| c == "setup"),
            remote
                .setup
                .as_ref()
                .is_some_and(|h| h.scope == SetupScope::Enabled),
            remote.device_id.as_str()
        ],
    )?;
    tx.execute("UPDATE setup_state SET withdrawn=0 WHERE singleton=1", [])?;
    clear_own_deny(tx, local)?;
    let mut s = state(tx)?;
    let compatible = remote.capabilities.iter().any(|c| c == "setup")
        && remote.setup.as_ref().is_some_and(|r| {
            r.scope == SetupScope::Enabled
                && !(r.setup_id.is_some() && s.setup_id.is_some() && r.setup_id != s.setup_id)
        });
    if scope != SetupScope::Disabled
        && remote.capabilities.iter().any(|c| c == "setup")
        && remote.setup.as_ref().is_some_and(|r| {
            r.scope != SetupScope::Disabled
                && s.setup_id.is_some()
                && r.setup_id.is_some()
                && s.setup_id != r.setup_id
        })
    {
        set_scope(tx, &remote.device_id, SetupScope::Disabled)?;
        tx.execute("UPDATE setup_links SET outcome='Direct link kept; separate setups not linked' WHERE device=?",[remote.device_id.as_str()])?;
    }
    let mut remote_value = Member {
        device_id: remote.device_id.clone(),
        fingerprint: pin.clone(),
        role: remote.role.unwrap_or(DeviceRole::Phone),
        name: remote.name.clone(),
        status: MemberStatus::Member,
        hlc: 0,
        writer_device: local.device_id.clone(),
        reach: None,
        readmits: None,
    };
    let mut retired_member = false;
    // Replacing an exported certificate terminally retires its old incarnation.
    for e in entries(tx)? {
        if e.value.device_id == remote.device_id
            && e.value.fingerprint != *pin
            && e.value.status == MemberStatus::Member
        {
            retired_member |= e.setup_id == s.setup_id;
            let mut removed = e.value;
            removed.status = MemberStatus::Removed;
            removed.reach = None;
            removed.readmits = None;
            removed.hlc = allocate_stamp(tx)?.1;
            removed.writer_device = local.device_id.clone();
            save_entry(
                tx,
                &Entry {
                    ready_generation: None,
                    readmission: None,
                    admitted_hlc: None,
                    joined_notice: None,
                    value: removed.clone(),
                    removal: Some(removed),
                    denied: true,
                    authored: false,
                    revoked_binding: true,
                    setup_id: e.setup_id,
                },
            )?;
        }
    }
    if let Some(mut old) = entries(tx)?
        .into_iter()
        .find(|e| e.value.device_id == remote.device_id && e.value.fingerprint == *pin)
    {
        remote_value.readmits = fresh_pair_claim(&old);
        old.readmission = None;
        old.ready_generation = None;
        old.denied = false;
        old.authored = false;
        old.revoked_binding = false;
        old.value = remote_value.clone();
        old.value.hlc = allocate_stamp(tx)?.1;
        old.setup_id = None;
        save_entry(tx, &old)?;
    }
    if scope == SetupScope::Enabled && compatible {
        if s.setup_id.is_none() {
            let id = match remote.setup.as_ref().and_then(|r| r.setup_id.clone()) {
                Some(id) => Some(id),
                None if local.device_id.as_str() < remote.device_id.as_str() => {
                    Some(fresh_setup_id(tx)?)
                }
                None => None,
            };
            if let Some(id) = id {
                tx.execute(
                    "UPDATE setup_state SET setup_id=?,generation=generation+1 WHERE singleton=1",
                    [id.as_str()],
                )?;
                clear_pending_link(tx)?;
                s = state(tx)?;
            }
        }
        if let Some(id) = s.setup_id {
            local_value(tx, local.clone(), &id, false)?;
            local_value(tx, remote_value, &id, true)?;
        }
    }
    if retired_member {
        dissolve_if_last(tx, &local.device_id, &mut SetupDelta::default())?;
    }
    clear_resolved_conflicts(tx)?;
    validate_capacity(tx)
}
// Callers validate capacity after all lifecycle changes in the transaction.
fn reconcile_candidates(
    tx: &Transaction<'_>,
    local: &Id,
    blocked: &[(Option<Id>, Hash)],
    delta: &mut SetupDelta,
) -> Result<()> {
    let s = state(tx)?;
    let Some(setup_id) = s.setup_id else {
        return Ok(());
    };
    for mut entry in entries(tx)? {
        let Some(record) = entry.readmission.clone() else {
            continue;
        };
        if !entry.denied
            || entry.authored
            || record.key.setup_generation != s.generation
            || !entry
                .removal
                .as_ref()
                .is_some_and(|r| claims(&record.value, r))
        {
            continue;
        }
        let value = &record.value;
        if blocked
            .iter()
            .any(|(id, pin)| id.as_ref() == Some(&value.device_id) || *pin == value.fingerprint)
        {
            continue;
        }
        let conflict: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE id=? OR fp=?)",
            params![value.device_id.as_str(), value.fingerprint.as_str()],
            |r| r.get(0),
        )?;
        let active_conflict = entries(tx)?.iter().any(|e| {
            e.value.status == MemberStatus::Member
                && ((e.value.device_id == value.device_id
                    && e.value.fingerprint != value.fingerprint)
                    || (e.value.device_id != value.device_id
                        && e.value.fingerprint == value.fingerprint))
        });
        if conflict || active_conflict {
            record_conflict(tx, value)?;
            continue;
        }
        if entry.joined_notice.is_none() {
            entry.joined_notice = Some(joined_is_new(tx, local, value)?);
        }
        entry.value = record.value;
        entry.setup_id = Some(setup_id.clone());
        entry.denied = false;
        entry.authored = false;
        entry.revoked_binding = false;
        entry.readmission = None;
        save_entry(tx, &entry)?;
        tx.execute(
            "INSERT INTO setup_candidates VALUES(?,?,?,?) ON CONFLICT(device,fingerprint) DO UPDATE SET introduced_by=excluded.introduced_by,introducer_name=excluded.introducer_name",
            params![
                entry.value.device_id.as_str(),
                entry.value.fingerprint.as_str(),
                record.introducer.as_str(),
                record.introducer_name
            ],
        )?;
    }
    let candidates: Vec<(Id, Hash, String, String)> = {
        let mut q = tx.prepare(
            "SELECT device,fingerprint,introduced_by,introducer_name FROM setup_candidates",
        )?;
        q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .map(|r| {
            let (d, p, b, n) = r?;
            Ok((Id::new(d)?, Hash::new(p)?, b, n))
        })
        .collect::<Result<_>>()?
    };
    for (id, pin, by, name) in candidates {
        if entries(tx)?
            .iter()
            .any(|e| e.denied && e.value.device_id == id)
            && !entries(tx)?.iter().any(|e| {
                !e.denied
                    && e.value.device_id == id
                    && e.value.fingerprint == pin
                    && e.removal.is_some()
            })
        {
            continue;
        }
        if blocked
            .iter()
            .any(|(d, p)| d.as_ref() == Some(&id) || *p == pin)
        {
            continue;
        }
        let Some(entry) = entries(tx)?.into_iter().find(|e| {
            e.value.device_id == id
                && e.value.fingerprint == pin
                && !e.denied
                && e.setup_id.as_ref() == Some(&setup_id)
                && e.value.status == MemberStatus::Member
        }) else {
            tx.execute(
                "DELETE FROM setup_candidates WHERE device=? AND fingerprint=?",
                params![id.as_str(), pin.as_str()],
            )?;
            continue;
        };
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE id=? OR fp=?)",
            params![id.as_str(), pin.as_str()],
            |r| r.get(0),
        )?;
        if exists {
            continue;
        }
        let joined_notice = match entry.joined_notice {
            Some(joined) => joined,
            None => joined_is_new(tx, local, &entry.value)?,
        };
        let generation: i64 = tx.query_row(
            "SELECT coalesce((SELECT generation FROM peer_generations WHERE device=?),0)+1",
            [id.as_str()],
            |r| r.get(0),
        )?;
        tx.execute("INSERT INTO peers(id,fp,generation,role,name,addresses,established) VALUES(?,?,?,?,?,'[]',0)",params![id.as_str(),pin.as_str(),generation,scope_role(entry.value.role),entry.value.name])?;
        tx.execute("INSERT INTO peer_generations VALUES(?,?) ON CONFLICT(device) DO UPDATE SET generation=excluded.generation",params![id.as_str(),generation])?;
        set_scope(tx, &id, SetupScope::Enabled)?;
        tx.execute(
            "UPDATE setup_links SET negotiated=1,participating=1 WHERE device=?",
            [id.as_str()],
        )?;
        tx.execute(
            "UPDATE setup_links SET introduced_by=?,introducer_name=? WHERE device=?",
            params![by, name, id.as_str()],
        )?;
        if joined_notice {
            tx.execute("INSERT OR IGNORE INTO setup_notices VALUES(?,?,?)",params![id.as_str(),generation,serde_json::to_string(&serde_json::json!({"device":id,"name":entry.value.name,"introduced_by":by,"introducer_name":name}))?])?;
        }
        settings::initialize(tx, local, &id, entry.value.role == DeviceRole::Phone)?;
        if let Some(reach) = &entry.value.reach {
            merge_routes(tx, &id, &pin, &reach.addresses)?;
        }
        tx.execute(
            "DELETE FROM setup_candidates WHERE device=? AND fingerprint=?",
            params![id.as_str(), pin.as_str()],
        )?;
        delta.added.push(id);
    }
    Ok(())
}
impl Store {
    pub(crate) async fn setup_reconcile_candidates(
        &self,
        local: Id,
        blocked: Vec<(Option<Id>, Hash)>,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        self.setup_write(
            move |tx, delta| {
                reconcile_candidates(tx, &local, &blocked, delta)?;
                validate_capacity(tx)
            },
            finish,
        )
        .await
    }
    pub(crate) async fn setup_refresh_self(&self, local: Member) -> Result<()> {
        // Periodic reach discovery can capture a name before a concurrent rename.
        // Only the serialized configuration path may write that register.
        self.setup_refresh_self_inner(local, false, || Ok(()), || {})
            .await
    }
    pub(crate) async fn setup_refresh_self_then(
        &self,
        local: Member,
        persist: impl FnOnce() -> Result<()> + Send + 'static,
        finish: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        self.setup_refresh_self_inner(local, true, persist, finish)
            .await
    }
    async fn setup_refresh_self_inner(
        &self,
        local: Member,
        update_name: bool,
        persist: impl FnOnce() -> Result<()> + Send + 'static,
        finish: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        let expected = self.setup_state().generation;
        let result = Arc::new(Mutex::new(Ok(())));
        let saved_result = result.clone();
        self.setup_write(
            move |tx, _| {
                let s = state(tx)?;
                ensure!(s.generation == expected, PairingRevoked);
                let Some(setup_id) = s.setup_id else {
                    return Ok(());
                };
                if let Some(mut entry) = entries(tx)?.into_iter().find(|e| {
                    e.value.device_id == local.device_id
                        && e.value.fingerprint == local.fingerprint
                        && !e.denied
                        && e.setup_id.as_ref() == Some(&setup_id)
                }) {
                    entry.admitted_hlc.get_or_insert(entry.value.hlc);
                    if update_name && entry.value.name != local.name {
                        entry.value.name = local.name;
                        entry.value.hlc = allocate_stamp(tx)?.1;
                        entry.value.writer_device = local.device_id.clone();
                    }
                    if entry.value.reach.as_ref().map(|r| &r.addresses)
                        != local.reach.as_ref().map(|r| &r.addresses)
                    {
                        entry.value.reach = local.reach;
                        if let Some(r) = &mut entry.value.reach {
                            r.hlc = allocate_stamp(tx)?.1;
                        }
                    }
                    save_entry(tx, &entry)?;
                    validate_capacity(tx)?;
                }
                Ok(())
            },
            move |_| {
                // The roster is durable first. Startup repairs the config name
                // from it if saving fails or the process exits between writes.
                let outcome = persist();
                if outcome.is_ok() {
                    finish();
                }
                *saved_result.lock().expect("config persistence") = outcome;
            },
        )
        .await?;
        std::mem::replace(&mut *result.lock().expect("config persistence"), Ok(()))
    }
}
fn fresh_setup_id(tx: &Transaction<'_>) -> Result<Id> {
    loop {
        let id = Id::new(crate::random_hex(16))?;
        let retired: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM setup_retired WHERE setup_id=?)",
            [id.as_str()],
            |r| r.get(0),
        )?;
        if !retired {
            return Ok(id);
        }
    }
}

fn clear_own_deny(tx: &Transaction<'_>, local: &Member) -> Result<()> {
    if let Some(mut own) = entries(tx)?.into_iter().find(|e| {
        e.value.device_id == local.device_id
            && e.value.fingerprint == local.fingerprint
            && (e.denied || e.value.status != MemberStatus::Member)
    }) {
        own.readmission = None;
        own.denied = false;
        own.authored = false;
        own.revoked_binding = false;
        let claim = fresh_pair_claim(&own);
        own.value = local.clone();
        own.value.readmits = claim;
        own.value.hlc = allocate_stamp(tx)?.1;
        own.setup_id = None;
        save_entry(tx, &own)?;
    }
    Ok(())
}
impl Store {
    /// Only a newly confirmed local token attempt, or consumption of a matching
    /// active local invitation, may call this. Old-token recovery is not consent.
    pub(crate) async fn setup_fresh_local_recovery(
        &self,
        peer: Id,
        local: Member,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        let epoch = self.peer_epoch(&peer);
        self.setup_write(
            move |tx, _| {
                epoch.check(tx)?;
                tx.execute(
                    "UPDATE setup_links SET introduced_by=NULL,introducer_name=NULL WHERE device=?",
                    [peer.as_str()],
                )?;
                tx.execute("DELETE FROM setup_notices WHERE device=?", [peer.as_str()])?;
                let pin: String =
                    tx.query_row("SELECT fp FROM peers WHERE id=?", [peer.as_str()], |r| {
                        r.get(0)
                    })?;
                if let Some(mut entry) = entries(tx)?
                    .into_iter()
                    .find(|e| e.value.device_id == peer && e.value.fingerprint.as_str() == pin)
                {
                    entry.readmission = None;
                    if entry.removal.is_some() {
                        entry.value.readmits = fresh_pair_claim(&entry);
                        entry.value.status = MemberStatus::Member;
                        entry.value.hlc = allocate_stamp(tx)?.1;
                        entry.value.writer_device = local.device_id.clone();
                        entry.denied = false;
                        entry.authored = false;
                        entry.revoked_binding = false;
                    }
                    save_entry(tx, &entry)?;
                }
                if state(tx)?.withdrawn {
                    clear_own_deny(tx, &local)?;
                    tx.execute("UPDATE setup_state SET withdrawn=0 WHERE singleton=1", [])?;
                }
                validate_capacity(tx)
            },
            finish,
        )
        .await
    }
}

pub(super) fn members_metadata(snapshot: &Members) -> Metadata {
    Metadata::MembersHash {
        sha256: format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&snapshot.clone().canonical()).expect("MEMBERS serialization")
            )
        ),
    }
}

fn clear_pending_link(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("DELETE FROM setup_pending_link; UPDATE setup_links SET discovery_connection=NULL,discovery_id=NULL;")?;
    Ok(())
}
fn enable_ask(tx: &Transaction<'_>) -> Result<()> {
    tx.execute("UPDATE setup_links SET scope='enabled',prompt=0,outcome=NULL,allow_connection=NULL,token_founding=0 WHERE scope='ask'", [])?;
    Ok(())
}
impl Store {
    pub(crate) async fn setup_begin_link(
        &self,
        peer: Id,
        expected: Option<(u64, SetupScope)>,
        connections: Vec<(Id, u64)>,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<bool> {
        let epoch = self.peer_epoch(&peer);
        self.setup_write(move |tx,delta| {
            let s=state(tx)?;
            // Check the displayed binding and scope in the enabling transaction.
            if let Some((binding,scope))=expected {
                let current:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM peers WHERE id=? AND generation=?)",params![peer.as_str(),binding as i64],|r|r.get(0))?;
                if !current || !s.links.iter().any(|l|l.device==peer&&l.scope==scope) {
                    return Ok(false);
                }
            } else {
                epoch.check(tx)?;
            }
            ensure!(!s.withdrawn,"fresh token pairing required after withdrawal");
            for (id,connection) in connections {
                tx.execute("UPDATE setup_links SET last_connection=? WHERE device=?",params![connection as i64,id.as_str()])?;
            }
            if let Some(prompt)=s.links.iter().find(|l|l.prompt && l.device==peer).or_else(||s.links.iter().find(|l|l.prompt)) {
                tx.execute("UPDATE setup_links SET outcome='allow_pending_refresh',allow_connection=last_connection WHERE device=?",[prompt.device.as_str()])?;
            } else {
                // Capture only covered binding incarnations. Peer deletion/replacement cascades their discovery state.
                tx.execute("UPDATE setup_links SET discovery_connection=last_connection,discovery_id=NULL WHERE scope='ask' OR device=?",[peer.as_str()])?;
                enable_ask(tx)?;
                set_scope(tx,&peer,SetupScope::Enabled)?;
                tx.execute("UPDATE setup_links SET token_founding=0 WHERE device=?",[peer.as_str()])?;
                if s.setup_id.is_none() {
                    tx.execute("INSERT INTO setup_pending_link VALUES(1,?) ON CONFLICT(singleton) DO NOTHING",[crate::now_ms().saturating_add(3000) as i64])?;
                } else {clear_pending_link(tx)?;}
            }
            delta.reconnect=true;
            Ok(true)
        },finish).await
    }
    pub(crate) async fn setup_discovery_tick(
        &self,
        local: Member,
        now: u64,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        self.setup_write(
            move |tx, delta| {
                let s = state(tx)?;
                if !s.pending_link {
                    return Ok(());
                }
                if s.setup_id.is_some() || s.withdrawn {
                    return clear_pending_link(tx);
                }
                let deadline: i64 =
                    tx.query_row("SELECT deadline FROM setup_pending_link", [], |r| r.get(0))?;
                if now < deadline as u64 || s.waiting_for.is_some() {
                    return Ok(());
                }
                let id = fresh_setup_id(tx)?;
                tx.execute(
                    "UPDATE setup_state SET setup_id=?,generation=generation+1 WHERE singleton=1",
                    [id.as_str()],
                )?;
                local_value(tx, local, &id, false)?;
                clear_pending_link(tx)?;
                validate_capacity(tx)?;
                delta.reconnect = true;
                Ok(())
            },
            finish,
        )
        .await
    }
}

impl Store {
    pub(crate) async fn setup_legacy_ready(&self, fence: SetupFence) -> Result<()> {
        self.setup_write(
            move |tx, _| {
                fence.check(tx)?;
                tx.execute(
                    "UPDATE setup_links SET negotiated=0,participating=0,last_connection=? WHERE device=?",
                    params![fence.connection as i64,fence.device.as_str()],
                )?;
                let phone: bool = tx.query_row(
                    "SELECT role='phone' FROM peers WHERE id=?",
                    [fence.device.as_str()],
                    |r| r.get(0),
                )?;
                if phone {
                    phone_ready(tx, &fence)?;
                }
                Ok(())
            },
            |_| {},
        )
        .await
    }
}

impl Store {
    pub(crate) async fn setup_readmit(
        &self,
        key: ReadmissionKey,
        local: Id,
        allow: bool,
        blocked: Vec<(Option<Id>, Hash)>,
        finish: impl FnOnce(SetupDelta) + Send + 'static,
    ) -> Result<()> {
        self.setup_write(move |tx, delta| {
            let s = state(tx)?;
            if s.withdrawn || s.setup_id.is_none() || s.generation != key.setup_generation { return Ok(()); }
            let all = entries(tx)?;
            let Some(mut entry) = all.iter().find(|e| e.denied && e.value.device_id == key.device && e.value.fingerprint == key.fingerprint).cloned() else { return Ok(()); };
            let Some(record) = entry.readmission.as_ref().filter(|r| r.key == key && r.decision == "pending") else { return Ok(()); };
            if !entry.authored || !entry.removal.as_ref().is_some_and(|r| r.hlc == key.removal_hlc && r.writer_device == key.removal_writer && claims(&record.value,r)) { return Ok(()); }
            if !allow {
                entry.readmission.as_mut().unwrap().decision = "kept".into();
                return save_entry(tx, &entry);
            }
            ensure!(!blocked.iter().any(|(id,pin)| id.as_ref() == Some(&key.device) || *pin == key.fingerprint), "Pairing in progress · Try Allow when it finishes");
            let conflict: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM peers WHERE id=? OR fp=?)", params![key.device.as_str(), key.fingerprint.as_str()], |r| r.get(0))?;
            let active_conflict = all.iter().any(|e| e.value.status == MemberStatus::Member && ((e.value.device_id == key.device && e.value.fingerprint != key.fingerprint) || (e.value.device_id != key.device && e.value.fingerprint == key.fingerprint)));
            ensure!(!conflict && !active_conflict, "{} has a different certificate · Pair with it directly", record.value.name);
            let record = record.clone();
            entry.value = record.value.clone();
            entry.setup_id = s.setup_id;
            entry.denied = false;
            entry.authored = false;
            entry.revoked_binding = false;
            entry.ready_generation = None;
            entry.joined_notice = Some(false);
            entry.readmission.as_mut().unwrap().decision = "allowed".into();
            save_entry(tx, &entry)?;
            let previous: i64 = tx.query_row("SELECT coalesce((SELECT generation FROM peer_generations WHERE device=?),0)", [key.device.as_str()], |r| r.get(0))?;
            let generation = previous.checked_add(1).context("pairing generation exhausted")?;
            tx.execute("INSERT INTO peers(id,fp,generation,role,name,addresses,established) VALUES(?,?,?,?,?,'[]',0)", params![key.device.as_str(),key.fingerprint.as_str(),generation,scope_role(record.value.role),record.value.name])?;
            tx.execute("INSERT INTO peer_generations VALUES(?,?) ON CONFLICT(device) DO UPDATE SET generation=excluded.generation",params![key.device.as_str(),generation])?;
            set_scope(tx,&key.device,SetupScope::Enabled)?;
            tx.execute("UPDATE setup_links SET negotiated=1,participating=1,introduced_by=?,introducer_name=? WHERE device=?",params![record.introducer.as_str(),record.introducer_name,key.device.as_str()])?;
            settings::initialize(tx,&local,&key.device,record.value.role==DeviceRole::Phone)?;
            if let Some(reach)=&record.value.reach {merge_routes(tx,&key.device,&key.fingerprint,&reach.addresses)?;}
            tx.execute("DELETE FROM setup_candidates WHERE device=? AND fingerprint=?",params![key.device.as_str(),key.fingerprint.as_str()])?;
            validate_capacity(tx)?;
            delta.added.push(key.device);
            Ok(())
        },finish).await
    }
}

/// The ordinary authored removal, shared by explicit removal and local reinstall replacement.
pub(super) fn remove(
    tx: &Transaction<'_>,
    local: &Id,
    device: &Id,
    now: Option<u64>,
    delta: &mut SetupDelta,
) -> Result<()> {
    let s = state(tx)?;
    ensure!(!s.withdrawn, "withdrawn setup");
    let id = s.setup_id.context("no setup")?;
    let member = s
        .roster
        .into_iter()
        .find(|m| m.device_id == *device && m.status == MemberStatus::Member)
        .context("not an active setup member")?;
    ensure!(device != local, "cannot remove own identity");
    let mut removed = member;
    removed.status = MemberStatus::Removed;
    removed.reach = None;
    removed.readmits = None;
    removed.hlc = match now {
        Some(now) => allocate_stamp_at(tx, now)?.1,
        None => allocate_stamp(tx)?.1,
    };
    removed.writer_device = local.clone();
    save_entry(
        tx,
        &Entry {
            ready_generation: None,
            readmission: None,
            admitted_hlc: None,
            joined_notice: None,
            value: removed.clone(),
            removal: Some(removed.clone()),
            denied: true,
            authored: true,
            revoked_binding: true,
            setup_id: Some(id),
        },
    )?;
    delta
        .tombstones
        .push((removed.device_id.clone(), removed.fingerprint.clone()));
    forget(tx, &removed.device_id, &removed.fingerprint, delta)?;
    dissolve_if_last(tx, local, delta)?;
    validate_capacity(tx)
}
