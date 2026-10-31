//! Paired peers, completed file receipts, bounded clipboard history, latest outgoing text.
#[path = "store_clipboard.rs"]
mod clipboard;
#[path = "store_members.rs"]
pub(crate) mod members;
#[path = "outbox.rs"]
pub mod outbox;
#[path = "store_settings.rs"]
mod settings;
pub(crate) use settings::{SettingsMalformed, SettingsUnexpected};

use crate::storage::{Folder, Stage, VerifiedFile, clean_staging, staging_directory};
use crate::work::{Reservation, Work, start_blocking};
use anyhow::{Context, Result, ensure};
use pneu_proto::{Clipboard, ClipboardOrigin, FileBegin, Hash, Id, NackCode};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

// Legacy HELLO extensions are not negotiated hints. Direction supplies the
// known role; only an outbound invitation/route can identify a legacy computer.
pub(crate) fn normalize_legacy_hello(hello: &mut pneu_proto::Hello, outbound: bool) {
    if !hello.capabilities.iter().any(|c| c == "mesh") {
        hello.role = Some(if outbound {
            pneu_proto::DeviceRole::Computer
        } else {
            pneu_proto::DeviceRole::Phone
        });
        hello.addresses = None;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextReceipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hlc: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_device: Option<Id>,
    pub transfer_id: Id,
    pub sha256: Hash,
    pub created_at: u64,
    #[serde(default)]
    pub origin: ClipboardOrigin,
}
impl From<&Clipboard> for TextReceipt {
    fn from(text: &Clipboard) -> Self {
        Self {
            hlc: text.hlc,
            source_device: text.source_device.clone(),
            transfer_id: text.transfer_id.clone(),
            sha256: Hash::new(hex::encode(Sha256::digest(text.text.as_bytes()))).expect("SHA-256"),
            created_at: text.created_at,
            origin: text.origin,
        }
    }
}
fn keyed_text_receipt(
    key: &crate::clipboard_fingerprint::FingerprintKey,
    text: &Clipboard,
) -> TextReceipt {
    let mut receipt = TextReceipt::from(text);
    let raw: [u8; 32] = hex::decode(receipt.sha256.as_str())
        .expect("SHA-256")
        .try_into()
        .expect("SHA-256");
    receipt.sha256 = Hash::new(hex::encode(key.digest(&raw))).expect("HMAC-SHA256");
    receipt
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation")]
pub enum Metadata {
    File(FileBegin),
    Settings(pneu_proto::PeerSettings),
    Members(pneu_proto::Members),
    MembersHash { sha256: String },
    Text(TextReceipt),
}

pub(crate) const RECENT_TEXT_LIMIT: usize = 64;
const CACHE_OWNER_LIMIT: usize = 128;
struct CacheOwner {
    path: PathBuf,
    file: crate::storage::OwnedFile,
}
#[derive(Clone, Debug)]
pub(crate) struct PeerInfo {
    pub id: Id,
    pub fingerprint: Hash,
    pub generation: u64,
    pub role: String,
    pub name: String,
    pub addresses: Vec<String>,
    pub route_proven: bool,
}
#[derive(Clone)]
struct AbandonedClaim {
    epoch: PeerEpoch,
    token: String,
}
#[derive(Clone)]
pub struct Store {
    pub(crate) setup: Arc<Mutex<Option<members::SetupState>>>,
    // Last authenticated HELLO scope, tied to the binding incarnation. UI only.
    pub(crate) setup_remote_scopes: Arc<Mutex<HashMap<Id, (u64, pneu_proto::SetupScope)>>>,
    pub(crate) setup_changed: tokio::sync::watch::Sender<u64>,
    connection: Arc<Mutex<Connection>>,
    clipboard_history_revision: Arc<AtomicU64>,
    clipboard_clock_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) clipboard_fingerprint_key: crate::clipboard_fingerprint::FingerprintKey,
    abandoned_claims: Arc<Mutex<HashMap<(Id, Id), AbandonedClaim>>>,
    #[cfg(test)]
    faults: Arc<Mutex<HashMap<&'static str, usize>>>,
    pub(crate) pair_settings: Arc<Mutex<HashMap<Id, Vec<pneu_proto::PeerSetting>>>>,
    pub(crate) settings_changed: tokio::sync::watch::Sender<u64>,
    pub(crate) settings_generations: Arc<Mutex<HashMap<Id, u64>>>,
    _ownership: Arc<StoreOwnership>,
    folder: Folder,
    reserve: Arc<AtomicU64>,
    work: Arc<Work>,
    pub(crate) connection_id: Option<u64>,
    pub(crate) action_connection_id: Option<u64>,
    pub(crate) mesh: bool,
    pub(crate) setup_generation: Option<u64>,
    pub(crate) local_device: Option<Id>,
    generations: Arc<Mutex<HashMap<Id, u64>>>,
    peer_generations: Option<HashMap<Id, u64>>,
    staging_ready: Arc<Mutex<HashSet<crate::platform::fs::Identity>>>,
    // Nonpersistent, bounded metadata proof for failed-save previews (no file FDs). A restart or
    // eviction deliberately leaves retirement to normal cache pruning.
    cache_owners: Arc<Mutex<HashMap<(Id, Id), CacheOwner>>>,
    #[cfg(test)]
    peer_write_barrier: Arc<Mutex<Option<PeerWriteBarrier>>>,
}
#[cfg(test)]
struct PeerWriteBarrier {
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
/// Closing a CLOEXEC descriptor is insufficient during another thread's fork:
/// the child briefly inherits the same flock/open-file description before exec.
/// Release explicitly when the final daemon owner ends, even if a child still
/// holds an inherited descriptor. Remaining Store jobs keep this Arc alive.
struct StoreOwnership(fs::File);
impl Drop for StoreOwnership {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = self.0.unlock();
        // Windows owns one byte on the DB handle; closing it releases the lock.
    }
}
/// Immutable authority captured before scheduling work, never refreshed by a
/// delayed blocking job or by reuse of a device ID after re-pairing.
#[derive(Clone)]
struct PeerEpoch {
    device: Id,
    generation: u64,
}
impl PeerEpoch {
    fn check(&self, c: &Connection) -> Result<()> {
        let valid: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE generation=? AND id=?)",
            params![self.generation as i64, self.device.as_str()],
            |r| r.get(0),
        )?;
        ensure!(valid, PairingRevoked);
        Ok(())
    }
}
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}
#[derive(Debug)]
struct PairingRevoked;
impl std::fmt::Display for PairingRevoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("pairing generation revoked")
    }
}
impl std::error::Error for PairingRevoked {}
#[derive(Debug)]
pub(crate) struct ClipboardStorageFailure;
impl std::fmt::Display for ClipboardStorageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("clipboard clock/policy storage failed")
    }
}
impl std::error::Error for ClipboardStorageFailure {}

#[derive(Debug)]
struct ReceiptConflict;
impl std::fmt::Display for ReceiptConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("receipt ID conflict")
    }
}
impl std::error::Error for ReceiptConflict {}
/// A short-lived shared-ID reservation, not a replay queue. The claim owns its
/// cleanup even if the caller disappears while a blocking insert is finishing.
pub(crate) struct ClipboardReservation {
    // Bridge completion of the blocking insert to observation/drop of its
    // result, so shutdown cannot miss cleanup of an abandoned returned claim.
    reservation: Option<Reservation>,
    epoch: PeerEpoch,
    store: Store,
    device: Id,
    text: Clipboard,
    token: Option<String>,
}
impl ClipboardReservation {
    #[cfg(test)]
    fn finalize(self) -> impl std::future::Future<Output = Result<()>> + Send {
        self.complete(None, true)
    }

    pub(crate) async fn discard(mut self) -> Result<()> {
        let store = self.store.clone();
        store
            .start_cleanup(move |_| {
                self.release()?;
                Ok(())
            })
            .await?
    }
    fn release(&mut self) -> Result<()> {
        if let Some(token) = &self.token {
            release_text_claim(
                &self.store,
                self.epoch.clone(),
                &self.device,
                &self.text.transfer_id,
                token,
            )?;
            self.token.take();
        }
        Ok(())
    }
    pub(crate) fn complete(
        mut self,
        application_hlc: Option<u64>,
        applied: bool,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        let store = self.store.clone();
        let epoch = self.epoch.clone();
        // This method submits synchronously: even dropping the returned future
        // cannot discard a successful OS effect. The claim retained its insert's
        // admission before it was handed to the clipboard caller.
        let reservation = self.reservation.take().expect("text finalization capacity");
        let job = store.submit(reservation, move |store| {
            #[cfg(test)]
            store.fault("finalize_text")?;
            store.peer_write(epoch, |tx| {
                let token = self.token.as_ref().context("missing text claim")?;
                let removed = tx.execute(
                    "DELETE FROM receipts WHERE device=? AND id=? AND destination=?",
                    params![self.device.as_str(), self.text.transfer_id.as_str(), token],
                )?;
                ensure!(removed == 1, "text claim lost");
                record_text(
                    tx,
                    &self.device,
                    &self.text,
                    application_hlc,
                    applied,
                    &store.clipboard_fingerprint_key,
                )?;
                Ok(())
            })?;
            self.token.take();
            drop(self);
            Ok(())
        });
        async move { job.await? }
    }
}
impl Drop for ClipboardReservation {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            // Only plain data enters the cleanup task. If runtime shutdown drops
            // the queued closure, it must not drop another ClipboardReservation and recursively
            // enqueue cleanup. Startup removes any abandoned reservation.
            let reservation = self.reservation.take();
            let epoch = self.epoch.clone();
            let device = self.device.clone();
            let id = self.text.transfer_id.clone();
            self.store
                .abandoned_claims
                .lock()
                .expect("abandoned text claims")
                .insert(
                    (device.clone(), id.clone()),
                    AbandonedClaim {
                        epoch: epoch.clone(),
                        token: token.clone(),
                    },
                );
            self.store.start_cleanup(move |store| {
                let _reservation = reservation;
                let result = release_text_claim(store, epoch, &device, &id, &token);
                if let Err(error) = &result {
                    eprintln!("text claim cleanup deferred: {error}");
                }
                result
            });
        }
    }
}
fn release_text_claim(
    store: &Store,
    epoch: PeerEpoch,
    device: &Id,
    id: &Id,
    token: &str,
) -> Result<()> {
    #[cfg(test)]
    store.fault("release_text_claim")?;
    let result = store.peer_write(epoch, |tx| {
        tx.execute(
            "DELETE FROM receipts WHERE device=? AND id=? AND destination=?",
            params![device.as_str(), id.as_str(), token],
        )?;
        Ok(())
    });
    if let Err(error) = result {
        // Revocation removed the old claim. Never acquire cleanup authority
        // over a replacement pairing, even with the same ID.
        if error.downcast_ref::<PairingRevoked>().is_none() {
            return Err(error);
        }
    }
    let mut abandoned = store
        .abandoned_claims
        .lock()
        .expect("abandoned text claims");
    let key = (device.clone(), id.clone());
    if abandoned
        .get(&key)
        .is_some_and(|claim| claim.token == token)
    {
        abandoned.remove(&key);
    }
    Ok(())
}
/// Overflow-safe rejection happens before *any* clock/register/history mutation.
pub(crate) fn validate_stamp(hlc: u64, now: u64) -> Result<()> {
    ensure!(
        hlc.saturating_sub(now) <= 60_000,
        "clipboard clock disagreement"
    );
    Ok(())
}
fn observe_hlc(tx: &Transaction<'_>, hlc: u64) -> Result<()> {
    let previous: String = tx.query_row(
        "SELECT last_seen FROM mesh_clock WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    if hlc > previous.parse::<u64>()? {
        tx.execute(
            "UPDATE mesh_clock SET last_seen=? WHERE singleton=1",
            [hlc.to_string()],
        )?;
    }
    Ok(())
}
fn allocate_stamp(tx: &Transaction<'_>) -> Result<(u64, u64)> {
    allocate_stamp_at(tx, crate::now_ms())
}
fn allocate_stamp_at(tx: &Transaction<'_>, now: u64) -> Result<(u64, u64)> {
    let previous: String = tx.query_row(
        "SELECT last_seen FROM mesh_clock WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let hlc = now.max(
        previous
            .parse::<u64>()?
            .checked_add(1)
            .context("clipboard clock exhausted")?,
    );
    let legacy: i64 = tx.query_row(
        "SELECT last_outgoing_at FROM local_clock WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let created_at = now.max(
        (legacy as u64)
            .checked_add(1)
            .context("clipboard timestamp exhausted")?,
    );
    ensure!(
        created_at <= i64::MAX as u64,
        "clipboard timestamp exhausted"
    );
    tx.execute(
        "UPDATE mesh_clock SET last_seen=? WHERE singleton=1",
        [hlc.to_string()],
    )?;
    tx.execute(
        "UPDATE local_clock SET last_outgoing_at=? WHERE singleton=1",
        [created_at as i64],
    )?;
    Ok((created_at, hlc))
}
fn stamp_eligible(c: &Connection, hlc: u64, source: &Id) -> Result<bool> {
    let applied: Option<(Option<String>, String)> = c
        .query_row(
            "SELECT hlc,source_device FROM clipboard_clock WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match applied {
        Some((Some(previous), device)) => {
            Ok((hlc, source.as_str()) > (previous.parse::<u64>()?, device.as_str()))
        }
        _ => Ok(true),
    }
}
fn apply_stamp(tx: &Transaction<'_>, hlc: u64, source: &Id, created_at: u64) -> Result<()> {
    observe_hlc(tx, hlc)?;
    if stamp_eligible(tx, hlc, source)? {
        tx.execute("INSERT INTO clipboard_clock(singleton,created_at,source_device,hlc) VALUES(1,?,?,?) ON CONFLICT(singleton) DO UPDATE SET created_at=excluded.created_at,source_device=excluded.source_device,hlc=excluded.hlc", params![created_at as i64,source.as_str(),hlc.to_string()])?;
    }
    Ok(())
}
fn record_text(
    tx: &Transaction<'_>,
    device: &Id,
    text: &Clipboard,
    application_hlc: Option<u64>,
    applied: bool,
    key: &crate::clipboard_fingerprint::FingerprintKey,
) -> Result<()> {
    if applied {
        if text.hlc.is_none() {
            tx.execute("INSERT INTO legacy_clipboard_clock VALUES (?,?) ON CONFLICT(device) DO UPDATE SET created_at=max(created_at,excluded.created_at)", params![device.as_str(), text.created_at as i64])?;
            // Legacy peers keep their per-sender gate; their successful OS effect
            // fences every queued mesh operation with a local observation stamp.
        }
        let hlc = match application_hlc.or(text.hlc) {
            Some(hlc) => hlc,
            None => allocate_stamp(tx)?.1,
        };
        apply_stamp(tx, hlc, device, text.created_at)?;
    }
    tx.execute(
        "INSERT INTO clipboard_recent VALUES (?,?,?)",
        params![
            device.as_str(),
            text.transfer_id.as_str(),
            serde_json::to_string(&Metadata::Text(keyed_text_receipt(key, text)))?
        ],
    )?;
    tx.execute("DELETE FROM clipboard_recent WHERE device=? AND rowid NOT IN (SELECT rowid FROM clipboard_recent WHERE device=? ORDER BY rowid DESC LIMIT ?)", params![device.as_str(), device.as_str(), RECENT_TEXT_LIMIT as i64])?;
    Ok(())
}
impl Store {
    pub(crate) fn text_receipt(&self, text: &Clipboard) -> TextReceipt {
        keyed_text_receipt(&self.clipboard_fingerprint_key, text)
    }
    pub fn open(path: &Path) -> Result<Self> {
        #[cfg(windows)]
        let (mut conn, lock, folder) = crate::platform::fs::open_database(path)?;
        #[cfg(unix)]
        let (mut conn, lock) = {
            ensure!(
                !path.exists() || !fs::symlink_metadata(path)?.file_type().is_symlink(),
                "database is a symlink"
            );
            let lock = crate::platform::fs::open_store_lock(&path.with_extension("lock"))?;
            lock.try_lock()
                .map_err(crate::platform::fs::lock_error)
                .context("data store is already owned by another daemon")?;
            #[cfg(target_os = "macos")]
            {
                // Precreate the database before SQLite can write secret state. New
                // sidecars inherit from this private parent, existing ones must pass.
                let parent =
                    crate::storage::Folder::open(path.parent().context("database parent")?, false)?;
                crate::platform::private::check(&parent.file)?;
                crate::platform::private::open(path)?;
                for suffix in ["-wal", "-shm", "-journal"] {
                    let sidecar = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
                    match fs::symlink_metadata(&sidecar) {
                        Ok(_) => {
                            #[cfg(target_os = "macos")]
                            crate::platform::private::check_path(&sidecar)?;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            let conn = Connection::open(path)?;
            crate::platform::fs::set_private_file(path)?;
            (conn, lock)
        };
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;")?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(version <= 4, "unsupported store schema version {version}");
        // All schema changes and legacy cleanup are one transaction. A failed
        // migration leaves the existing pairing, receipts and outbox intact.
        let tx = conn.transaction()?;
        tx.execute_batch("
          CREATE TABLE IF NOT EXISTS peers(id TEXT PRIMARY KEY, fp TEXT UNIQUE NOT NULL);
          CREATE TABLE IF NOT EXISTS receipts(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,id TEXT NOT NULL,meta TEXT NOT NULL,destination TEXT,PRIMARY KEY(device,id));
          CREATE TABLE IF NOT EXISTS outgoing(seq INTEGER PRIMARY KEY AUTOINCREMENT,device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,payload TEXT NOT NULL,retry_at INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS clipboard_recent(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,id TEXT NOT NULL,meta TEXT NOT NULL,PRIMARY KEY(device,id));
          CREATE TABLE IF NOT EXISTS failed_cache(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,id TEXT NOT NULL,meta TEXT NOT NULL,destination TEXT NOT NULL,PRIMARY KEY(device,id));
          CREATE TABLE IF NOT EXISTS staging_folders(path TEXT PRIMARY KEY);
          CREATE TABLE IF NOT EXISTS clipboard_fingerprints(seq INTEGER PRIMARY KEY,meta TEXT NOT NULL);")?;
        // Keep earlier folder hints before retiring receipt recovery columns.
        // They authorize disposable-root sweeping, never deletion of final files.
        let old_locations: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('receipts') WHERE name='location')",
            [],
            |r| r.get(0),
        )?;
        if old_locations {
            tx.execute_batch("INSERT OR IGNORE INTO staging_folders SELECT json_extract(location,'$.folder') FROM receipts WHERE json_valid(location) AND json_type(location,'$.folder')='text';")?;
        }
        let legacy_complete: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('receipts') WHERE name='complete')",
            [],
            |r| r.get(0),
        )?;
        if legacy_complete {
            tx.execute("DELETE FROM receipts WHERE complete=0", [])?;
        }
        // Preserve the timestamp floor before retiring legacy queued text.
        let legacy_outgoing_floor: i64 = tx.query_row(
            "SELECT coalesce(max(json_extract(payload,'$.created_at')),0) FROM outgoing",
            [],
            |r| r.get(0),
        )?;
        // Retire earlier pending/rollback receipts without touching user files.
        // Preserve completed file dedupe, but clipboard history has a bounded horizon.
        tx.execute_batch(
            "DROP TABLE IF EXISTS stages;
            DELETE FROM receipts WHERE json_extract(meta,'$.operation')='Text';
            DELETE FROM outgoing;",
        )?;
        for column in ["staging", "location", "rolling_back", "complete"] {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('receipts') WHERE name=?)",
                [column],
                |r| r.get(0),
            )?;
            if exists {
                tx.execute_batch(&format!("ALTER TABLE receipts DROP COLUMN {column}"))?;
            }
        }
        let legacy_blocked: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('outgoing') WHERE name='blocked')",
            [],
            |r| r.get(0),
        )?;
        if legacy_blocked {
            tx.execute_batch(
                "DELETE FROM outgoing WHERE blocked=1; ALTER TABLE outgoing DROP COLUMN blocked;",
            )?;
        }
        tx.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS latest_text ON outgoing(device);")?;
        if version == 0 {
            tx.execute_batch("CREATE TABLE IF NOT EXISTS pairing_epoch(singleton INTEGER PRIMARY KEY CHECK(singleton=1),generation INTEGER NOT NULL,last_outgoing_at INTEGER NOT NULL DEFAULT 0);
                INSERT OR IGNORE INTO pairing_epoch(singleton,generation) VALUES(1,0);
                ALTER TABLE peers ADD COLUMN role TEXT NOT NULL DEFAULT 'phone' CHECK(role IN ('phone','computer'));
                ALTER TABLE peers ADD COLUMN name TEXT NOT NULL DEFAULT '';
                ALTER TABLE peers ADD COLUMN addresses TEXT NOT NULL DEFAULT '[]';
                ALTER TABLE peers ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;
                UPDATE peers SET generation=(SELECT generation FROM pairing_epoch WHERE singleton=1);
                CREATE TABLE peer_generations(device TEXT PRIMARY KEY,generation INTEGER NOT NULL);
                INSERT INTO peer_generations SELECT id,generation FROM peers;
                CREATE TABLE local_clock(singleton INTEGER PRIMARY KEY CHECK(singleton=1),last_outgoing_at INTEGER NOT NULL);
                INSERT INTO local_clock SELECT singleton,last_outgoing_at FROM pairing_epoch;
                CREATE TABLE IF NOT EXISTS clipboard_clock(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,created_at INTEGER NOT NULL);
                ALTER TABLE clipboard_clock RENAME TO legacy_clipboard_clock;
                CREATE TABLE clipboard_clock(singleton INTEGER PRIMARY KEY CHECK(singleton=1),created_at INTEGER NOT NULL,source_device TEXT NOT NULL,hlc TEXT);
                INSERT INTO clipboard_clock(singleton,created_at,source_device) SELECT 1,created_at,device FROM legacy_clipboard_clock ORDER BY created_at DESC,device DESC LIMIT 1;
                CREATE TABLE peer_settings(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,key TEXT NOT NULL,enabled INTEGER NOT NULL,hlc TEXT NOT NULL,writer_device TEXT NOT NULL,PRIMARY KEY(device,key));
                DROP TABLE pairing_epoch;
                PRAGMA user_version=1;")?;
        }
        if version < 2 {
            // Evidence belongs to this exact row/generation. Never infer READY
            // from a HELLO or migrate unconfirmed pairings as established.
            tx.execute_batch("ALTER TABLE peers ADD COLUMN established INTEGER NOT NULL DEFAULT 0 CHECK(established IN (0,1));
                PRAGMA user_version=2;")?;
        }
        // Repair an unfenced v1 database that a released binary has opened.
        // Its initial schema batch may have recreated the global epoch table.
        // Retain its outgoing clock and never lower any surviving peer's epoch.
        let legacy_epoch: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='pairing_epoch' AND type='table')",
            [], |r| r.get(0),
        )?;
        if legacy_epoch {
            tx.execute_batch("UPDATE local_clock SET last_outgoing_at=max(last_outgoing_at,coalesce((SELECT last_outgoing_at FROM pairing_epoch WHERE singleton=1),0));
                DROP TABLE pairing_epoch;")?;
        }
        tx.execute_batch("UPDATE peers SET established=0 WHERE generation<coalesce((SELECT generation FROM peer_generations WHERE device=peers.id),0);
            UPDATE peers SET generation=max(generation,coalesce((SELECT generation FROM peer_generations WHERE device=peers.id),0));
            INSERT INTO peer_generations SELECT id,generation FROM peers WHERE true ON CONFLICT(device) DO UPDATE SET generation=max(peer_generations.generation,excluded.generation);")?;
        // Released 2.0.2 unconditionally INSERTs into pairing_epoch in its first
        // startup batch, BEFORE pruning peers/receipts. A read-only view makes
        // that INSERT fail, even though released code ignores user_version.
        // Keep this fence on every open, including recovery of an unfenced v1 DB.
        tx.execute_batch("CREATE VIEW IF NOT EXISTS pairing_epoch AS SELECT singleton,0 AS generation,last_outgoing_at FROM local_clock;")?;
        tx.execute(
            "UPDATE local_clock SET last_outgoing_at=max(last_outgoing_at,?) WHERE singleton=1",
            [legacy_outgoing_floor],
        )?;
        // Decimal TEXT preserves the full unsigned wire HLC range in SQLite.
        tx.execute_batch("CREATE TABLE IF NOT EXISTS mesh_clock(singleton INTEGER PRIMARY KEY CHECK(singleton=1),last_seen TEXT NOT NULL);
            INSERT OR IGNORE INTO mesh_clock VALUES(1,'0');")?;
        let legacy_floor: i64 = tx.query_row(
            "SELECT last_outgoing_at FROM local_clock WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        observe_hlc(&tx, legacy_floor as u64)?;
        let applied: Option<String> = tx
            .query_row(
                "SELECT hlc FROM clipboard_clock WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(applied) = applied {
            observe_hlc(&tx, applied.parse()?)?;
        }
        let generations = {
            let mut q = tx.prepare("SELECT device,generation FROM peer_generations")?;
            q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .map(|row| {
                    let (id, generation) = row?;
                    Ok((Id::new(id)?, generation as u64))
                })
                .collect::<Result<HashMap<_, _>>>()?
        };
        tx.execute_batch("CREATE TABLE IF NOT EXISTS settings_support(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,supported INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS settings_receipts(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,id TEXT NOT NULL,meta TEXT NOT NULL,PRIMARY KEY(device,id));
            CREATE INDEX IF NOT EXISTS settings_receipts_sequence ON settings_receipts(device);
            CREATE TABLE IF NOT EXISTS settings_outgoing(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS settings_replay(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS settings_confirmed(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,key TEXT NOT NULL,enabled INTEGER NOT NULL,hlc TEXT NOT NULL,writer_device TEXT NOT NULL,PRIMARY KEY(device,key));")?;
        outbox::migrate(&tx)?;
        let setup = members::migrate(&tx)?;
        // Policy is authoritative before Core can expose any clipboard offers.
        let pair_settings = {
            let mut q = tx.prepare("SELECT id FROM peers")?;
            let ids = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids.into_iter()
                .map(|id| {
                    let id = Id::new(id)?;
                    let fields = settings::fields(&tx, &id)?;
                    Ok((id, fields))
                })
                .collect::<Result<HashMap<_, _>>>()?
        };
        #[cfg(unix)]
        let clipboard_fingerprint_key = {
            crate::clipboard_fingerprint::FingerprintKey::cleanup_temporary(
                &path.with_file_name("clipboard-fingerprint.key"),
            )?;
            crate::clipboard_fingerprint::FingerprintKey::load_or_create(
                &path.with_file_name("clipboard-fingerprint.key"),
            )?
        };
        #[cfg(windows)]
        let clipboard_fingerprint_key =
            crate::clipboard_fingerprint::FingerprintKey::load_in(&folder)?;
        clipboard::migrate_fingerprints(&tx, &clipboard_fingerprint_key)?;
        tx.commit()?;
        // Remove obsolete raw hashes from the migration WAL as well.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        #[cfg(unix)]
        let folder = Folder::open(path.parent().context("database parent")?, false)?;
        folder.file.sync_all()?;
        Ok(Self {
            setup: Arc::new(Mutex::new(Some(setup))),
            setup_remote_scopes: Arc::new(Mutex::new(HashMap::new())),
            setup_changed: tokio::sync::watch::channel(0).0,
            connection: Arc::new(Mutex::new(conn)),
            pair_settings: Arc::new(Mutex::new(pair_settings)),
            settings_changed: tokio::sync::watch::channel(0).0,
            settings_generations: Arc::new(Mutex::new(HashMap::new())),
            _ownership: Arc::new(StoreOwnership(lock)),
            folder,
            reserve: Arc::new(AtomicU64::new(16 * 1024 * 1024)),
            work: Arc::new(Work::default()),
            connection_id: None,
            action_connection_id: None,
            mesh: false,
            setup_generation: None,
            local_device: None,
            generations: Arc::new(Mutex::new(generations)),
            peer_generations: None,
            staging_ready: Arc::new(Mutex::new(HashSet::new())),
            cache_owners: Arc::new(Mutex::new(HashMap::new())),
            clipboard_history_revision: Arc::new(AtomicU64::new(0)),
            clipboard_clock_slots: Arc::new(tokio::sync::Semaphore::new(2)),
            clipboard_fingerprint_key,
            abandoned_claims: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            peer_write_barrier: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            faults: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    fn retry_abandoned_claims(&self) -> Result<()> {
        let pending: Vec<_> = self
            .abandoned_claims
            .lock()
            .expect("abandoned text claims")
            .iter()
            .map(|((peer, id), claim)| (peer.clone(), id.clone(), claim.clone()))
            .collect();
        for (peer, id, claim) in pending {
            release_text_claim(self, claim.epoch, &peer, &id, &claim.token)?;
        }
        Ok(())
    }
    /// Per-store, per-operation faults are shared by captured session handles.
    /// They simulate local I/O failure without changing a real database or disk.
    #[cfg(test)]
    pub(crate) fn fail_next(&self, site: &'static str, count: usize) {
        self.faults.lock().unwrap().insert(site, count);
    }
    #[cfg(test)]
    pub(crate) fn faults_remaining(&self, site: &'static str) -> usize {
        *self.faults.lock().unwrap().get(site).unwrap_or(&0)
    }
    #[cfg(test)]
    pub(crate) fn fault(&self, site: &'static str) -> Result<()> {
        let mut faults = self.faults.lock().unwrap();
        if let Some(left) = faults.get_mut(site)
            && *left > 0
        {
            *left -= 1;
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                Some(format!("injected storage failure: {site}")),
            )
            .into());
        }
        Ok(())
    }
    #[cfg(all(test, windows))]
    pub(crate) fn test_locked<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        self.locked(f)
    }
    fn locked<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        f(&mut *self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?)
    }
    /// Core calls this while holding its pairing gate during registration/IPC
    /// admission; every operation from that session keeps the same authority.
    pub(crate) fn with_current_pairing(&self) -> Self {
        let mut store = self.clone();
        store.peer_generations = Some(self.generations.lock().expect("peer generations").clone());
        store
    }
    pub(crate) fn for_next_pairing(&self, device: &Id) -> Self {
        let mut captured = self.with_current_pairing();
        let generation = captured
            .pairing_generation(device)
            .unwrap_or(0)
            .checked_add(1)
            .expect("pairing generation exhausted");
        captured
            .peer_generations
            .as_mut()
            .expect("captured generations")
            .insert(device.clone(), generation);
        captured
    }
    /// Durable epoch, including revocation tombstones. Callers must also verify
    /// that the device is still paired before accepting cached presentation data.
    pub(crate) fn pairing_generation(&self, device: &Id) -> Option<u64> {
        match &self.peer_generations {
            Some(generations) => generations.get(device).copied(),
            None => self
                .generations
                .lock()
                .expect("peer generations")
                .get(device)
                .copied(),
        }
    }
    fn peer_epoch(&self, device: &Id) -> PeerEpoch {
        let generation = self.pairing_generation(device);
        PeerEpoch {
            device: device.clone(),
            generation: generation.unwrap_or(u64::MAX),
        }
    }
    /// All writes to peer-owned state validate authority in the transaction that
    /// performs the write. Holding only this short mutex/transaction also keeps
    /// file placement atomic against pairing changes without delivery leases.
    fn peer_write<T>(
        &self,
        epoch: PeerEpoch,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        #[cfg(test)]
        {
            let barrier = self.peer_write_barrier.lock().unwrap().take();
            if let Some(barrier) = barrier {
                let _ = barrier.entered.send(());
                barrier.release.recv()?;
            }
        }
        self.locked(|c| {
            let tx = c.transaction()?;
            epoch.check(&tx)?;
            let result = f(&tx)?;
            tx.commit()?;
            Ok(result)
        })
    }
    #[cfg(test)]
    fn delay_next_peer_write(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        *self.peer_write_barrier.lock().unwrap() = Some(PeerWriteBarrier {
            entered,
            release: blocking,
        });
        (wait, release)
    }
    #[cfg(test)]
    pub(crate) fn active_jobs(&self) -> usize {
        self.work.active()
    }
    #[cfg(test)]
    pub(crate) fn admitted_jobs(&self) -> usize {
        self.work.admitted()
    }
    pub(crate) async fn drain(&self) {
        self.work.drain().await;
    }
    pub(crate) async fn reserve(&self) -> Reservation {
        self.work.reserve().await
    }
    pub(crate) fn submit<T: Send + 'static>(
        &self,
        reservation: Reservation,
        f: impl FnOnce(&Self) -> Result<T> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<T>> {
        let store = self.clone();
        reservation.submit(move || f(&store))
    }
    /// Cleanup cannot await admission in Drop or compete for a permit held by
    /// the job dropping its result. Each bounded producer owns finite cleanup.
    pub(crate) fn start_cleanup<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Self) -> Result<T> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<T>> {
        self.start_completion(f)
    }
    /// Finish an effect whose bounded producer has already applied/observed it.
    /// No admission wait is allowed between that effect and durable submission.
    pub(crate) fn start_completion<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Self) -> Result<T> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<T>> {
        let store = self.clone();
        start_blocking(vec![self.work.clone()], move || f(&store))
    }
    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Self) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.submit(self.reserve().await, f).await?
    }
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.blocking(move |store| store.locked(f)).await
    }
    pub async fn admission(&self) -> Result<()> {
        self.blocking(|store| {
            #[cfg(test)]
            store.fault("admission")?;
            capacity(&store.folder, store.reserve.load(Ordering::SeqCst))
        })
        .await
    }
    pub async fn peers(&self) -> Result<HashMap<Hash, Id>> {
        self.call(|c| {
            let mut q = c.prepare("SELECT fp,id FROM peers")?;
            let mut peers = HashMap::new();
            for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (fp, id) = row?;
                peers.insert(Hash::new(fp)?, Id::new(id)?);
            }
            Ok(peers)
        })
        .await
    }
    pub(crate) async fn peer_info(&self) -> Result<Vec<PeerInfo>> {
        #[cfg(test)]
        self.fault("peer_info")?;
        self.call(|c| {
            let mut q =
                c.prepare("SELECT id,fp,generation,role,name,addresses FROM peers ORDER BY id")?;
            q.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .map(|row| {
                let (id, fp, generation, role, name, addresses) = row?;
                let addresses: Vec<String> = serde_json::from_str(&addresses)?;
                ensure!(
                    addresses.len() <= 16
                        && addresses
                            .iter()
                            .all(|a| pneu_proto::validate_address(a).is_ok()),
                    "invalid saved addresses"
                );
                Ok(PeerInfo {
                    id: Id::new(id.clone())?,
                    fingerprint: Hash::new(fp)?,
                    generation: generation as u64,
                    role,
                    name,
                    addresses,
                    route_proven: members::route_proven(c, &Id::new(id)?)?,
                })
            })
            .collect()
        })
        .await
    }
    pub(crate) async fn mark_peer_established(&self, device: Id) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            #[cfg(test)]
            store.fault("mark_peer_established")?;
            store.peer_write(epoch, |tx| {
                tx.execute(
                    "UPDATE peers SET established=1 WHERE id=?",
                    [device.as_str()],
                )?;
                Ok(())
            })
        })
        .await
    }
    pub(crate) async fn peer_established(&self, device: Id) -> Result<bool> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            #[cfg(test)]
            store.fault("peer_established")?;
            store.peer_write(epoch, |tx| {
                Ok(tx.query_row(
                    "SELECT established FROM peers WHERE id=?",
                    [device.as_str()],
                    |row| row.get(0),
                )?)
            })
        })
        .await
    }
    pub(crate) async fn remember_route(&self, device: Id, address: String) -> Result<()> {
        pneu_proto::validate_address(&address)?;
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            #[cfg(test)]
            store.fault("remember_route")?;
            store.peer_write(epoch, |tx| {
                let previous: String = tx.query_row(
                    "SELECT addresses FROM peers WHERE id=?",
                    [device.as_str()],
                    |r| r.get(0),
                )?;
                let mut addresses: Vec<String> = serde_json::from_str(&previous)?;
                addresses.retain(|a| a != &address);
                addresses.insert(0, address);
                addresses.truncate(16);
                tx.execute(
                    "UPDATE peers SET addresses=? WHERE id=?",
                    params![serde_json::to_string(&addresses)?, device.as_str()],
                )?;
                tx.execute(
                    "UPDATE setup_links SET route_proven=1 WHERE device=?",
                    [device.as_str()],
                )?;
                Ok(())
            })
        })
        .await
    }
    /// Presentation/address hints never replace captured trust. Await bounded
    /// submission so replacing a connection cancels its unscheduled hints.
    pub(crate) async fn remember_peer_hello(
        &self,
        mut hello: pneu_proto::Hello,
        current: Arc<AtomicU64>,
        connection: u64,
        outbound: bool,
    ) -> Result<()> {
        normalize_legacy_hello(&mut hello, outbound);
        let epoch = self.peer_epoch(&hello.device_id);
        let result = self
            .blocking(move |store| {
                #[cfg(test)]
                store.fault("remember_peer_hello")?;
                store.peer_write(epoch, |tx| {
                    if current.load(Ordering::SeqCst) != connection {
                        return Ok(());
                    }
                    tx.execute("INSERT INTO outbox_capabilities SELECT id,generation,? FROM peers WHERE id=? ON CONFLICT(device) DO UPDATE SET generation=excluded.generation,files=excluded.files",params![hello.capabilities.iter().any(|c|c=="files"),hello.device_id.as_str()])?;
                    tx.execute("INSERT INTO settings_support VALUES(?,?) ON CONFLICT(device) DO UPDATE SET supported=excluded.supported",params![hello.device_id.as_str(),hello.capabilities.iter().any(|c|c=="mesh")])?;
                    let role = match hello.role {
                        Some(pneu_proto::DeviceRole::Computer) => "computer",
                        _ => "phone",
                    };
                    let previous: String = tx.query_row(
                        "SELECT addresses FROM peers WHERE id=?",
                        [hello.device_id.as_str()],
                        |r| r.get(0),
                    )?;
                    let cached: Vec<String> = if !outbound && !hello.capabilities.iter().any(|c| c == "mesh") {
                        // A legacy inbound peer is a phone, so historical
                        // unnegotiated route hints have no routing authority.
                        Vec::new()
                    } else {
                        serde_json::from_str(&previous)?
                    };
                    let proven=members::route_proven(tx,&hello.device_id)?&&!cached.is_empty();
                    let mut addresses: Vec<String> = if proven {cached.first().cloned().into_iter().collect()}else{Vec::new()};
                    for address in hello
                        .addresses
                        .unwrap_or_default()
                        .into_iter()
                        .chain(cached.into_iter().skip(usize::from(proven)))
                    {
                        if pneu_proto::validate_address(&address).is_ok()
                            && !addresses.contains(&address)
                            && addresses.len() < 16
                        {
                            addresses.push(address);
                        }
                    }
                    tx.execute("UPDATE setup_links SET route_proven=? WHERE device=?",params![proven,hello.device_id.as_str()])?;
                    tx.execute(
                        "UPDATE peers SET name=?,role=?,addresses=? WHERE id=?",
                        params![
                            hello.name,
                            role,
                            serde_json::to_string(&addresses)?,
                            hello.device_id.as_str()
                        ],
                    )?;
                    Ok(())
                })
            })
            .await;
        if let Err(error) = &result
            && error.downcast_ref::<PairingRevoked>().is_none()
        {
            eprintln!("peer metadata unavailable: {error}");
        }
        result
    }
    #[cfg(test)]
    pub async fn pair(&self, device: Id, fp: Hash) -> Result<()> {
        self.pair_then(device, fp, || {}).await
    }
    #[cfg(test)]
    pub(crate) async fn pair_then(
        &self,
        device: Id,
        fp: Hash,
        finish: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        self.pair_then_hello(device, fp, None, finish).await
    }
    #[cfg(test)]
    pub(crate) async fn pair_then_hello(
        &self,
        device: Id,
        fp: Hash,
        hello: Option<pneu_proto::Hello>,
        finish: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        self.pair_then_hello_scoped(
            device,
            fp,
            hello,
            None,
            pneu_proto::SetupScope::Ask,
            false,
            finish,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn pair_then_hello_scoped(
        &self,
        device: Id,
        fp: Hash,
        hello: Option<pneu_proto::Hello>,
        local: Option<pneu_proto::Member>,
        scope: pneu_proto::SetupScope,
        route_proven: bool,
        finish: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        self.blocking(move |store| {
            #[cfg(test)]
                store.fault("pair_then_hello")?;
                store.locked(|c| {
                capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                let tx = c.transaction()?;
                let previous: i64 = tx.query_row("SELECT coalesce((SELECT generation FROM peer_generations WHERE device=?),0)", [device.as_str()], |r| r.get(0))?;
                let generation = previous.checked_add(1).context("pair generation exhausted")?;
                // A fingerprint cannot silently displace a different device ID.
                // Re-pairing this ID resets its delivery state, preserving settings.
                let settings: Vec<(String,bool,String,String)> = {
                    let mut q = tx.prepare("SELECT key,enabled,hlc,writer_device FROM peer_settings WHERE device=?")?;
                    q.query_map([device.as_str()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
                };
                let previous_setup = members::state(&tx)?.links.into_iter().find(|l|l.device==device);
                tx.execute("DELETE FROM members_receipts WHERE device=?",[device.as_str()])?;
                tx.execute("DELETE FROM peers WHERE id=?", [device.as_str()])?;
                let role = if hello.as_ref().is_some_and(|h| h.role == Some(pneu_proto::DeviceRole::Computer)) { "computer" } else { "phone" };
                let addresses = hello.as_ref().and_then(|h| h.addresses.clone()).unwrap_or_default();
                ensure!(addresses.len() <= 16 && addresses.iter().all(|a| pneu_proto::validate_address(a).is_ok()), "invalid initial routes");
                tx.execute("INSERT INTO peers(id,fp,generation,role,name,addresses) VALUES(?,?,?,?,?,?)",params![device.as_str(),fp.as_str(),generation,role,hello.as_ref().map(|h| h.name.as_str()).unwrap_or_default(),serde_json::to_string(&addresses)?])?;
                for (key,enabled,hlc,writer) in settings {
                    tx.execute("INSERT INTO peer_settings VALUES(?,?,?,?,?)",params![device.as_str(),key,enabled,hlc,writer])?;
                }
                tx.execute("INSERT INTO peer_generations VALUES(?,?) ON CONFLICT(device) DO UPDATE SET generation=excluded.generation",params![device.as_str(),generation])?;
                if let (Some(local),Some(hello)) = (&local,&hello) { members::paired(&tx,local,hello,&fp,scope,previous_setup)?; }
                else { tx.execute("INSERT INTO setup_links(device,scope) VALUES(?,'ask')",[device.as_str()])?; }
                if route_proven&&!addresses.is_empty() {tx.execute("UPDATE setup_links SET route_proven=1 WHERE device=?",[device.as_str()])?;}
                let setup = members::state(&tx)?;
                tx.commit()?;
                let mut remote_scopes = store.setup_remote_scopes.lock().expect("remote setup scopes");
                remote_scopes.remove(&device);
                if let Some(remote) = hello.as_ref().filter(|h| h.capabilities.iter().any(|c| c == "setup")).and_then(|h| h.setup.as_ref()) {
                    remote_scopes.insert(device.clone(), (generation as u64, remote.scope));
                }
                drop(remote_scopes);
                *store.setup.lock().expect("setup") = Some(setup);
                store.setup_changed.send_modify(|n|*n=n.wrapping_add(1));
                store.generations.lock().expect("peer generations").insert(device, generation as u64);
                Ok(())
            })?;
            finish();
            Ok(())
        })
        .await
    }
    #[cfg(test)]
    pub(crate) async fn unpair_then(
        &self,
        device: Id,
        finish: impl FnOnce(bool) + Send + 'static,
    ) -> Result<()> {
        self.unpair_reserved(self.reserve().await, device, finish)
            .await
    }
    pub(crate) fn unpair_reserved(
        &self,
        reservation: Reservation,
        device: Id,
        finish: impl FnOnce(bool) + Send + 'static,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        let epoch = self.peer_epoch(&device);
        let job = self.submit(reservation, move |store| {
            let result = store.locked(|c| {
                #[cfg(test)]
                store.fault("unpair_reserved")?;
                let tx = c.transaction()?;
                epoch.check(&tx)?;
                tx.execute(
                    "DELETE FROM members_receipts WHERE device=?",
                    [device.as_str()],
                )?;
                ensure!(
                    tx.execute("DELETE FROM peers WHERE id=?", [device.as_str()])? == 1,
                    "pairing was not removed"
                );
                tx.execute(
                    "UPDATE peer_generations SET generation=generation+1 WHERE device=?",
                    [device.as_str()],
                )?;
                let generation: i64 = tx.query_row(
                    "SELECT generation FROM peer_generations WHERE device=?",
                    [device.as_str()],
                    |r| r.get(0),
                )?;
                let setup = members::state(&tx)?;
                tx.commit()?;
                store
                    .setup_remote_scopes
                    .lock()
                    .expect("remote setup scopes")
                    .remove(&device);
                *store.setup.lock().expect("setup") = Some(setup);
                store.setup_changed.send_modify(|n| *n = n.wrapping_add(1));
                store
                    .pair_settings
                    .lock()
                    .expect("pair settings")
                    .remove(&device);
                store
                    .settings_generations
                    .lock()
                    .expect("settings generations")
                    .remove(&device);
                store
                    .generations
                    .lock()
                    .expect("peer generations")
                    .insert(device, generation as u64);
                Ok(())
            });
            // The callback owns lifecycle restoration and the pairing gate even
            // when the awaiting async command has been canceled.
            finish(result.is_ok());
            result
        });
        async move { job.await? }
    }
    pub async fn receipt(&self, device: Id, id: Id) -> Result<Option<(Metadata, bool)>> {
        #[cfg(test)]
        self.fault("receipt")?;
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.retry_abandoned_claims()?;
            store.locked(|c| {
                epoch.check(c)?;
                receipt_row(c, device.as_str(), id.as_str())
            })
        })
        .await
    }
    /// Persist discovery before creating a staging directory or file. Startup
    /// sweeps every ever-used folder, independent of the current configuration.
    pub(crate) async fn create_stage(
        &self,
        path: PathBuf,
        destination: String,
        size: u64,
    ) -> Result<Stage> {
        self.blocking(move |store| {
            #[cfg(test)]
            store.fault("create_stage")?;
            store.locked(|c| {
                capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                c.execute(
                    "INSERT OR IGNORE INTO staging_folders VALUES (?)",
                    [path.to_str().context("UTF-8 folder")?],
                )?;
                Ok(())
            })?;
            let folder = Folder::open(&path, true)?;
            // Retry deferred startup cleanup before admitting a new transfer.
            // Serialize only the first sweep/creation, never sweep a live stage.
            let mut ready = store.staging_ready.lock().expect("staging readiness");
            let identity = folder.identity()?;
            if !ready.contains(&identity) {
                clean_staging(&folder)?;
            }
            let directory = staging_directory(&folder)?;
            let stage = Stage::create(folder, directory, destination, size)?;
            *stage.0.cleanup.lock().expect("stage cleanup") = Some(crate::storage::Cleanup {
                store: store.clone(),
                _work: crate::work::track(vec![store.work.clone()]),
            });
            ready.insert(identity);
            Ok(stage)
        })
        .await
    }
    pub(crate) async fn clean_staging(
        &self,
        paths: Vec<PathBuf>,
    ) -> Result<Vec<(PathBuf, String)>> {
        self.blocking(move |store| {
            let paths = store.locked(|c| {
                for path in paths {
                    c.execute(
                        "INSERT OR IGNORE INTO staging_folders VALUES (?)",
                        [path.to_str().context("UTF-8 folder")?],
                    )?;
                }
                let mut q = c.prepare("SELECT path FROM staging_folders")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })?;
            let mut failures = Vec::new();
            let mut ready = store.staging_ready.lock().expect("staging readiness");
            for path in paths.into_iter().map(PathBuf::from) {
                let result = Folder::open(&path, false).and_then(|folder| {
                    let identity = folder.identity()?;
                    if !ready.contains(&identity) {
                        clean_staging(&folder)?;
                    }
                    Ok(identity)
                });
                match result {
                    Ok(identity) => {
                        ready.insert(identity);
                    }
                    // Missing volumes/never-created folders stay deferred. If
                    // they appear, sweep before creating the first live stage.
                    Err(e) if crate::platform::fs::is_missing(&e) => {}
                    Err(e) => failures.push((path, e.to_string())),
                }
            }
            Ok(failures)
        })
        .await
    }
    pub async fn discard_stage(&self, stage: Stage) -> Result<()> {
        self.blocking(move |store| {
            #[cfg(not(test))]
            let _ = store;
            #[cfg(test)]
            store.fault("discard_stage")?;
            stage.cleanup()
        })
        .await
    }
    pub async fn commit_file(
        &self,
        device: Id,
        verified: VerifiedFile,
        stage: Stage,
    ) -> Result<bool> {
        self.commit_file_checked(device, verified, stage, None)
            .await
    }
    pub(crate) async fn commit_saved_file(
        &self,
        device: Id,
        verified: VerifiedFile,
        stage: Stage,
        cache: PathBuf,
    ) -> Result<bool> {
        self.commit_file_checked(device, verified, stage, Some(cache))
            .await
    }
    async fn commit_file_checked(
        &self,
        device: Id,
        verified: VerifiedFile,
        stage: Stage,
        cache: Option<PathBuf>,
    ) -> Result<bool> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            #[cfg(test)]
            store.fault("commit_file_checked")?;
            let begin = verified.metadata();
            ensure!(
                stage.0.file.metadata()?.len() == begin.size,
                "staging size mismatch"
            );
            stage.0.file.sync_all()?;
            let mut obsolete_cache = None;
            // Serialize journal changes and ownership publication/retirement.
            let mut owners = store.cache_owners.lock().expect("cache ownership");
            let key = (device.clone(), begin.transfer_id.clone());
            let result = store.peer_write(epoch, |tx| {
                if let Some((meta, _)) =
                    receipt_row(tx, device.as_str(), begin.transfer_id.as_str())?
                {
                    ensure!(meta == Metadata::File(begin.clone()), ReceiptConflict);
                    return Ok(false);
                }
                validate_failed_file(tx, &device, &begin)?;
                capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                if let Some(cache) = &cache {
                    // Use the retained directory's current path, so a folder
                    // moved into the cache during a transfer cannot become a save.
                    #[cfg(windows)]
                    let overlaps = crate::platform::fs::retained_folder_overlaps_cache(
                        &stage.0.folder,
                        cache,
                    )?;
                    #[cfg(unix)]
                    let overlaps =
                        crate::config::save_folder_overlaps_cache(&stage.0.folder.path()?, cache);
                    ensure!(!overlaps, crate::platform::fs::permission_denied());
                }
                store.check_file_setting(tx, &device, begin.kind)?;
                stage.place()?;
                tx.execute(
                    "INSERT INTO receipts(device,id,meta,destination) VALUES (?,?,?,?)",
                    params![
                        device.as_str(),
                        begin.transfer_id.as_str(),
                        serde_json::to_string(&Metadata::File(begin.clone()))?,
                        stage.destination_path()?.to_str()
                    ],
                )?;
                if let Some(path) = failed_file(tx, &device, &begin)?.map(|(_, path)| path) {
                    tx.execute(
                        "DELETE FROM failed_cache WHERE device=? AND id=?",
                        params![device.as_str(), begin.transfer_id.as_str()],
                    )?;
                    obsolete_cache = Some(path);
                }
                Ok(true)
            });
            if matches!(result, Ok(true))
                && let Some(path) = obsolete_cache
            {
                // A journal pathname alone never authorises deletion. Keep the
                // original directory and verify the recorded inode/metadata.
                if let Some(owner) = owners.remove(&key)
                    && owner.path == path
                {
                    let _ = owner.file.remove();
                }
            }
            // Never roll back a final file, nor infer ownership after restart.
            // A crash/failure after rename but before commit may leave a duplicate
            // on replay; only disposable staging is automatically deleted.
            let _ = stage.cleanup();
            result
        })
        .await
    }
    /// Retain a failed folder save without minting a receipt. A later phone
    /// retry must still attempt the folder save.
    pub async fn commit_cache(
        &self,
        device: Id,
        verified: VerifiedFile,
        stage: Stage,
    ) -> Result<bool> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            #[cfg(test)]
            store.fault("commit_cache")?;
            let begin = verified.metadata();
            ensure!(
                stage.0.file.metadata()?.len() == begin.size,
                "cache staging size mismatch"
            );
            stage.0.file.sync_all()?;
            let mut owners = store.cache_owners.lock().expect("cache ownership");
            let key = (device.clone(), begin.transfer_id.clone());
            let result = store.peer_write(epoch, |tx| {
                if let Some((meta, _)) = receipt_row(tx, device.as_str(), begin.transfer_id.as_str())? {
                    ensure!(meta == Metadata::File(begin.clone()), ReceiptConflict);
                    return Ok(false);
                }
                validate_failed_file(tx, &device, &begin)?;
                if let Some((_, path)) = failed_file(tx, &device, &begin)?
                    && owners.get(&key).is_some_and(|owner|
                        owner.path == path && owner.file.matches_path(&path).unwrap_or(false)) {
                    return Ok(false);
                }
                capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                store.check_file_setting(tx,&device,begin.kind)?;
                stage.place()?;
                tx.execute("INSERT INTO failed_cache(device,id,meta,destination) VALUES (?,?,?,?) ON CONFLICT(device,id) DO UPDATE SET destination=excluded.destination",
                    params![device.as_str(),begin.transfer_id.as_str(),serde_json::to_string(&Metadata::File(begin.clone()))?,stage.destination_path()?.to_str()])?;
                Ok(true)
            });
            if matches!(result, Ok(true)) {
                // Publish evidence only after the placement AND journal commit
                // succeeded. Failure to record identity merely skips retirement.
                if let (Ok(path), Ok(file)) = (stage.destination_path(), stage.record_placed()) {
                    if owners.len() == CACHE_OWNER_LIMIT && !owners.contains_key(&key)
                        && let Some(old) = owners.keys().next().cloned() { owners.remove(&old); }
                    owners.insert(key, CacheOwner { path, file });
                }
            }
            let _ = stage.cleanup();
            result
        })
        .await
    }
    /// Reserve the transfer namespace before any clipboard effect. Completed
    /// or stale operations need no claim; pending operations must never be ACKed.
    pub(crate) async fn claim_text(
        &self,
        device: Id,
        text: Clipboard,
    ) -> Result<Option<ClipboardReservation>> {
        let epoch = self.peer_epoch(&device);
        let reservation = self.reserve().await;
        let store = self.clone();
        reservation
            .submit_with(move |reservation| {
                #[cfg(test)]
                store.fault("claim_text")?;
                store.retry_abandoned_claims()?;
                ensure!(
                    store
                        .abandoned_claims
                        .lock()
                        .expect("abandoned text claims")
                        .len()
                        < RECENT_TEXT_LIMIT,
                    "text cleanup capacity exhausted"
                );
                store.peer_write(epoch.clone(), |tx| {
                    let expected =
                        Metadata::Text(keyed_text_receipt(&store.clipboard_fingerprint_key, &text));
                    let cached: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM failed_cache WHERE device=? AND id=?)",
                        params![device.as_str(), text.transfer_id.as_str()],
                        |r| r.get(0),
                    )?;
                    ensure!(!cached, ReceiptConflict);
                    if let Some((previous, complete)) =
                        receipt_row(tx, device.as_str(), text.transfer_id.as_str())?
                    {
                        ensure!(previous == expected, ReceiptConflict);
                        ensure!(complete, "clipboard operation still pending");
                        return Ok(None);
                    }
                    capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                    let token = crate::random_hex(16);
                    tx.execute(
                        "INSERT INTO receipts(device,id,meta,destination) VALUES (?,?,?,?)",
                        params![
                            device.as_str(),
                            text.transfer_id.as_str(),
                            serde_json::to_string(&expected)?,
                            token
                        ],
                    )?;
                    Ok(Some(ClipboardReservation {
                        reservation: Some(reservation),
                        epoch,
                        store: store.clone(),
                        device,
                        text,
                        token: Some(token),
                    }))
                })
            })
            .await?
    }
    #[cfg(test)]
    pub async fn text_is_stale(&self, device: Id, text: Clipboard) -> Result<bool> {
        let epoch = self.peer_epoch(&device);
        let expected = Metadata::Text(keyed_text_receipt(&self.clipboard_fingerprint_key, &text));
        self.call(move |c| {
            epoch.check(c)?;
            if let Some((previous, complete)) =
                receipt_row(c, device.as_str(), text.transfer_id.as_str())?
            {
                ensure!(previous == expected, ReceiptConflict);
                ensure!(complete, "clipboard operation still pending");
                return Ok(true);
            }
            let timestamp: Option<i64> = c
                .query_row(
                    "SELECT created_at FROM legacy_clipboard_clock WHERE device=?",
                    [device.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(timestamp.is_some_and(|t| text.created_at <= t as u64))
        })
        .await
    }
    #[cfg(test)]
    async fn applied_text(&self, device: Id, text: Clipboard) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        // Receipt commits are uncancelable; the actor owns clipboard ordering.
        self.start_completion(move |store| {
            store.peer_write(epoch, |tx| {
                let expected =
                    Metadata::Text(keyed_text_receipt(&store.clipboard_fingerprint_key, &text));
                if let Some((previous, _)) =
                    receipt_row(tx, device.as_str(), text.transfer_id.as_str())?
                {
                    ensure!(previous == expected, ReceiptConflict);
                    return Ok(());
                }
                capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                record_text(
                    tx,
                    &device,
                    &text,
                    None,
                    true,
                    &store.clipboard_fingerprint_key,
                )?;
                Ok(())
            })
        })
        .await?
    }
    /// Clock/register fixture for Store migration and exhaustion tests.
    #[cfg(test)]
    pub(crate) async fn local_clipboard(&self, source: Id) -> Result<(u64, u64)> {
        self.call(move |c| {
            let tx = c.transaction()?;
            let (created_at, hlc) = allocate_stamp(&tx)?;
            apply_stamp(&tx, hlc, &source, created_at)?;
            tx.commit()?;
            Ok((created_at, hlc))
        })
        .await
    }
    pub(crate) async fn observe_hlc(&self, device: Id, hlc: u64) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            #[cfg(test)]
            store.fault("observe_hlc")?;
            store.peer_write(epoch, |tx| observe_hlc(tx, hlc))
        })
        .await
    }
    #[cfg(test)]
    pub(crate) async fn stamp_eligible(&self, hlc: u64, source: Id) -> Result<bool> {
        self.call(move |c| stamp_eligible(c, hlc, &source)).await
    }
    /// Local timestamp assignment and enqueue share one peer write transaction.
    #[cfg(test)]
    pub async fn enqueue_clipboard(&self, device: Id, text: String) -> Result<Clipboard> {
        self.prepare_clipboard(device, text).await
    }
    #[cfg(test)]
    async fn prepare_clipboard(&self, device: Id, text: String) -> Result<Clipboard> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.peer_write(epoch, |tx| {
                let previous: i64 = tx.query_row(
                    "SELECT last_outgoing_at FROM local_clock WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                let text = Clipboard {
                    hlc: None,
                    source_device: None,
                    transfer_id: Id::new(crate::random_hex(16))?,
                    text,
                    created_at: crate::now_ms().max((previous as u64).saturating_add(1)),
                    origin: ClipboardOrigin::Clipboard,
                };
                replace_outgoing(store, tx, &device, &text)?;
                Ok(text)
            })
        })
        .await
    }
    #[cfg(test)]
    pub async fn enqueue(&self, device: Id, text: Clipboard) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.peer_write(epoch, |tx| replace_outgoing(store, tx, &device, &text))
        })
        .await
    }
    #[cfg(test)]
    pub async fn first_outgoing(&self, device: Id) -> Result<Option<(i64, Clipboard, i64)>> {
        let epoch = self.peer_epoch(&device);
        self.call(move |c| {
            epoch.check(c)?;
            let row: Option<(i64, String, i64)> = c
                .query_row(
                    "SELECT seq,payload,retry_at FROM outgoing WHERE device=?",
                    [device.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            row.map(|(seq, p, t)| Ok((seq, serde_json::from_str(&p)?, t)))
                .transpose()
        })
        .await
    }
}
#[cfg(test)]
fn replace_outgoing(
    store: &Store,
    tx: &Transaction<'_>,
    device: &Id,
    text: &Clipboard,
) -> Result<()> {
    // Validate encoded size before modifying timestamps or the existing value.
    let mut wire = text.clone();
    if !store.mesh {
        wire.hlc = None;
        wire.source_device = None;
    }
    pneu_proto::encode_frame(&pneu_proto::Message::Clipboard(wire))?;
    let previous: Option<i64> = tx
        .query_row(
            "SELECT json_extract(payload,'$.created_at') FROM outgoing WHERE device=?",
            [device.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if previous.is_some_and(|timestamp| timestamp as u64 >= text.created_at) {
        return Ok(());
    }
    capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
    tx.execute("DELETE FROM outgoing WHERE device=?", [device.as_str()])?;
    tx.execute(
        "INSERT INTO outgoing(device,payload) VALUES (?,?)",
        params![device.as_str(), serde_json::to_string(text)?],
    )?;
    tx.execute(
        "UPDATE local_clock SET last_outgoing_at=max(last_outgoing_at,?) WHERE singleton=1",
        [text.created_at as i64],
    )?;
    Ok(())
}
fn capacity(folder: &Folder, reserve: u64) -> Result<()> {
    ensure!(
        folder.space()? >= reserve.saturating_add(256 * 1024),
        "receipt filesystem reserve reached"
    );
    Ok(())
}
fn failed_file(
    c: &Connection,
    device: &Id,
    begin: &FileBegin,
) -> Result<Option<(Metadata, PathBuf)>> {
    let row: Option<(String, String)> = c
        .query_row(
            "SELECT meta,destination FROM failed_cache WHERE device=? AND id=?",
            params![device.as_str(), begin.transfer_id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(meta, path)| Ok((serde_json::from_str(&meta)?, PathBuf::from(path))))
        .transpose()
}
fn validate_failed_file(c: &Connection, device: &Id, begin: &FileBegin) -> Result<()> {
    if let Some((meta, _)) = failed_file(c, device, begin)? {
        ensure!(meta == Metadata::File(begin.clone()), ReceiptConflict);
    }
    Ok(())
}
fn receipt_row(c: &Connection, device: &str, id: &str) -> Result<Option<(Metadata, bool)>> {
    let row: Option<(String, bool)> = c.query_row("SELECT meta,json_extract(meta,'$.operation')!='Text' FROM receipts WHERE device=? AND id=? UNION ALL SELECT meta,1 FROM clipboard_recent WHERE device=? AND id=? UNION ALL SELECT meta,1 FROM settings_receipts WHERE device=? AND id=? UNION ALL SELECT meta,1 FROM members_receipts WHERE device=? AND id=? AND generation=(SELECT generation FROM setup_state WHERE singleton=1)", params![device,id,device,id,device,id,device,id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    row.map(|(m, complete)| Ok((serde_json::from_str(&m)?, complete)))
        .transpose()
}
/// Preserve OS causes through anyhow contexts, including platform filesystem errors.
pub(crate) fn save_failure_code(error: &anyhow::Error) -> NackCode {
    for cause in error.chain() {
        if cause.downcast_ref::<settings::SettingDisabled>().is_some() {
            return NackCode::UnsupportedKind;
        }
        if cause.downcast_ref::<ReceiptConflict>().is_some() {
            return NackCode::IdConflict;
        }
        if let Some(code) = crate::platform::fs::storage_failure(cause) {
            return code;
        }
    }
    NackCode::StorageFailed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    fn test_connection(path: &Path) -> Result<Connection> {
        #[cfg(windows)]
        crate::platform::private::open(path)?;
        Ok(Connection::open(path)?)
    }
    #[test]
    #[cfg(windows)]
    fn sqlite_recreated_sidecar_crash_fixture() -> Result<()> {
        let Some(path) = std::env::var_os("PNEU_TEST_SIDECAR_DB") else {
            return Ok(());
        };
        let mode = std::env::var("PNEU_TEST_SIDECAR_MODE")?;
        let path = Path::new(&path);
        let store = Store::open(path)?;
        store.locked(|conn| {
            conn.execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE crash_probe(value); INSERT INTO crash_probe VALUES(1);")?;
            assert!(!path.with_extension("sqlite3-wal").exists());
            assert!(!path.with_extension("sqlite3-shm").exists());
            if mode == "wal" {
                conn.execute_batch("PRAGMA journal_mode=WAL; UPDATE crash_probe SET value=2;")?;
                for suffix in ["-wal", "-shm"] {
                    assert!(PathBuf::from(format!("{}{suffix}", path.display())).exists());
                }
            } else {
                conn.execute_batch("BEGIN IMMEDIATE; UPDATE crash_probe SET value=2;")?;
                assert!(PathBuf::from(format!("{}-journal", path.display())).exists());
            }
            Ok(())
        })?;
        // Simulated crash: no Connection/Store destructors or sidecar cleanup.
        std::process::exit(0);
    }
    #[test]
    #[cfg(windows)]
    fn sqlite_recreated_wal_shm_and_journal_survive_crash_restart() -> Result<()> {
        for mode in ["wal", "delete"] {
            let dir = crate::test_support::scratch()?;
            let path = dir.path().join("state.sqlite3");
            let output = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "store::tests::sqlite_recreated_sidecar_crash_fixture",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env("PNEU_TEST_SIDECAR_DB", &path)
                .env("PNEU_TEST_SIDECAR_MODE", mode)
                .output()?;
            ensure!(
                output.status.success(),
                "sidecar crash fixture failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let parent = Folder::open(dir.path(), false)?;
            let suffixes: &[&str] = if mode == "wal" {
                &["-wal", "-shm"]
            } else {
                &["-journal"]
            };
            for suffix in suffixes {
                let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
                assert!(sidecar.exists());
                // Prove this is the inherited/recreated path, not our protected
                // precreation. The contextual policy is the one that admits it.
                assert!(crate::platform::private::check_path(&sidecar).is_err());
                crate::platform::private::check_in(&parent.file, sidecar.file_name().unwrap())?;
            }
            let store = Store::open(&path)?;
            let value: i32 = store
                .locked(|c| Ok(c.query_row("SELECT value FROM crash_probe", [], |r| r.get(0))?))?;
            assert_eq!(value, if mode == "wal" { 2 } else { 1 });
            drop(store);
            drop(Store::open(&path)?);
        }
        Ok(())
    }
    #[tokio::test]
    #[cfg(windows)]
    async fn canonical_save_placement_rejects_cache_aliases() -> Result<()> {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let (dir, store, device) = fixture().await?;
        let cache = dir.path().join("ActualCacheLongName");
        Folder::open_private(&cache)?;
        let mut aliases = vec![cache.clone(), dir.path().join("ACTUALCACHELONGNAME")];
        let mapping = crate::platform::fs::TestDrive::new(&cache)?;
        aliases.push(mapping.path());
        let input: Vec<_> = cache.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut short = vec![0u16; 32768];
        // SAFETY: terminated input and sized output buffers.
        let len = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(
                input.as_ptr(),
                short.as_mut_ptr(),
                short.len() as u32,
            )
        } as usize;
        ensure!(
            len > 0 && len < short.len(),
            "cannot query short cache path"
        );
        let short = PathBuf::from(std::ffi::OsString::from_wide(&short[..len]));
        if short != cache {
            aliases.push(short);
        }
        for alias in aliases {
            let staged = store
                .create_stage(cache.clone(), ".pneu-cache-peer-file".into(), 4)
                .await?;
            (&staged.0.file).write_all(b"keep")?;
            let mut meta = begin(b"keep");
            meta.file_name = ".pneu-cache-peer-file".into();
            let staged_path = staged.path()?;
            let error = store
                .commit_saved_file(device.clone(), VerifiedFile::fixture(meta), staged, alias)
                .await
                .unwrap_err();
            assert_eq!(
                save_failure_code(&error),
                NackCode::NotWritable,
                "{error:#}"
            );
            store.drain().await;
            assert!(!staged_path.exists());
            assert!(!cache.join(".pneu-cache-peer-file").exists());
        }
        // A subst root below cache must be rejected using retained ancestry.
        let child = cache.join("child");
        Folder::open_private(&child)?;
        let child_mapping = crate::platform::fs::TestDrive::new(&child)?;
        let staged = stage(&store, child_mapping.path(), b"keep").await?;
        let error = store
            .commit_saved_file(
                device.clone(),
                VerifiedFile::fixture(begin(b"keep")),
                staged,
                cache.clone(),
            )
            .await
            .unwrap_err();
        assert_eq!(save_failure_code(&error), NackCode::NotWritable);
        store.drain().await;
        // The same path is allowed as an ordinary save outside the cache.
        let saved = dir.path().join("saved");
        let staged = stage(&store, saved.clone(), b"keep").await?;
        assert!(
            store
                .commit_saved_file(device, VerifiedFile::fixture(begin(b"keep")), staged, cache)
                .await?
        );
        Ok(())
    }
    #[tokio::test]
    #[cfg(windows)]
    async fn provisionally_admitted_save_rejects_later_cache_placement() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let cache = dir.path().join("cache");
        Folder::open_private(&cache)?;
        let late = cache.join("late-drive-target");
        // A task-owned DOS drive exists, but its target is unavailable until
        // after config admission. No global drive mapping is changed.
        let mut mapping = crate::platform::fs::TestDrive::new(&late)?;
        let config = crate::Config {
            save_path: mapping.path(),
            save_to_folder: true,
            ..crate::Config::default()
        };
        config.validate_with_cache(&cache)?;
        assert!(Folder::open(&config.save_path, false).is_err());
        Folder::open_private(&late)?;
        let error = config.validate_with_cache(&cache).unwrap_err();
        assert!(error.to_string().contains("save folder must not be"));
        // Keep a completed fallback entry for this transfer: a rejected save
        // must neither publish a receipt nor retire/prune that cached copy.
        let meta = begin(b"keep");
        let cached = store
            .create_stage(cache.clone(), ".pneu-cache-recoverable".into(), 4)
            .await?;
        (&cached.0.file).write_all(b"keep")?;
        store
            .commit_cache(device.clone(), VerifiedFile::fixture(meta.clone()), cached)
            .await?;
        let staged = store
            .create_stage(config.save_path.clone(), "saved.bin".into(), 4)
            .await?;
        (&staged.0.file).write_all(b"keep")?;
        let staged_path = staged.path()?;
        // Retarget the configured drive outside cache after staging. Only the
        // retained Folder reveals that the actual placement is still in cache.
        let safe = dir.path().join("safe");
        Folder::open_private(&safe)?;
        mapping.repoint(&safe)?;
        config.validate_with_cache(&cache)?;
        let error = store
            .commit_saved_file(
                device.clone(),
                VerifiedFile::fixture(meta.clone()),
                staged,
                cache.clone(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            save_failure_code(&error),
            NackCode::NotWritable,
            "{error:#}"
        );
        store.drain().await;
        assert!(!late.join("saved.bin").exists());
        assert!(!safe.join("saved.bin").exists());
        assert!(!staged_path.exists());
        assert_eq!(
            std::fs::read(cache.join(".pneu-cache-recoverable"))?,
            b"keep"
        );
        assert!(
            store
                .locked(|c| receipt_row(c, device.as_str(), meta.transfer_id.as_str()))?
                .is_none()
        );
        assert!(store.locked(|c| failed_file(c, &device, &meta))?.is_some());
        Ok(())
    }
    #[tokio::test]
    #[cfg(windows)]
    async fn saved_and_cache_commits_handoff_readonly_received_handles() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        for fallback_cache in [false, true] {
            let (dir, store, device) = fixture().await?;
            let destination = dir
                .path()
                .join(if fallback_cache { "cache" } else { "save" });
            Folder::open_private(&destination)?;
            let staged = stage(&store, destination, b"received").await?;
            // Model the writer clone held by the stream until completion.
            let stream_writer = staged.0.file.try_clone()?;
            let verified = VerifiedFile::fixture(begin(b"received"));
            let committed = if fallback_cache {
                store.commit_cache(device, verified, staged.clone()).await?
            } else {
                store
                    .commit_saved_file(
                        device,
                        verified,
                        staged.clone(),
                        dir.path().join("unused-cache"),
                    )
                    .await?
            };
            assert!(committed);
            let received = crate::actions::ReceivedFile::from_file(
                staged.destination_path()?,
                staged.0.file.try_clone()?,
            )?;
            drop(stream_writer);
            drop(staged);
            store.drain().await;
            let reader = fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(received.path())?;
            assert_eq!(reader.metadata()?.len(), 8);
            assert_eq!(received.read_bounded(8)?, b"received");
        }
        Ok(())
    }
    fn begin(bytes: &[u8]) -> FileBegin {
        FileBegin {
            hlc: None,
            source_device: None,
            transfer_id: Id::new(crate::random_hex(16)).unwrap(),
            file_name: "file.bin".into(),
            mime: "application/octet-stream".into(),
            size: bytes.len() as u64,
            sha256: Hash::new(hex::encode(Sha256::digest(bytes))).unwrap(),
            kind: pneu_proto::FileKind::File,
            created_at: 0,
            source_media_id: None,
        }
    }
    // Join completion can precede runtime teardown of the job's lock-owning
    // environment. Restart fixtures wait for every owner, not just the result.
    async fn close_for_restart(store: Store) -> Result<()> {
        store.drain().await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while Arc::strong_count(&store._ownership) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("store jobs retained restart ownership")?;
        drop(store);
        Ok(())
    }
    // The verbatim released startup can fail before wrapping its raw flock in
    // StoreOwnership. A concurrent test's fork may then inherit that lock until
    // exec. Run this historical code alone so process exit releases every FD.
    #[cfg(unix)]
    async fn released_startup(path: &Path, expected: &str) -> Result<()> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "store::tests::released_startup_fixture",
                    "--nocapture",
                ])
                .env("PNEU_TEST_RELEASED_STORE", path)
                .env("PNEU_TEST_RELEASED_EXPECTED", expected)
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        ensure!(
            output.status.success(),
            "released startup fixture failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    #[test]
    #[cfg(unix)]
    fn released_startup_fixture() -> Result<()> {
        let Some(path) = std::env::var_os("PNEU_TEST_RELEASED_STORE") else {
            return Ok(());
        };
        let expected = std::env::var("PNEU_TEST_RELEASED_EXPECTED")?;
        let result = released_store::Store::open(Path::new(&path));
        if expected == "success" {
            drop(result?);
        } else {
            let error = result
                .err()
                .expect("released startup must refuse migrated storage");
            assert!(error.to_string().contains(&expected), "{error:#}");
        }
        Ok(())
    }

    async fn fixture() -> Result<(tempfile::TempDir, Store, Id)> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let device = Id::new(crate::random_hex(16))?;
        store
            .pair(device.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        Ok((dir, store, device))
    }

    fn text(created_at: u64) -> Clipboard {
        Clipboard {
            hlc: None,
            source_device: None,
            transfer_id: Id::new(crate::random_hex(16)).unwrap(),
            text: "clipboard secret".into(),
            created_at,
            origin: ClipboardOrigin::Clipboard,
        }
    }
    async fn stage(store: &Store, path: PathBuf, bytes: &[u8]) -> Result<Stage> {
        let stage = store
            .create_stage(
                path,
                format!("{}.bin", crate::random_hex(16)),
                bytes.len() as u64,
            )
            .await?;
        (&stage.0.file).write_all(bytes)?;
        Ok(stage)
    }
    #[tokio::test]
    async fn legacy_pair_migrates_and_multiple_peers_survive_restart() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let first = Id::new(crate::random_hex(16))?;
        let fp = Hash::new(crate::random_hex(32))?;
        let pending = text(123);
        let file = begin(b"completed");
        {
            let c = test_connection(&path)?;
            c.execute_batch("CREATE TABLE peers(id TEXT PRIMARY KEY,fp TEXT UNIQUE NOT NULL);
                CREATE TABLE pairing_epoch(singleton INTEGER PRIMARY KEY,generation INTEGER NOT NULL,last_outgoing_at INTEGER NOT NULL);
                INSERT INTO pairing_epoch VALUES(1,7,123);
                CREATE TABLE clipboard_clock(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,created_at INTEGER NOT NULL);
                CREATE TABLE receipts(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,id TEXT NOT NULL,meta TEXT NOT NULL,destination TEXT,PRIMARY KEY(device,id));
                CREATE TABLE outgoing(seq INTEGER PRIMARY KEY AUTOINCREMENT,device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,payload TEXT NOT NULL,retry_at INTEGER NOT NULL DEFAULT 0);")?;
            c.execute(
                "INSERT INTO peers VALUES(?,?)",
                params![first.as_str(), fp.as_str()],
            )?;
            c.execute(
                "INSERT INTO clipboard_clock VALUES(?,?)",
                params![first.as_str(), 99],
            )?;
            c.execute(
                "INSERT INTO receipts VALUES(?,?,?,?)",
                params![
                    first.as_str(),
                    file.transfer_id.as_str(),
                    serde_json::to_string(&Metadata::File(file.clone()))?,
                    "saved.png"
                ],
            )?;
            c.execute(
                "INSERT INTO outgoing(device,payload) VALUES(?,?)",
                params![first.as_str(), serde_json::to_string(&pending)?],
            )?;
        }
        let store = Store::open(&path)?;
        assert_eq!(store.peers().await?.get(&fp), Some(&first));
        assert!(store.first_outgoing(first.clone()).await?.is_none());
        assert_eq!(
            store
                .receipt(first.clone(), file.transfer_id.clone())
                .await?
                .unwrap()
                .0,
            Metadata::File(file)
        );
        assert!(store.text_is_stale(first.clone(), text(98)).await?);
        let second = Id::new(crate::random_hex(16))?;
        store
            .pair(second.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        assert_eq!(store.peers().await?.len(), 2);
        close_for_restart(store).await?;
        let store = Store::open(&path)?;
        let peers = store.peers().await?;
        assert_eq!(peers.len(), 2);
        assert_eq!(peers.get(&fp), Some(&first));
        assert!(peers.values().any(|id| id == &second));
        assert!(store.first_outgoing(first).await?.is_none());
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn sweep_released_startup_cannot_delete_migrated_data() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        // Create the original schema with the real released startup, then
        // migrate an existing pairing before adding the second device.
        released_startup(&path, "success").await?;
        let first = Id::from_bytes([1; 16]);
        let c = test_connection(&path)?;
        c.execute(
            "INSERT INTO peers VALUES(?,?)",
            params![first.as_str(), crate::random_hex(32)],
        )?;
        drop(c);
        let store = Store::open(&path)?;
        let second = Id::from_bytes([2; 16]);
        store
            .pair(second.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        for device in [&first, &second] {
            store.applied_text(device.clone(), text(123)).await?;
            store.enqueue(device.clone(), text(456)).await?;
            let file = begin(b"completed");
            store.locked(|c| {
                c.execute(
                    "INSERT INTO receipts VALUES(?,?,?,?)",
                    params![
                        device.as_str(),
                        file.transfer_id.as_str(),
                        serde_json::to_string(&Metadata::File(file.clone()))?,
                        "saved.png"
                    ],
                )?;
                c.execute(
                    "INSERT INTO failed_cache VALUES(?,?,?,?)",
                    params![
                        device.as_str(),
                        file.transfer_id.as_str(),
                        serde_json::to_string(&Metadata::File(file.clone()))?,
                        "cached.png"
                    ],
                )?;
                c.execute(
                    "INSERT INTO peer_settings VALUES(?,?,?,?,?)",
                    params![
                        device.as_str(),
                        "send_clipboard",
                        true,
                        "123",
                        device.as_str()
                    ],
                )?;
                Ok(())
            })?;
        }
        close_for_restart(store).await?;
        let snapshot = || -> Result<Vec<String>> {
            let c = test_connection(&path)?;
            let mut rows = Vec::new();
            for table in [
                "peers",
                "peer_generations",
                "receipts",
                "outgoing",
                "clipboard_recent",
                "legacy_clipboard_clock",
                "clipboard_clock",
                "local_clock",
                "peer_settings",
                "failed_cache",
                "staging_folders",
            ] {
                let mut q = c.prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
                let cols = q.column_count();
                for row in q.query_map([], |r| {
                    Ok(format!(
                        "{table}: {:?}",
                        (0..cols)
                            .map(|i| r.get_ref(i).unwrap().to_owned())
                            .collect::<Vec<_>>()
                    ))
                })? {
                    rows.push(row?);
                }
            }
            Ok(rows)
        };
        let before = snapshot()?;
        released_startup(&path, "view").await?;
        assert_eq!(
            snapshot()?,
            before,
            "released startup deleted migrated data"
        );
        let store = Store::open(&path)?;
        assert_eq!(store.peers().await?.len(), 2);
        close_for_restart(store).await?;
        assert_eq!(
            snapshot()?,
            before
                .into_iter()
                .filter(|row| !row.starts_with("outgoing:"))
                .collect::<Vec<_>>(),
            "new startup after old attempt lost data"
        );
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn sweep_unfenced_v1_old_startup_artifacts_recover_without_further_loss() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let file = begin(b"kept");
        let pending = text(123);
        store.enqueue(device.clone(), pending.clone()).await?;
        let before_epoch = store.peer_epoch(&device).generation;
        store.locked(|c| {
            c.execute("INSERT INTO receipts VALUES(?,?,?,?)",params![device.as_str(),file.transfer_id.as_str(),serde_json::to_string(&Metadata::File(file.clone()))?,"saved.png"])?;
            // Model startup of released code against pre-fence v1: it recreated
            // pairing_epoch; a subsequent old re-pair can reset peers.generation.
            c.execute_batch("DROP VIEW pairing_epoch; CREATE TABLE pairing_epoch(singleton INTEGER PRIMARY KEY,generation INTEGER NOT NULL,last_outgoing_at INTEGER NOT NULL); INSERT INTO pairing_epoch VALUES(1,0,9876); UPDATE peers SET generation=0;")?;
            Ok(())
        })?;
        let path = dir.path().join("state.sqlite3");
        close_for_restart(store).await?;
        let store = Store::open(&path)?;
        assert_eq!(store.peer_epoch(&device).generation, before_epoch);
        assert!(
            store
                .receipt(device.clone(), file.transfer_id.clone())
                .await?
                .is_some()
        );
        assert!(store.first_outgoing(device).await?.is_none());
        store.locked(|c| {
            assert_eq!(
                c.query_row("SELECT last_outgoing_at FROM local_clock", [], |r| r
                    .get::<_, i64>(0))?,
                9876
            );
            assert_eq!(
                c.query_row(
                    "SELECT type FROM sqlite_schema WHERE name='pairing_epoch'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "view"
            );
            Ok(())
        })?;
        close_for_restart(store).await?;
        released_startup(&path, "view").await?;
        let store = Store::open(&path)?;
        assert_eq!(store.peers().await?.len(), 1);
        Ok(())
    }
    #[tokio::test]
    async fn established_evidence_migrates_conservatively_and_is_generation_scoped() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let path = dir.path().join("state.sqlite3");
        let generation = store.pairing_generation(&device);
        store
            .call(|c| {
                c.execute_batch(
                    "ALTER TABLE peers DROP COLUMN established; PRAGMA user_version=1;",
                )?;
                Ok(())
            })
            .await?;
        close_for_restart(store).await?;
        let store = Store::open(&path)?;
        assert_eq!(store.pairing_generation(&device), generation);
        assert!(!store.peer_established(device.clone()).await?);
        store.mark_peer_established(device.clone()).await?;
        close_for_restart(store).await?;
        let store = Store::open(&path)?;
        assert!(store.peer_established(device.clone()).await?);
        let old = store.with_current_pairing();
        store
            .pair(device.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        assert!(!store.peer_established(device.clone()).await?);
        assert!(old.mark_peer_established(device.clone()).await.is_err());
        assert!(!store.peer_established(device.clone()).await?);
        drop(old);
        // Startup generation repair must not transfer READY evidence either.
        store.mark_peer_established(device.clone()).await?;
        store
            .call(|c| {
                c.execute("UPDATE peer_generations SET generation=generation+1", [])?;
                Ok(())
            })
            .await?;
        close_for_restart(store).await?;
        let store = Store::open(&path)?;
        assert!(!store.peer_established(device).await?);
        close_for_restart(store).await
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn v1_migration_adds_only_established_and_preserves_downgrade_fence() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let path = dir.path().join("state.sqlite3");
        let pending = text(456);
        store.enqueue(device.clone(), pending.clone()).await?;
        let generation = store.pairing_generation(&device);
        let peers = store.peers().await?;
        // Model the committed v1 schema with an existing binding.
        store
            .call(|c| {
                c.execute_batch(
                    "ALTER TABLE peers DROP COLUMN established; PRAGMA user_version=1;",
                )?;
                Ok(())
            })
            .await?;
        close_for_restart(store).await?;
        let store = Store::open(&path)?;
        store
            .call(|c| {
                assert_eq!(
                    c.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))?,
                    4
                );
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM sqlite_schema WHERE name='join_installations'",
                        [],
                        |r| r.get::<_, u32>(0)
                    )?,
                    0
                );
                assert_eq!(
                    c.query_row(
                        "SELECT type FROM sqlite_schema WHERE name='pairing_epoch'",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "view"
                );
                Ok(())
            })
            .await?;
        assert_eq!(store.peers().await?, peers);
        assert_eq!(store.pairing_generation(&device), generation);
        assert!(store.first_outgoing(device.clone()).await?.is_none());
        close_for_restart(store).await?;
        released_startup(&path, "view").await?;
        let store = Store::open(&path)?;
        assert_eq!(store.peers().await?, peers);
        assert!(store.first_outgoing(device).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn failed_migration_rolls_back_schema_and_preserves_pairing() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let c = test_connection(&path)?;
        c.execute_batch("CREATE TABLE peers(id TEXT PRIMARY KEY,fp TEXT UNIQUE NOT NULL);
            CREATE TABLE pairing_epoch(singleton INTEGER PRIMARY KEY,generation INTEGER NOT NULL,last_outgoing_at INTEGER NOT NULL);
            INSERT INTO pairing_epoch VALUES(1,7,123);
            CREATE TABLE peer_settings(injected_conflict INTEGER);")?;
        let device = crate::random_hex(16);
        let fp = crate::random_hex(32);
        c.execute("INSERT INTO peers VALUES(?,?)", params![device, fp])?;
        drop(c);
        assert!(Store::open(&path).is_err());
        let c = test_connection(&path)?;
        let pair: (String, String) = c.query_row("SELECT id,fp FROM peers", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        assert_eq!(pair, (device, fp));
        assert_eq!(
            c.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))?,
            0
        );
        assert_eq!(
            c.query_row("SELECT count(*) FROM pragma_table_info('peers')", [], |r| r
                .get::<_, u32>(0))?,
            2
        );
        assert_eq!(
            c.query_row("SELECT generation FROM pairing_epoch", [], |r| r
                .get::<_, u32>(0))?,
            7
        );
        Ok(())
    }
    #[tokio::test]
    async fn device_clipboard_register_survives_unpair_and_legacy_gates_stay_independent()
    -> Result<()> {
        let (dir, store, first) = fixture().await?;
        store.applied_text(first.clone(), text(100)).await?;
        let second = Id::new(crate::random_hex(16))?;
        store
            .pair(second.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        assert!(!store.text_is_stale(second.clone(), text(1)).await?);
        store.applied_text(second.clone(), text(1)).await?;
        store.unpair_then(second.clone(), |_| {}).await?;
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let source: String = store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT source_device FROM clipboard_clock WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await?;
        assert_eq!(source, second.as_str());
        assert!(store.text_is_stale(first, text(99)).await?);
        Ok(())
    }
    #[tokio::test]
    async fn another_pair_does_not_revoke_captured_peer_authority() -> Result<()> {
        let (_dir, store, first) = fixture().await?;
        let admitted = store.with_current_pairing();
        let first_generation = store.peer_epoch(&first).generation;
        let second = Id::new(crate::random_hex(16))?;
        store
            .pair(second.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        admitted.enqueue(first.clone(), text(1)).await?;
        let second_generation = store.peer_epoch(&second).generation;
        store
            .pair(second.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        admitted.enqueue(first.clone(), text(2)).await?;
        assert_eq!(store.peer_epoch(&first).generation, first_generation);
        assert_eq!(store.peer_epoch(&second).generation, second_generation + 1);
        store.unpair_then(second, |_| {}).await?;
        let latest = text(3);
        admitted.enqueue(first.clone(), latest.clone()).await?;
        assert_eq!(store.first_outgoing(first).await?.unwrap().1, latest);
        Ok(())
    }
    #[tokio::test]
    async fn pairing_changes_durably_clear_peer_presentation_and_addresses() -> Result<()> {
        for unpair in [false, true] {
            let (dir, store, device) = fixture().await?;
            let captured = store.with_current_pairing();
            let current = Arc::new(AtomicU64::new(1));
            let hello = pneu_proto::Hello {
                setup: None,
                version: 1,
                device_id: device.clone(),
                name: "Revoked computer".into(),
                role: Some(pneu_proto::DeviceRole::Computer),
                addresses: Some(vec!["192.0.2.1:47821".into()]),
                capabilities: vec!["clipboard_text".into(), "mesh".into()],
                paused: Some(false),
            };
            captured
                .remember_peer_hello(hello.clone(), current.clone(), 1, false)
                .await?;
            let old: (String, String, String) = store
                .call(|c| {
                    Ok(
                        c.query_row("SELECT name,role,addresses FROM peers", [], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                        })?,
                    )
                })
                .await?;
            assert_eq!(
                old,
                (
                    hello.name.clone(),
                    "computer".into(),
                    serde_json::to_string(hello.addresses.as_ref().unwrap())?
                )
            );
            if unpair {
                store.unpair_then(device.clone(), |_| {}).await?;
            } else {
                store
                    .pair(device.clone(), Hash::new(crate::random_hex(32))?)
                    .await?;
            }
            // Revoked HELLO work cannot restore hints into the replacement row.
            assert!(
                captured
                    .remember_peer_hello(hello, current, 1, false)
                    .await
                    .is_err()
            );
            drop(captured);
            close_for_restart(store).await?;
            let restarted = Store::open(&dir.path().join("state.sqlite3"))?;
            let metadata: Vec<(String, String, String)> = restarted
                .call(|c| {
                    let mut q = c.prepare("SELECT name,role,addresses FROM peers")?;
                    Ok(q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                        .collect::<rusqlite::Result<_>>()?)
                })
                .await?;
            assert_eq!(
                metadata,
                if unpair {
                    vec![]
                } else {
                    vec![("".into(), "phone".into(), "[]".into())]
                }
            );
            close_for_restart(restarted).await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn delayed_peer_writes_cannot_cross_pairing_generations() -> Result<()> {
        // Both same-ID replacement and unpair/re-pair must revoke every write
        // class, including no-op receipts and jobs that outlive their caller.
        for unpair_first in [false, true] {
            for operation in [0, 1, 2, 3, 8, 9, 10] {
                let (dir, store, device) = fixture().await?;
                let old_scope = store.with_current_pairing();
                let pending = text(1);
                store.enqueue(device.clone(), pending).await?;
                let staged = stage(&store, dir.path().join("save"), b"old file").await?;
                let placed = staged.destination_path()?;
                let temporary = staged.path()?;
                let meta = begin(b"old file");
                let id = meta.transfer_id.clone();
                let claim = if operation == 9 {
                    Some(store.claim_text(device.clone(), text(99)).await?.unwrap())
                } else {
                    None
                };
                let (entered, release) = store.delay_next_peer_write();
                // Use the unscoped store here: generation must be captured
                // before spawn_blocking, rather than inside its delayed closure.
                let writer = store.clone();
                let peer = device.clone();
                let job = tokio::spawn(async move {
                    match operation {
                        0 => writer
                            .commit_file(peer, VerifiedFile::fixture(meta), staged)
                            .await
                            .map(|_| ()),
                        1 => writer.applied_text(peer, text(99)).await,
                        2 => writer.enqueue(peer, text(99)).await,
                        3 => writer
                            .enqueue_clipboard(peer, "revoked".into())
                            .await
                            .map(|_| ()),
                        8 => writer.claim_text(peer, text(99)).await.map(|_| ()),
                        10 => writer.mark_peer_established(peer).await,
                        _ => claim.unwrap().finalize().await,
                    }
                });
                entered.await?;
                if unpair_first {
                    store.unpair_then(device.clone(), |_| {}).await?;
                }
                store
                    .pair(device.clone(), Hash::new(crate::random_hex(32))?)
                    .await?;
                let newest = text(2);
                store.enqueue(device.clone(), newest.clone()).await?;
                let applied = text(3);
                store.applied_text(device.clone(), applied.clone()).await?;
                release.send(())?;
                let result = job.await?;
                store.drain().await;
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("pairing generation revoked")
                );
                assert!(!placed.exists());
                assert!(!temporary.exists());
                assert!(store.receipt(device.clone(), id).await?.is_none());
                assert_eq!(
                    store.first_outgoing(device.clone()).await?.unwrap().1,
                    newest
                );
                assert!(store.text_is_stale(device.clone(), applied.clone()).await?);
                assert!(!store.text_is_stale(device.clone(), text(4)).await?);
                // A session retained after replacement cannot acquire fresh
                // authority merely by starting its next operation later.
                assert!(
                    old_scope
                        .receipt(device.clone(), applied.transfer_id)
                        .await
                        .is_err()
                );
                assert!(
                    old_scope
                        .text_is_stale(device.clone(), text(4))
                        .await
                        .is_err()
                );
                assert!(old_scope.first_outgoing(device.clone()).await.is_err());
            }
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[cfg(unix)]
    async fn restart_releases_lock_even_while_concurrent_child_is_before_exec() -> Result<()> {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (dir, store, _) = fixture().await?;
        let lock_fd = store._ownership.0.as_raw_fd();
        let (parent, child_control) = std::os::unix::net::UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let mut parent = tokio::net::UnixStream::from_std(parent)?;
        let child = tokio::task::spawn_blocking(move || -> Result<()> {
            let control = child_control.as_raw_fd();
            let mut command = std::process::Command::new("/bin/true");
            // Only async-signal-safe syscalls in the child; this forces the fork
            // window that concurrent D-Bus test process launches can otherwise hit.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(lock_fd, libc::F_GETFD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut byte = 1u8;
                    if libc::write(control, (&byte as *const u8).cast(), 1) != 1
                        || libc::read(control, (&mut byte as *mut u8).cast(), 1) != 1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let status = command.spawn()?.wait()?;
            anyhow::ensure!(status.success(), "test child failed");
            Ok(())
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), parent.read_u8()).await??;
        close_for_restart(store).await?;
        let reopened = Store::open(&dir.path().join("state.sqlite3"));
        parent.write_u8(1).await?;
        child.await??;
        reopened?.drain().await;
        Ok(())
    }
    #[tokio::test]
    async fn pairing_generation_and_outgoing_clock_survive_restart() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let generation = store.peer_epoch(&device).generation;
        let future = crate::now_ms() + 100_000;
        store.enqueue(device.clone(), text(future)).await?;
        store.unpair_then(device.clone(), |_| {}).await?;
        store
            .pair(device.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        store.drain().await;
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        assert_eq!(store.peer_epoch(&device).generation, generation + 2);
        let clipboard = store
            .enqueue_clipboard(device.clone(), "new".into())
            .await?;
        assert!(clipboard.created_at > future);
        assert_eq!(store.first_outgoing(device).await?.unwrap().1, clipboard);
        Ok(())
    }
    #[tokio::test]
    async fn legacy_pending_text_seeds_serialized_clock_before_first_local_enqueue() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("state.sqlite3");
        let device = Id::new(crate::random_hex(16))?;
        let future = crate::now_ms() + 100_000;
        {
            let c = test_connection(&path)?;
            c.execute_batch("CREATE TABLE peers(id TEXT PRIMARY KEY,fp TEXT UNIQUE NOT NULL);
                CREATE TABLE outgoing(seq INTEGER PRIMARY KEY AUTOINCREMENT,device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,payload TEXT NOT NULL,retry_at INTEGER NOT NULL DEFAULT 0);")?;
            c.execute(
                "INSERT INTO peers VALUES(?,?)",
                params![device.as_str(), crate::random_hex(32)],
            )?;
            c.execute(
                "INSERT INTO outgoing(device,payload) VALUES(?,?)",
                params![device.as_str(), serde_json::to_string(&text(future))?],
            )?;
        }
        let store = Store::open(&path)?;
        let newest = store
            .enqueue_clipboard(device.clone(), "new local text".into())
            .await?;
        assert!(newest.created_at > future);
        assert_eq!(store.first_outgoing(device).await?.unwrap().1, newest);
        Ok(())
    }
    #[tokio::test]
    async fn reverse_enqueue_order_preserves_latest_and_invalid_text_changes_nothing() -> Result<()>
    {
        let (_dir, store, device) = fixture().await?;
        let (entered, release) = store.delay_next_peer_write();
        let writer = store.clone();
        let peer = device.clone();
        let old = tokio::spawn(async move { writer.enqueue(peer, text(1)).await });
        entered.await?;
        let latest = text(2);
        let result = store.enqueue(device.clone(), latest.clone()).await;
        release.send(())?;
        old.await??;
        result?;
        store.enqueue(device.clone(), text(2)).await?;
        let before = store.first_outgoing(device.clone()).await?.unwrap();
        assert_eq!(before.1, latest);
        let failed = store
            .enqueue_clipboard(device.clone(), "x".repeat(20_000))
            .await;
        assert!(failed.is_err());
        assert_eq!(store.first_outgoing(device).await?.unwrap(), before);
        Ok(())
    }
    #[tokio::test]
    async fn wal_full_and_pairing_preserves_other_peer_state() -> Result<()> {
        let (_dir, store, device) = fixture().await?;
        store.applied_text(device.clone(), text(10)).await?;
        store.enqueue(device.clone(), text(11)).await?;
        let newer = Id::new(crate::random_hex(16))?;
        store
            .pair(newer.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        let (mode, synchronous, old): (String, i64, i64) = store.call(|c| Ok((
            c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
            c.query_row("PRAGMA synchronous", [], |r| r.get(0))?,
            c.query_row("SELECT (SELECT count(*) FROM clipboard_recent)+(SELECT count(*) FROM clipboard_clock)+(SELECT count(*) FROM outgoing)", [], |r| r.get(0))?,
        ))).await?;
        assert_eq!((mode.as_str(), synchronous, old), ("wal", 2, 3));
        let peers = store.peers().await?;
        assert_eq!(peers.len(), 2);
        assert!(peers.values().any(|id| id == &device));
        assert!(peers.values().any(|id| id == &newer));
        Ok(())
    }
    #[tokio::test]
    async fn bounded_lww_history_survives_restart_and_keeps_no_plaintext() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let first = text(1);
        assert!(!store.text_is_stale(device.clone(), first.clone()).await?);
        store.applied_text(device.clone(), first.clone()).await?;
        let mut conflict = first.clone();
        conflict.text = "different".into();
        assert_eq!(
            save_failure_code(
                &store
                    .text_is_stale(device.clone(), conflict)
                    .await
                    .unwrap_err()
            ),
            NackCode::IdConflict
        );
        for timestamp in 2..=100 {
            store.applied_text(device.clone(), text(timestamp)).await?;
        }
        assert!(store.text_is_stale(device.clone(), first.clone()).await?);
        assert!(store.text_is_stale(device.clone(), text(100)).await?);
        let (count, meta): (i64, String) = store
            .call(|c| {
                Ok((
                    c.query_row("SELECT count(*) FROM clipboard_recent", [], |r| r.get(0))?,
                    c.query_row("SELECT meta FROM clipboard_recent LIMIT 1", [], |r| {
                        r.get(0)
                    })?,
                ))
            })
            .await?;
        assert_eq!(count, RECENT_TEXT_LIMIT as i64);
        assert!(!meta.contains("clipboard secret"));
        store.drain().await;
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        assert!(store.text_is_stale(device.clone(), first).await?);
        assert!(!store.text_is_stale(device, text(101)).await?);
        Ok(())
    }

    #[tokio::test]
    async fn unpolled_finalization_still_records_applied_text() -> Result<()> {
        let (_dir, store, device) = fixture().await?;
        let applied = text(1);
        let claim = store
            .claim_text(device.clone(), applied.clone())
            .await?
            .context("claim")?;
        drop(claim.finalize());
        store.drain().await;
        assert!(store.receipt(device, applied.transfer_id).await?.unwrap().1);
        Ok(())
    }
    #[tokio::test]
    async fn all_claims_can_discard_without_waiting_on_their_own_capacity() -> Result<()> {
        let (_dir, store, device) = fixture().await?;
        let mut claims = Vec::new();
        for timestamp in 1..=crate::work::BLOCKING_LIMIT {
            claims.push(
                store
                    .claim_text(device.clone(), text(timestamp as u64))
                    .await?
                    .context("claim")?,
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            for claim in claims {
                claim.discard().await?;
            }
            store.drain().await;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        for timestamp in 1..=crate::work::BLOCKING_LIMIT {
            assert!(
                store
                    .receipt(device.clone(), text(timestamp as u64).transfer_id)
                    .await?
                    .is_none()
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn startup_sweeps_every_used_folder_even_without_receipts_or_current_config() -> Result<()>
    {
        let (dir, store, _) = fixture().await?;
        let a = dir.path().join("old-save");
        let b = dir.path().join("new-save");
        let first = stage(&store, a.clone(), b"partial").await?;
        let second = stage(&store, b.clone(), b"partial").await?;
        assert_ne!(
            first.0.directory.identity()?,
            second.0.directory.identity()?
        );
        drop(first);
        drop(second);
        store.drain().await;
        let external = dir.path().join("external");
        fs::create_dir(&external)?;
        fs::write(external.join("keep"), b"foreign")?;
        for path in [&a, &b] {
            fs::write(
                path.join(".pneu-staging/crash-before-stage-return"),
                b"orphan",
            )?;
            symlink(&external, path.join(".pneu-staging/link"))?;
        }
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store
            .clean_staging(vec![dir.path().join("unrelated-current-config")])
            .await?;
        for path in [a, b] {
            assert_eq!(fs::read_dir(path.join(".pneu-staging"))?.count(), 0);
        }
        assert_eq!(fs::read(external.join("keep"))?, b"foreign");
        Ok(())
    }
    #[tokio::test]
    async fn completed_receipts_never_inspect_or_delete_replaced_user_files() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let staged = stage(&store, dir.path().join("save"), b"owned").await?;
        let placed = staged.destination_path()?;
        let meta = begin(b"owned");
        store
            .commit_file(device.clone(), VerifiedFile::fixture(meta.clone()), staged)
            .await?;
        fs::remove_file(&placed)?;
        fs::write(&placed, b"foreign replacement")?;
        store.drain().await;
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.clean_staging(vec![]).await?;
        assert_eq!(
            store.receipt(device, meta.transfer_id.clone()).await?,
            Some((Metadata::File(meta), true))
        );
        assert_eq!(fs::read(placed)?, b"foreign replacement");
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn cache_retry_reuses_only_unchanged_identity_and_keeps_verified_replacement()
    -> Result<()> {
        for change in [
            "none",
            "truncate",
            "edit",
            "replace",
            "symlink",
            "directory",
            "missing",
            "parent",
            "parent_symlink",
            "restart",
            "evicted",
        ] {
            let (dir, mut store, device) = fixture().await?;
            let cache = dir.path().join("cache");
            let staged = stage(&store, cache.clone(), b"owned").await?;
            let old_path = staged.destination_path()?;
            let meta = begin(b"owned");
            assert!(
                store
                    .commit_cache(device.clone(), VerifiedFile::fixture(meta.clone()), staged)
                    .await?
            );
            let foreign = dir.path().join("foreign");
            fs::write(&foreign, b"foreign")?;
            match change {
                "truncate" => {
                    fs::File::options()
                        .write(true)
                        .open(&old_path)?
                        .set_len(1)?;
                }
                "edit" => {
                    let original = fs::metadata(&old_path)?.modified()?;
                    fs::write(&old_path, b"other")?;
                    // Even restoring mtime cannot conceal the in-place edit.
                    fs::File::open(&old_path)?
                        .set_times(fs::FileTimes::new().set_modified(original))?;
                }
                "replace" | "symlink" | "directory" | "missing" => {
                    fs::remove_file(&old_path)?;
                    match change {
                        "replace" => {
                            fs::copy(&foreign, &old_path)?;
                        }
                        "symlink" => symlink(&foreign, &old_path)?,
                        "directory" => fs::create_dir(&old_path)?,
                        _ => {}
                    }
                }
                "parent" | "parent_symlink" => {
                    fs::rename(&cache, dir.path().join("moved"))?;
                    if change == "parent_symlink" {
                        let external = dir.path().join("external");
                        fs::create_dir(&external)?;
                        symlink(&external, &cache)?;
                    } else {
                        fs::create_dir(&cache)?;
                    }
                    fs::write(&old_path, b"foreign")?;
                }
                "restart" => {
                    close_for_restart(store).await?;
                    store = Store::open(&dir.path().join("state.sqlite3"))?;
                }
                "evicted" => store.cache_owners.lock().unwrap().clear(),
                _ => {}
            }
            let retry_folder = if change == "parent_symlink" {
                dir.path().join("fresh-cache")
            } else {
                cache
            };
            let retry = stage(&store, retry_folder, b"owned").await?;
            let retry_path = retry.destination_path()?;
            assert_ne!(retry_path, old_path);
            let kept = store
                .commit_cache(device.clone(), VerifiedFile::fixture(meta.clone()), retry)
                .await?;
            assert_eq!(kept, change != "none", "{change}");
            let destination = store
                .call(move |c| Ok(failed_file(c, &device, &meta)?.unwrap().1))
                .await?;
            if kept {
                assert_eq!(destination, retry_path);
                assert_eq!(fs::read(&retry_path)?, b"owned");
            } else {
                assert_eq!(destination, old_path);
                assert!(!retry_path.exists());
            }
            match change {
                "truncate" => assert_eq!(fs::read(&old_path)?, b"o"),
                "edit" => assert_eq!(fs::read(&old_path)?, b"other"),
                "replace" | "parent" | "parent_symlink" => {
                    assert_eq!(fs::read(&old_path)?, b"foreign")
                }
                "symlink" => {
                    assert!(fs::symlink_metadata(&old_path)?.file_type().is_symlink())
                }
                "directory" => assert!(old_path.is_dir()),
                "missing" => assert!(!old_path.exists()),
                _ => assert_eq!(fs::read(&old_path)?, b"owned"),
            }
            assert_eq!(fs::read(&foreign)?, b"foreign");
        }
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn failed_cache_retirement_requires_live_inode_and_directory_ownership() -> Result<()> {
        for replacement in [
            "none",
            "file",
            "symlink",
            "parent",
            "parent_symlink",
            "missing",
            "restart",
        ] {
            let (dir, mut store, device) = fixture().await?;
            let cache = dir.path().join("cache");
            let staged = stage(&store, cache.clone(), b"owned").await?;
            let path = staged.destination_path()?;
            let name = path.file_name().unwrap().to_owned();
            let meta = begin(b"owned");
            assert!(
                store
                    .commit_cache(device.clone(), VerifiedFile::fixture(meta.clone()), staged)
                    .await?
            );
            let foreign = dir.path().join("foreign");
            fs::write(&foreign, b"foreign")?;
            match replacement {
                "file" | "symlink" | "missing" => {
                    fs::remove_file(&path)?;
                    match replacement {
                        "file" => fs::write(&path, b"replacement")?,
                        "symlink" => symlink(&foreign, &path)?,
                        _ => {}
                    }
                }
                "parent" | "parent_symlink" => {
                    fs::rename(&cache, dir.path().join("moved"))?;
                    if replacement == "parent" {
                        fs::create_dir(&cache)?;
                    } else {
                        fs::create_dir(dir.path().join("external"))?;
                        symlink(dir.path().join("external"), &cache)?;
                    }
                    fs::write(&path, b"replacement")?;
                }
                "restart" => {
                    close_for_restart(store).await?;
                    store = Store::open(&dir.path().join("state.sqlite3"))?;
                }
                _ => {}
            }
            let retry = stage(&store, dir.path().join("save"), b"owned").await?;
            assert!(
                store
                    .commit_file(device.clone(), VerifiedFile::fixture(meta.clone()), retry)
                    .await?
            );
            assert!(store.receipt(device, meta.transfer_id).await?.is_some());
            match replacement {
                "none" | "missing" => assert!(!path.exists()),
                "file" => assert_eq!(fs::read(&path)?, b"replacement"),
                "symlink" => assert!(fs::symlink_metadata(&path)?.file_type().is_symlink()),
                "parent" | "parent_symlink" => {
                    assert_eq!(fs::read(&path)?, b"replacement");
                    assert!(!dir.path().join("moved").join(name).exists());
                }
                "restart" => assert_eq!(fs::read(&path)?, b"owned"),
                _ => unreachable!(),
            }
            assert_eq!(fs::read(foreign)?, b"foreign");
        }
        Ok(())
    }
    #[tokio::test]
    async fn failed_cache_ownership_is_bounded_and_eviction_skips_retirement() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let mut cached = Vec::new();
        for _ in 0..=CACHE_OWNER_LIMIT {
            let staged = stage(&store, dir.path().join("cache"), b"owned").await?;
            let path = staged.destination_path()?;
            let meta = begin(b"owned");
            store
                .commit_cache(device.clone(), VerifiedFile::fixture(meta.clone()), staged)
                .await?;
            cached.push((meta, path));
        }
        let (meta, path) = {
            let owners = store.cache_owners.lock().unwrap();
            assert_eq!(owners.len(), CACHE_OWNER_LIMIT);
            cached
                .into_iter()
                .find(|(meta, _)| !owners.contains_key(&(device.clone(), meta.transfer_id.clone())))
                .unwrap()
        };
        let retry = stage(&store, dir.path().join("save"), b"owned").await?;
        store
            .commit_file(device, VerifiedFile::fixture(meta), retry)
            .await?;
        assert_eq!(fs::read(path)?, b"owned");
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[cfg(unix)]
    async fn cache_evidence_never_pins_expired_files_across_pairing_lifecycles() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        for lifecycle in ["prune", "unpair", "repair"] {
            let (dir, store, device) = fixture().await?;
            let cache = dir.path().join("cache");
            let staged = store
                .create_stage(cache.clone(), crate::storage::cache_basename("file.bin"), 5)
                .await?;
            (&staged.0.file).write_all(b"owned")?;
            let path = staged.destination_path()?;
            store
                .commit_cache(
                    device.clone(),
                    VerifiedFile::fixture(begin(b"owned")),
                    staged,
                )
                .await?;
            store.drain().await;
            let meta = fs::metadata(&path)?;
            if lifecycle == "unpair" {
                store.unpair_then(device.clone(), |_| {}).await?;
            }
            if lifecycle == "repair" {
                store
                    .pair(device, Hash::new(crate::random_hex(32))?)
                    .await?;
            }
            let folder = Folder::open(&cache, false)?;
            crate::storage::prune_cache(
                &folder,
                std::time::SystemTime::now() + std::time::Duration::from_secs(1),
            )?;
            assert!(!path.exists());
            // On Linux an open descriptor pins the deleted inode and its blocks.
            // Wait for finite stage-cleanup jobs, then prove no owner retains it.
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let held = fs::read_dir("/proc/self/fd")?
                        .filter_map(|e| e.ok())
                        .any(|entry| {
                            fs::metadata(entry.path())
                                .is_ok_and(|m| (m.dev(), m.ino()) == (meta.dev(), meta.ino()))
                        });
                    if !held {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
        }
        Ok(())
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn cache_version_survives_writer_close_for_reuse_and_retirement() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let cache = dir.path().join("cache");
        let staged = stage(&store, cache.clone(), b"owned").await?;
        let writer = staged.0.file.try_clone()?;
        let meta = begin(b"owned");
        assert!(
            store
                .commit_cache(
                    device.clone(),
                    VerifiedFile::fixture(meta.clone()),
                    staged.clone()
                )
                .await?
        );
        let original = staged.destination_path()?;
        // Cross the coarsest local-filesystem write-time granularity on both
        // sides of the final writer close, including a duplicated handle.
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        drop(staged);
        drop(writer);
        store.drain().await;
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        let retry = stage(&store, cache.clone(), b"owned").await?;
        let duplicate = retry.destination_path()?;
        assert!(
            !store
                .commit_cache(device.clone(), VerifiedFile::fixture(meta.clone()), retry)
                .await?,
            "an untouched preview must be reused after its writers close"
        );
        store.drain().await;
        assert_eq!(fs::read(&original)?, b"owned");
        assert!(!duplicate.exists());
        assert_eq!(fs::read_dir(&cache)?.count(), 2); // preview + staging directory
        let saved = stage(&store, dir.path().join("saved"), b"owned").await?;
        let saved_path = saved.destination_path()?;
        assert!(
            store
                .commit_file(device.clone(), VerifiedFile::fixture(meta.clone()), saved)
                .await?
        );
        store.drain().await;
        assert!(
            !original.exists(),
            "a successful save must retire its preview"
        );
        assert_eq!(fs::read(saved_path)?, b"owned");
        assert!(store.receipt(device, meta.transfer_id).await?.is_some());
        Ok(())
    }
    #[tokio::test]
    async fn cache_retirement_preserves_files_modified_since_commit() -> Result<()> {
        for change_size in [true, false] {
            let (dir, store, device) = fixture().await?;
            let staged = stage(&store, dir.path().join("cache"), b"owned").await?;
            let path = staged.destination_path()?;
            let meta = begin(b"owned");
            store
                .commit_cache(device.clone(), VerifiedFile::fixture(meta.clone()), staged)
                .await?;
            if change_size {
                fs::write(&path, b"modified by another user action")?;
            } else {
                fs::OpenOptions::new().write(true).open(&path)?.set_times(
                    fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH),
                )?;
            }
            let expected = fs::read(&path)?;
            let retry = stage(&store, dir.path().join("saved"), b"owned").await?;
            store
                .commit_file(device, VerifiedFile::fixture(meta), retry)
                .await?;
            assert_eq!(fs::read(path)?, expected);
        }
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn stage_cleanup_and_drop_preserve_replaced_entries() -> Result<()> {
        for explicit in [false, true] {
            for link in [false, true] {
                let (dir, store, _) = fixture().await?;
                let staged = stage(&store, dir.path().join("save"), b"owned").await?;
                let path = staged.path()?;
                let foreign = dir.path().join("foreign");
                fs::write(&foreign, b"foreign")?;
                fs::remove_file(&path)?;
                if link {
                    symlink(&foreign, &path)?;
                } else {
                    fs::write(&path, b"foreign")?;
                }
                if explicit {
                    staged.cleanup()?;
                }
                drop(staged);
                store.drain().await;
                assert_eq!(fs::read(&path)?, b"foreign");
                assert_eq!(fs::symlink_metadata(path)?.file_type().is_symlink(), link);
                assert_eq!(fs::read(foreign)?, b"foreign");
            }
        }
        Ok(())
    }
    #[tokio::test]
    // NTFS rejects directory rename while descendants are open, including
    // NtSetInformationFile with POSIX flags. Windows tests cover that refusal.
    #[cfg(unix)]
    async fn renamed_ready_folder_cannot_sweep_live_stages_through_deferred_alias() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let a = dir.path().join("save-a");
        let b = dir.path().join("previously-offline-b");
        store.clean_staging(vec![b.clone()]).await?;
        let first = stage(&store, a.clone(), b"first").await?;
        fs::rename(&a, &b)?;
        let second = stage(&store, b.clone(), b"second").await?;
        assert!(first.path()?.exists());
        assert!(second.path()?.exists());
        assert!(store.clean_staging(vec![]).await?.is_empty());
        assert!(first.path()?.exists() && second.path()?.exists());
        let first_path = first.destination_path()?;
        let second_path = second.destination_path()?;
        store
            .commit_file(
                device.clone(),
                VerifiedFile::fixture(begin(b"first")),
                first,
            )
            .await?;
        store
            .commit_file(device, VerifiedFile::fixture(begin(b"second")), second)
            .await?;
        assert_eq!(fs::read(first_path)?, b"first");
        assert_eq!(fs::read(second_path)?, b"second");
        Ok(())
    }
    #[tokio::test]
    // NTFS rejects directory rename while descendants are open, including
    // NtSetInformationFile with POSIX flags. Windows tests cover that refusal.
    #[cfg(unix)]
    async fn directory_swap_cannot_redirect_placement_or_cancellation_cleanup() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let path = dir.path().join("save");
        let staged = stage(&store, path.clone(), b"anchored").await?;
        let pending = stage(&store, path.clone(), b"cancel").await?;
        let destination = staged.0.destination.clone();
        fs::rename(&path, dir.path().join("moved"))?;
        fs::create_dir(&path)?;
        fs::write(path.join(&destination), b"foreign")?;
        store
            .commit_file(device, VerifiedFile::fixture(begin(b"anchored")), staged)
            .await?;
        drop(pending);
        store.drain().await;
        assert_eq!(fs::read(path.join(&destination))?, b"foreign");
        assert_eq!(
            fs::read(dir.path().join("moved").join(destination))?,
            b"anchored"
        );
        assert_eq!(
            fs::read_dir(dir.path().join("moved/.pneu-staging"))?.count(),
            0
        );
        Ok(())
    }
    #[tokio::test]
    async fn metadata_collision_cleans_only_its_stage_and_preserves_foreign_receipt() -> Result<()>
    {
        let (dir, store, device) = fixture().await?;
        let clipboard = text(1);
        let mut meta = begin(b"file");
        meta.transfer_id = clipboard.transfer_id.clone();
        store
            .applied_text(device.clone(), clipboard.clone())
            .await?;
        let staged = stage(&store, dir.path().join("save"), b"file").await?;
        let temporary = staged.path()?;
        let error = store
            .commit_file(device.clone(), VerifiedFile::fixture(meta), staged)
            .await
            .unwrap_err();
        assert_eq!(save_failure_code(&error), NackCode::IdConflict);
        assert!(!temporary.exists());
        assert_eq!(
            store.receipt(device, clipboard.transfer_id.clone()).await?,
            Some((
                Metadata::Text(keyed_text_receipt(
                    &store.clipboard_fingerprint_key,
                    &clipboard
                )),
                true
            ))
        );
        Ok(())
    }
    #[tokio::test]
    async fn reserve_rejects_new_records_without_mutating_existing_state() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let clipboard = text(1);
        store
            .applied_text(device.clone(), clipboard.clone())
            .await?;
        store.reserve.store(u64::MAX, Ordering::SeqCst);
        assert!(store.applied_text(device.clone(), text(2)).await.is_err());
        assert!(store.enqueue(device.clone(), text(3)).await.is_err());
        assert!(
            store
                .create_stage(dir.path().join("new"), "file".into(), 1)
                .await
                .is_err()
        );
        assert!(
            store
                .pair(
                    Id::new(crate::random_hex(16))?,
                    Hash::new(crate::random_hex(32))?
                )
                .await
                .is_err()
        );
        assert!(store.text_is_stale(device.clone(), clipboard).await?);
        assert_eq!(store.peers().await?.len(), 1);
        store.unpair_then(device, |_| {}).await?;
        assert!(store.peers().await?.is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn dropped_and_cancelled_creation_results_are_globally_tracked_until_cleanup()
    -> Result<()> {
        let (dir, store, _) = fixture().await?;
        let path = dir.path().join("save");
        let staged = stage(&store, path.clone(), b"temporary").await?;
        let name = staged.path()?;
        drop(staged);
        store.drain().await;
        assert!(!name.exists());
        let held = store.clone();
        let path2 = path.clone();
        let task =
            tokio::spawn(async move { held.create_stage(path2, "canceled".into(), 1).await });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        store.drain().await;
        assert_eq!(fs::read_dir(path.join(".pneu-staging"))?.count(), 0);
        Ok(())
    }
    #[tokio::test]
    async fn simultaneous_identical_commits_place_one_file_and_report_the_replay() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let path = dir.path().join("save");
        let bytes = b"same operation";
        let meta = begin(bytes);
        let a = stage(&store, path.clone(), bytes).await?;
        let b = stage(&store, path.clone(), bytes).await?;
        let (a, b) = tokio::join!(
            store.commit_file(device.clone(), VerifiedFile::fixture(meta.clone()), a),
            store.commit_file(device.clone(), VerifiedFile::fixture(meta.clone()), b)
        );
        assert_ne!(a?, b?, "one new completion and one duplicate");
        assert_eq!(
            fs::read_dir(&path)?
                .filter(|e| e.as_ref().is_ok_and(|e| e.path().is_file()))
                .count(),
            1
        );
        let mut conflicting = text(1);
        conflicting.transfer_id = meta.transfer_id;
        assert_eq!(
            save_failure_code(&store.applied_text(device, conflicting).await.unwrap_err()),
            NackCode::IdConflict
        );
        let count: i64 = store
            .call(|c| Ok(c.query_row("SELECT count(*) FROM clipboard_recent", [], |r| r.get(0))?))
            .await?;
        assert_eq!(
            count, 0,
            "reverse-direction ID collision didn't create conflicting text history"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pending_text_claim_blocks_file_and_is_removed_on_discard_and_cancellation()
    -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let text = text(1);
        let claim = store
            .claim_text(device.clone(), text.clone())
            .await?
            .unwrap();
        let pending = store
            .receipt(device.clone(), text.transfer_id.clone())
            .await?
            .unwrap();
        assert!(!pending.1, "pending is not a deduplicated receipt");
        assert!(
            store
                .claim_text(device.clone(), text.clone())
                .await
                .is_err()
        );
        let mut meta = begin(b"file");
        meta.transfer_id = text.transfer_id.clone();
        let stage = stage(&store, dir.path().join("save"), b"file").await?;
        assert_eq!(
            save_failure_code(
                &store
                    .commit_file(device.clone(), VerifiedFile::fixture(meta), stage)
                    .await
                    .unwrap_err()
            ),
            NackCode::IdConflict
        );
        claim.discard().await?;
        assert!(
            store
                .receipt(device.clone(), text.transfer_id.clone())
                .await?
                .is_none()
        );
        let (entered, release) = store.delay_next_peer_write();
        let held = store.clone();
        let peer = device.clone();
        let payload = text.clone();
        let task = tokio::spawn(async move { held.claim_text(peer, payload).await });
        entered.await?;
        task.abort();
        let _ = task.await;
        release.send(())?;
        store.drain().await;
        assert!(
            store.receipt(device, text.transfer_id).await?.is_none(),
            "unobserved blocking insert leaked its reservation"
        );
        Ok(())
    }
    #[test]
    fn shutdown_drops_queued_claim_cleanup_without_recursively_rescheduling() -> Result<()> {
        use std::time::Duration;
        let directory = crate::test_support::scratch()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()?;
        let store = Store::open(&directory.path().join("state.sqlite3"))?;
        let device = Id::new(crate::random_hex(16))?;
        let pending = text(1);
        let claim = runtime.block_on(async {
            store
                .pair(device.clone(), Hash::new(crate::random_hex(32))?)
                .await?;
            store
                .claim_text(device.clone(), pending.clone())
                .await?
                .context("claim")
        })?;
        // Occupy the blocking worker so cleanup remains queued at shutdown.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(2))?;
        runtime.block_on(async {
            drop(claim);
        });
        runtime.shutdown_timeout(Duration::from_millis(20));
        release_tx.send(())?;
        // Joining proves the occupied worker exits, even though the runtime is gone.
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        observer.block_on(blocker)?;
        observer.block_on(store.drain());
        assert_eq!(store.work.active(), 0);
        observer.block_on(close_for_restart(store))?;
        let reopened = Store::open(&directory.path().join("state.sqlite3"))?;
        assert!(
            observer
                .block_on(reopened.receipt(device, pending.transfer_id))?
                .is_none()
        );
        Ok(())
    }
    #[tokio::test]
    async fn obsolete_claim_cleanup_cannot_delete_same_id_after_repair() -> Result<()> {
        let (_dir, store, device) = fixture().await?;
        let text = text(1);
        let old = store
            .claim_text(device.clone(), text.clone())
            .await?
            .unwrap();
        store
            .pair(device.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        let newest = store
            .claim_text(device.clone(), text.clone())
            .await?
            .unwrap();
        old.discard().await?;
        assert!(
            store
                .receipt(device.clone(), text.transfer_id.clone())
                .await?
                .is_some()
        );
        newest.finalize().await?;
        store.drain().await;
        assert!(store.text_is_stale(device, text).await?);
        Ok(())
    }
    #[tokio::test]
    async fn startup_discards_unfinished_text_claims_without_erasing_applied_history() -> Result<()>
    {
        let (dir, store, device) = fixture().await?;
        let applied = text(1);
        let claim = store
            .claim_text(device.clone(), applied.clone())
            .await?
            .unwrap();
        claim.finalize().await?;
        let pending = text(2);
        let mut abandoned = store
            .claim_text(device.clone(), pending.clone())
            .await?
            .unwrap();
        abandoned.token.take(); // Simulate process death before the effect/finalization.
        drop(abandoned);
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        assert!(
            store
                .receipt(device.clone(), pending.transfer_id)
                .await?
                .is_none()
        );
        assert!(store.text_is_stale(device, applied).await?);
        Ok(())
    }
    #[tokio::test]
    async fn legacy_migration_preserves_completed_files_and_staging_folder_discovery() -> Result<()>
    {
        let (dir, store, device) = fixture().await?;
        let folder = dir.path().join("previous-save");
        let staged = stage(&store, folder.clone(), b"saved").await?;
        let path = staged.destination_path()?;
        let meta = begin(b"saved");
        store
            .commit_file(device.clone(), VerifiedFile::fixture(meta.clone()), staged)
            .await?;
        let hint = serde_json::json!({"folder":folder, "dev":1,"ino":2}).to_string();
        store.call(move |c| {
            c.execute_batch("DELETE FROM staging_folders; ALTER TABLE receipts ADD COLUMN complete INTEGER NOT NULL DEFAULT 1; ALTER TABLE receipts ADD COLUMN location TEXT; ALTER TABLE receipts ADD COLUMN rolling_back INTEGER NOT NULL DEFAULT 0;")?;
            c.execute("UPDATE receipts SET location=?", [hint])?;
            Ok(())
        }).await?;
        let staging = Folder::open_private(&folder.join(".pneu-staging"))?;
        let mut orphan = crate::platform::fs::create_private_at(
            &staging.file,
            "orphan",
            crate::platform::fs::Access::Write,
        )?;
        orphan.write_all(b"partial")?;
        drop((orphan, staging));
        store.drain().await;
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        store.clean_staging(vec![]).await?;
        assert_eq!(fs::read_dir(folder.join(".pneu-staging"))?.count(), 0);
        assert_eq!(fs::read(path)?, b"saved");
        assert_eq!(
            store.receipt(device, meta.transfer_id.clone()).await?,
            Some((Metadata::File(meta), true))
        );
        let obsolete: i64 = store.call(|c| Ok(c.query_row("SELECT count(*) FROM pragma_table_info('receipts') WHERE name IN ('location','complete','rolling_back')", [], |r| r.get(0))?)).await?;
        assert_eq!(obsolete, 0);
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn save_error_causes_survive_context() {
        for (errno, code) in [
            (libc_errno(libc::ENOSPC), NackCode::DiskFull),
            (libc_errno(libc::EDQUOT), NackCode::DiskFull),
            (libc_errno(libc::EACCES), NackCode::NotWritable),
            (libc_errno(libc::EROFS), NackCode::NotWritable),
        ] {
            assert_eq!(
                save_failure_code(&anyhow::Error::new(errno).context("save")),
                code
            );
        }
        fn libc_errno(code: i32) -> std::io::Error {
            std::io::Error::from_raw_os_error(code)
        }
    }
    #[tokio::test]
    async fn legacy_claim_does_not_allocate_effect_stamp() -> Result<()> {
        let (_dir, store, device) = fixture().await?;
        let before: String = store
            .call(|c| Ok(c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?))
            .await?;
        let claim = store.claim_text(device, text(1)).await?.unwrap();
        let after: String = store
            .call(|c| Ok(c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?))
            .await?;
        claim.discard().await?;
        assert_eq!(
            before, after,
            "admission allocated a stamp before the OS effect boundary"
        );
        Ok(())
    }
    #[tokio::test]
    async fn mesh_clock_tie_break_history_skew_exhaustion_and_restart() -> Result<()> {
        let (dir, store, first) = fixture().await?;
        let low = Id::from_bytes([0; 16]);
        let high = Id::from_bytes([255; 16]);
        for peer in [&low, &high] {
            store
                .pair(peer.clone(), Hash::new(crate::random_hex(32))?)
                .await?;
        }
        let mut operation = text(1);
        let hlc = crate::now_ms() + 100;
        operation.hlc = Some(hlc);
        operation.source_device = Some(low.clone());
        let claim = store
            .claim_text(low.clone(), operation.clone())
            .await?
            .unwrap();
        claim.finalize().await?;
        assert!(!store.stamp_eligible(hlc, low.clone()).await?);
        assert!(store.stamp_eligible(hlc, high.clone()).await?);
        let mut conflict = operation.clone();
        conflict.hlc = Some(hlc + 1);
        assert!(
            store.claim_text(low.clone(), conflict).await.is_err(),
            "stamp is part of ID metadata equality"
        );
        assert!(validate_stamp(u64::MAX, crate::now_ms()).is_err());
        assert!(validate_stamp(u64::MAX, u64::MAX - 60_000).is_ok());
        assert!(validate_stamp(u64::MAX, u64::MAX - 60_001).is_err());
        store.unpair_then(low, |_| {}).await?;
        assert!(
            !store.stamp_eligible(hlc - 1, high).await?,
            "unpair preserves global register"
        );
        let (_, next) = store.local_clipboard(first.clone()).await?;
        assert!(next > hlc);
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let (_, after) = store.local_clipboard(first.clone()).await?;
        assert!(after > next);
        store
            .call(|c| {
                c.execute("UPDATE mesh_clock SET last_seen=?", [u64::MAX.to_string()])?;
                Ok(())
            })
            .await?;
        let old = store
            .call(|c| {
                Ok(c.query_row("SELECT hlc FROM clipboard_clock", [], |r| {
                    r.get::<_, String>(0)
                })?)
            })
            .await?;
        assert!(store.local_clipboard(first).await.is_err());
        assert_eq!(
            store
                .call(
                    |c| Ok(c.query_row("SELECT hlc FROM clipboard_clock", [], |r| r
                        .get::<_, String>(0))?)
                )
                .await?,
            old
        );
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[path = "../tests/fixtures/released_store.rs"]
mod released_store;

#[cfg(test)]
#[path = "store_settings_tests.rs"]
mod settings_tests;

#[cfg(test)]
#[path = "store_members_tests.rs"]
mod members_tests;
