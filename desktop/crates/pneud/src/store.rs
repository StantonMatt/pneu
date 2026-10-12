//! One phone, completed file receipts, bounded clipboard history, latest outgoing text.
use crate::storage::{Folder, Stage, VerifiedFile, clean_staging, staging_directory};
use crate::work::{Work, start_blocking};
use anyhow::{Context, Result, ensure};
use pneu_proto::{Clipboard, ClipboardOrigin, FileBegin, Hash, Id, NackCode};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextReceipt {
    pub transfer_id: Id,
    pub sha256: Hash,
    pub created_at: u64,
    #[serde(default)]
    pub origin: ClipboardOrigin,
}
impl From<&Clipboard> for TextReceipt {
    fn from(text: &Clipboard) -> Self {
        Self {
            transfer_id: text.transfer_id.clone(),
            sha256: Hash::new(hex::encode(Sha256::digest(text.text.as_bytes()))).expect("SHA-256"),
            created_at: text.created_at,
            origin: text.origin,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation")]
pub enum Metadata {
    File(FileBegin),
    Text(TextReceipt),
}

pub(crate) const RECENT_TEXT_LIMIT: usize = 64;
const CACHE_OWNER_LIMIT: usize = 128;
struct CacheOwner {
    path: PathBuf,
    file: crate::storage::OwnedFile,
}
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    _ownership: Arc<StoreOwnership>,
    folder: Folder,
    reserve: Arc<AtomicU64>,
    work: Arc<Work>,
    pub(crate) connection_id: Option<u64>,
    generation: Arc<AtomicU64>,
    peer_generation: Option<u64>,
    staging_ready: Arc<Mutex<HashSet<(u64, u64)>>>,
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
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
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
            "SELECT generation=? AND EXISTS(SELECT 1 FROM peers WHERE id=?) FROM pairing_epoch WHERE singleton=1",
            params![self.generation as i64, self.device.as_str()], |r| r.get(0))?;
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
struct ReceiptConflict;
impl std::fmt::Display for ReceiptConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("receipt ID conflict")
    }
}
impl std::error::Error for ReceiptConflict {}
/// A short-lived shared-ID reservation, not a replay queue. The claim owns its
/// cleanup even if the caller disappears while a blocking insert is finishing.
pub(crate) struct TextClaim {
    // Bridge completion of the blocking insert to observation/drop of its
    // result, so shutdown cannot miss cleanup of an abandoned returned claim.
    work: Option<crate::work::Token>,
    epoch: PeerEpoch,
    store: Store,
    device: Id,
    text: Clipboard,
    token: Option<String>,
}
impl TextClaim {
    pub(crate) async fn discard(mut self) -> Result<()> {
        let store = self.store.clone();
        store
            .blocking(move |_| {
                self.release()?;
                Ok(())
            })
            .await
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
    pub(crate) async fn finalize(
        mut self,
        application: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<()> {
        let store = self.store.clone();
        let epoch = self.epoch.clone();
        store
            .blocking(move |store| {
                let _application = application;
                store.peer_write(epoch, |tx| {
                    let token = self.token.as_ref().context("missing text claim")?;
                    let removed = tx.execute(
                        "DELETE FROM receipts WHERE device=? AND id=? AND destination=?",
                        params![self.device.as_str(), self.text.transfer_id.as_str(), token],
                    )?;
                    ensure!(removed == 1, "text claim lost");
                    record_text(tx, &self.device, &self.text)?;
                    Ok(())
                })?;
                self.token.take();
                drop(self);
                Ok(())
            })
            .await
    }
}
impl Drop for TextClaim {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            // Only plain data enters the cleanup task. If runtime shutdown drops
            // the queued closure, it must not drop another TextClaim and recursively
            // enqueue cleanup. Startup removes any abandoned reservation.
            let work = self.work.take();
            let epoch = self.epoch.clone();
            let device = self.device.clone();
            let id = self.text.transfer_id.clone();
            self.store.start_blocking(move |store| {
                let _work = work;
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
    Ok(())
}
fn record_text(tx: &Transaction<'_>, device: &Id, text: &Clipboard) -> Result<()> {
    tx.execute("INSERT INTO clipboard_clock VALUES (?,?) ON CONFLICT(device) DO UPDATE SET created_at=max(created_at,excluded.created_at)", params![device.as_str(), text.created_at as i64])?;
    tx.execute(
        "INSERT INTO clipboard_recent VALUES (?,?,?)",
        params![
            device.as_str(),
            text.transfer_id.as_str(),
            serde_json::to_string(&Metadata::Text(text.into()))?
        ],
    )?;
    tx.execute("DELETE FROM clipboard_recent WHERE device=? AND rowid NOT IN (SELECT rowid FROM clipboard_recent WHERE device=? ORDER BY rowid DESC LIMIT ?)", params![device.as_str(), device.as_str(), RECENT_TEXT_LIMIT as i64])?;
    Ok(())
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            !path.exists() || !fs::symlink_metadata(path)?.file_type().is_symlink(),
            "database is a symlink"
        );
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path.with_extension("lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("data store is already owned by another daemon")?;
        let conn = Connection::open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;
          CREATE TABLE IF NOT EXISTS peers(id TEXT PRIMARY KEY, fp TEXT UNIQUE NOT NULL);
          CREATE TABLE IF NOT EXISTS receipts(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,id TEXT NOT NULL,meta TEXT NOT NULL,destination TEXT,PRIMARY KEY(device,id));
          CREATE TABLE IF NOT EXISTS outgoing(seq INTEGER PRIMARY KEY AUTOINCREMENT,device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,payload TEXT NOT NULL,retry_at INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS clipboard_clock(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,created_at INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS clipboard_recent(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,id TEXT NOT NULL,meta TEXT NOT NULL,PRIMARY KEY(device,id));
          CREATE TABLE IF NOT EXISTS failed_cache(device TEXT NOT NULL REFERENCES peers(id) ON DELETE CASCADE,id TEXT NOT NULL,meta TEXT NOT NULL,destination TEXT NOT NULL,PRIMARY KEY(device,id));
          CREATE TABLE IF NOT EXISTS staging_folders(path TEXT PRIMARY KEY);
          CREATE TABLE IF NOT EXISTS pairing_epoch(singleton INTEGER PRIMARY KEY CHECK(singleton=1),generation INTEGER NOT NULL,last_outgoing_at INTEGER NOT NULL DEFAULT 0);
          INSERT OR IGNORE INTO pairing_epoch(singleton,generation) VALUES (1,0);")?;
        // Keep earlier folder hints before retiring receipt recovery columns.
        // They authorize disposable-root sweeping, never deletion of final files.
        let old_locations: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('receipts') WHERE name='location')",
            [],
            |r| r.get(0),
        )?;
        if old_locations {
            conn.execute_batch("INSERT OR IGNORE INTO staging_folders SELECT json_extract(location,'$.folder') FROM receipts WHERE json_valid(location) AND json_type(location,'$.folder')='text';")?;
        }
        let legacy_complete: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('receipts') WHERE name='complete')",
            [],
            |r| r.get(0),
        )?;
        if legacy_complete {
            conn.execute("DELETE FROM receipts WHERE complete=0", [])?;
        }
        // Retire earlier pending/rollback receipts without touching user files.
        // Preserve completed file dedupe, but clipboard history has a bounded horizon.
        conn.execute_batch(
            "DROP TABLE IF EXISTS stages;
            DELETE FROM receipts WHERE json_extract(meta,'$.operation')='Text';
            DELETE FROM peers WHERE rowid != (SELECT max(rowid) FROM peers);
            DELETE FROM outgoing WHERE seq NOT IN (SELECT max(seq) FROM outgoing GROUP BY device);",
        )?;
        for column in ["staging", "location", "rolling_back", "complete"] {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('receipts') WHERE name=?)",
                [column],
                |r| r.get(0),
            )?;
            if exists {
                conn.execute_batch(&format!("ALTER TABLE receipts DROP COLUMN {column}"))?;
            }
        }
        let legacy_blocked: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('outgoing') WHERE name='blocked')",
            [],
            |r| r.get(0),
        )?;
        if legacy_blocked {
            conn.execute_batch(
                "DELETE FROM outgoing WHERE blocked=1; ALTER TABLE outgoing DROP COLUMN blocked;",
            )?;
        }
        conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS latest_text ON outgoing(device);
            UPDATE pairing_epoch SET last_outgoing_at=max(last_outgoing_at,coalesce(
                (SELECT max(json_extract(payload,'$.created_at')) FROM outgoing),0)) WHERE singleton=1;")?;
        let generation: i64 = conn.query_row(
            "SELECT generation FROM pairing_epoch WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        let folder = Folder::open(path.parent().context("database parent")?, false)?;
        folder.file.sync_all()?;
        Ok(Self {
            connection: Arc::new(Mutex::new(conn)),
            _ownership: Arc::new(StoreOwnership(lock)),
            folder,
            reserve: Arc::new(AtomicU64::new(16 * 1024 * 1024)),
            work: Arc::new(Work::default()),
            connection_id: None,
            generation: Arc::new(AtomicU64::new(generation as u64)),
            peer_generation: None,
            staging_ready: Arc::new(Mutex::new(HashSet::new())),
            cache_owners: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            peer_write_barrier: Arc::new(Mutex::new(None)),
        })
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
        store.peer_generation = Some(self.generation.load(Ordering::SeqCst));
        store
    }
    fn peer_epoch(&self, device: &Id) -> PeerEpoch {
        PeerEpoch {
            device: device.clone(),
            generation: self
                .peer_generation
                .unwrap_or_else(|| self.generation.load(Ordering::SeqCst)),
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
    pub(crate) async fn drain(&self) {
        self.work.drain().await;
    }
    pub(crate) fn start_blocking<T: Send + 'static>(
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
        self.start_blocking(f).await?
    }
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.blocking(move |store| store.locked(f)).await
    }
    pub async fn admission(&self) -> Result<()> {
        self.blocking(|store| capacity(&store.folder, store.reserve.load(Ordering::SeqCst)))
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
    #[cfg(test)]
    pub async fn pair(&self, device: Id, fp: Hash) -> Result<()> {
        self.pair_then(device, fp, || {}).await
    }
    pub(crate) async fn pair_then(
        &self,
        device: Id,
        fp: Hash,
        finish: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        self.blocking(move |store| {
            store.locked(|c| {
                capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                let tx = c.transaction()?;
                tx.execute("DELETE FROM peers", [])?;
                tx.execute(
                    "INSERT INTO peers(id,fp) VALUES (?,?)",
                    params![device.as_str(), fp.as_str()],
                )?;
                tx.execute(
                    "UPDATE pairing_epoch SET generation=generation+1 WHERE singleton=1",
                    [],
                )?;
                let generation: i64 = tx.query_row(
                    "SELECT generation FROM pairing_epoch WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                tx.commit()?;
                store.generation.store(generation as u64, Ordering::SeqCst);
                Ok(())
            })?;
            finish();
            Ok(())
        })
        .await
    }
    pub(crate) async fn unpair_then(
        &self,
        device: Id,
        finish: impl FnOnce(bool) + Send + 'static,
    ) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            let result = store.locked(|c| {
                let tx = c.transaction()?;
                epoch.check(&tx)?;
                ensure!(
                    tx.execute("DELETE FROM peers WHERE id=?", [device.as_str()])? == 1,
                    "pairing was not removed"
                );
                tx.execute(
                    "UPDATE pairing_epoch SET generation=generation+1 WHERE singleton=1",
                    [],
                )?;
                let generation: i64 = tx.query_row(
                    "SELECT generation FROM pairing_epoch WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                tx.commit()?;
                store.generation.store(generation as u64, Ordering::SeqCst);
                Ok(())
            });
            // The callback owns lifecycle restoration and the pairing gate even
            // when the awaiting async command has been canceled.
            finish(result.is_ok());
            result
        })
        .await
    }
    pub async fn receipt(&self, device: Id, id: Id) -> Result<Option<(Metadata, bool)>> {
        let epoch = self.peer_epoch(&device);
        self.call(move |c| {
            epoch.check(c)?;
            receipt_row(c, device.as_str(), id.as_str())
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
                    Err(e)
                        if e.downcast_ref::<rustix::io::Errno>()
                            == Some(&rustix::io::Errno::NOENT) => {}
                    Err(e) => failures.push((path, e.to_string())),
                }
            }
            Ok(failures)
        })
        .await
    }
    pub async fn discard_stage(&self, stage: Stage) -> Result<()> {
        self.blocking(move |_| stage.cleanup()).await
    }
    pub async fn commit_file(
        &self,
        device: Id,
        verified: VerifiedFile,
        stage: Stage,
    ) -> Result<bool> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
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
    ) -> Result<Option<TextClaim>> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.peer_write(epoch.clone(), |tx| {
                let expected = Metadata::Text((&text).into());
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
                let timestamp: Option<i64> = tx
                    .query_row(
                        "SELECT created_at FROM clipboard_clock WHERE device=?",
                        [device.as_str()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if timestamp.is_some_and(|t| text.created_at <= t as u64) {
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
                Ok(Some(TextClaim {
                    work: Some(crate::work::track(vec![store.work.clone()])),
                    epoch,
                    store: store.clone(),
                    device,
                    text,
                    token: Some(token),
                }))
            })
        })
        .await
    }
    #[cfg(test)]
    pub async fn text_is_stale(&self, device: Id, text: Clipboard) -> Result<bool> {
        let epoch = self.peer_epoch(&device);
        self.call(move |c| {
            epoch.check(c)?;
            let expected = Metadata::Text((&text).into());
            if let Some((previous, complete)) =
                receipt_row(c, device.as_str(), text.transfer_id.as_str())?
            {
                ensure!(previous == expected, ReceiptConflict);
                ensure!(complete, "clipboard operation still pending");
                return Ok(true);
            }
            let timestamp: Option<i64> = c
                .query_row(
                    "SELECT created_at FROM clipboard_clock WHERE device=?",
                    [device.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(timestamp.is_some_and(|t| text.created_at <= t as u64))
        })
        .await
    }
    #[cfg(test)]
    async fn applied_text(
        &self,
        device: Id,
        text: Clipboard,
        application: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        // Async cancellation must not release application ordering before this
        // uncancelable history write finishes. This is a short global clipboard
        // critical section, not a socket/device delivery lease or durable barrier.
        self.blocking(move |store| {
            let _application = application;
            store.peer_write(epoch, |tx| {
                let expected = Metadata::Text((&text).into());
                if let Some((previous, _)) =
                    receipt_row(tx, device.as_str(), text.transfer_id.as_str())?
                {
                    ensure!(previous == expected, ReceiptConflict);
                    return Ok(());
                }
                capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                record_text(tx, &device, &text)?;
                Ok(())
            })
        })
        .await
    }
    /// Local timestamp assignment and enqueue share one peer write transaction.
    pub async fn enqueue_clipboard(&self, device: Id, text: String) -> Result<Clipboard> {
        self.prepare_clipboard(device, text, true).await
    }
    pub async fn ephemeral_clipboard(&self, device: Id, text: String) -> Result<Clipboard> {
        self.prepare_clipboard(device, text, false).await
    }
    async fn prepare_clipboard(
        &self,
        device: Id,
        text: String,
        persist: bool,
    ) -> Result<Clipboard> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.peer_write(epoch, |tx| {
                let previous: i64 = tx.query_row(
                    "SELECT last_outgoing_at FROM pairing_epoch WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                let text = Clipboard {
                    transfer_id: Id::new(crate::random_hex(16))?,
                    text,
                    created_at: crate::now_ms().max((previous as u64).saturating_add(1)),
                    origin: ClipboardOrigin::Clipboard,
                };
                if persist {
                    replace_outgoing(store, tx, &device, &text)?;
                } else {
                    pneu_proto::encode_frame(&pneu_proto::Message::Clipboard(text.clone()))?;
                    tx.execute(
                        "UPDATE pairing_epoch SET last_outgoing_at=? WHERE singleton=1",
                        [text.created_at as i64],
                    )?;
                }
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
    pub async fn outgoing_receipt(
        &self,
        device: Id,
        seq: i64,
        blocked: bool,
        ack: bool,
    ) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.peer_write(epoch, |tx| {
                if ack || blocked {
                    tx.execute(
                        "DELETE FROM outgoing WHERE device=? AND seq=?",
                        params![device.as_str(), seq],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE outgoing SET retry_at=? WHERE device=? AND seq=?",
                        params![
                            (crate::now_ms() + 48_000 + rand::random::<u64>() % 24_001) as i64,
                            device.as_str(),
                            seq
                        ],
                    )?;
                }
                Ok(())
            })
        })
        .await
    }
    pub async fn discard_outgoing(&self, device: Id) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.peer_write(epoch, |tx| {
                tx.execute("DELETE FROM outgoing WHERE device=?", [device.as_str()])?;
                Ok(())
            })
        })
        .await
    }
}
fn replace_outgoing(
    store: &Store,
    tx: &Transaction<'_>,
    device: &Id,
    text: &Clipboard,
) -> Result<()> {
    // Validate encoded size before modifying timestamps or the existing value.
    pneu_proto::encode_frame(&pneu_proto::Message::Clipboard(text.clone()))?;
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
        "UPDATE pairing_epoch SET last_outgoing_at=max(last_outgoing_at,?) WHERE singleton=1",
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
    let row: Option<(String, bool)> = c.query_row("SELECT meta,json_extract(meta,'$.operation')!='Text' FROM receipts WHERE device=? AND id=? UNION ALL SELECT meta,1 FROM clipboard_recent WHERE device=? AND id=?", params![device,id,device,id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    row.map(|(m, complete)| Ok((serde_json::from_str(&m)?, complete)))
        .transpose()
}
/// Preserve OS causes through anyhow contexts, including rustix filesystem errors.
pub(crate) fn save_failure_code(error: &anyhow::Error) -> NackCode {
    for cause in error.chain() {
        if cause.downcast_ref::<ReceiptConflict>().is_some() {
            return NackCode::IdConflict;
        }
        let errno = cause
            .downcast_ref::<std::io::Error>()
            .and_then(|e| e.raw_os_error())
            .or_else(|| {
                cause
                    .downcast_ref::<rustix::io::Errno>()
                    .map(|e| e.raw_os_error())
            });
        if let Some(errno) = errno {
            match rustix::io::Errno::from_raw_os_error(errno) {
                rustix::io::Errno::NOSPC | rustix::io::Errno::DQUOT => return NackCode::DiskFull,
                rustix::io::Errno::ACCESS
                | rustix::io::Errno::ROFS
                | rustix::io::Errno::PERM
                | rustix::io::Errno::NOENT
                | rustix::io::Errno::NOTDIR
                | rustix::io::Errno::EXIST => return NackCode::NotWritable,
                _ => {}
            }
        }
    }
    NackCode::StorageFailed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;
    fn begin(bytes: &[u8]) -> FileBegin {
        FileBegin {
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
    async fn fixture() -> Result<(tempfile::TempDir, Store, Id)> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let device = Id::new(crate::random_hex(16))?;
        store
            .pair(device.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        Ok((dir, store, device))
    }
    fn text(created_at: u64) -> Clipboard {
        Clipboard {
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
    async fn delayed_peer_writes_cannot_cross_pairing_generations() -> Result<()> {
        // Both same-ID replacement and unpair/re-pair must revoke every write
        // class, including no-op receipts and jobs that outlive their caller.
        for unpair_first in [false, true] {
            for operation in 0..10 {
                let (dir, store, device) = fixture().await?;
                let old_scope = store.with_current_pairing();
                let pending = text(1);
                store.enqueue(device.clone(), pending).await?;
                let seq = store.first_outgoing(device.clone()).await?.unwrap().0;
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
                        1 => writer.applied_text(peer, text(99), None).await,
                        2 => writer.enqueue(peer, text(99)).await,
                        3 => writer
                            .enqueue_clipboard(peer, "revoked".into())
                            .await
                            .map(|_| ()),
                        4 => writer.outgoing_receipt(peer, seq, false, true).await,
                        5 => writer.outgoing_receipt(peer, seq, true, false).await,
                        6 => writer.outgoing_receipt(peer, seq, false, false).await,
                        7 => writer.discard_outgoing(peer).await,
                        8 => writer.claim_text(peer, text(99)).await.map(|_| ()),
                        _ => {
                            claim
                                .unwrap()
                                .finalize(Arc::new(tokio::sync::Mutex::new(())).lock_owned().await)
                                .await
                        }
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
                store
                    .applied_text(device.clone(), applied.clone(), None)
                    .await?;
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
                assert!(old_scope.discard_outgoing(device).await.is_err());
            }
        }
        Ok(())
    }
    #[tokio::test]
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
        let generation = store.generation.load(Ordering::SeqCst);
        let future = crate::now_ms() + 100_000;
        store.enqueue(device.clone(), text(future)).await?;
        store.unpair_then(device.clone(), |_| {}).await?;
        store
            .pair(device.clone(), Hash::new(crate::random_hex(32))?)
            .await?;
        store.drain().await;
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        assert_eq!(store.generation.load(Ordering::SeqCst), generation + 2);
        let clipboard = store
            .enqueue_clipboard(device.clone(), "new".into())
            .await?;
        assert!(clipboard.created_at > future);
        assert_eq!(store.first_outgoing(device).await?.unwrap().1, clipboard);
        Ok(())
    }
    #[tokio::test]
    async fn legacy_pending_text_seeds_serialized_clock_before_first_local_enqueue() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let future = crate::now_ms() + 100_000;
        store.enqueue(device.clone(), text(future)).await?;
        store
            .call(|c| {
                c.execute_batch("DROP TABLE pairing_epoch;")?;
                Ok(())
            })
            .await?;
        store.drain().await;
        close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
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
    async fn wal_full_and_single_pairing_replace_all_old_peer_state() -> Result<()> {
        let (_dir, store, device) = fixture().await?;
        store.applied_text(device.clone(), text(10), None).await?;
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
        assert_eq!((mode.as_str(), synchronous, old), ("wal", 2, 0));
        assert_eq!(
            store.peers().await?.into_values().collect::<Vec<_>>(),
            vec![newer]
        );
        Ok(())
    }
    #[tokio::test]
    async fn bounded_lww_history_survives_restart_and_keeps_no_plaintext() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let first = text(1);
        assert!(!store.text_is_stale(device.clone(), first.clone()).await?);
        store
            .applied_text(device.clone(), first.clone(), None)
            .await?;
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
            store
                .applied_text(device.clone(), text(timestamp), None)
                .await?;
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
    async fn latest_outgoing_replaces_unsent_and_old_ack_or_nack_cannot_delete_it() -> Result<()> {
        let (_dir, store, device) = fixture().await?;
        store.enqueue(device.clone(), text(1)).await?;
        let (old, _, _) = store.first_outgoing(device.clone()).await?.unwrap();
        let latest = text(3);
        store.enqueue(device.clone(), text(2)).await?;
        store.enqueue(device.clone(), latest.clone()).await?;
        store
            .outgoing_receipt(device.clone(), old, false, true)
            .await?;
        store
            .outgoing_receipt(device.clone(), old, true, false)
            .await?;
        assert_eq!(store.first_outgoing(device).await?.unwrap().1, latest);
        Ok(())
    }
    #[tokio::test]
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
    #[tokio::test]
    async fn cache_evidence_never_pins_expired_files_across_pairing_lifecycles() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        for lifecycle in ["prune", "unpair", "repair"] {
            let (dir, store, device) = fixture().await?;
            let cache = dir.path().join("cache");
            let staged = store
                .create_stage(
                    cache.clone(),
                    format!("received-{}", crate::random_hex(16)),
                    5,
                )
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
                fs::File::open(&path)?.set_times(
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
            .applied_text(device.clone(), clipboard.clone(), None)
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
            Some((Metadata::Text((&clipboard).into()), true))
        );
        Ok(())
    }
    #[tokio::test]
    async fn reserve_rejects_new_records_without_mutating_existing_state() -> Result<()> {
        let (dir, store, device) = fixture().await?;
        let clipboard = text(1);
        store
            .applied_text(device.clone(), clipboard.clone(), None)
            .await?;
        store.reserve.store(u64::MAX, Ordering::SeqCst);
        assert!(
            store
                .applied_text(device.clone(), text(2), None)
                .await
                .is_err()
        );
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
            save_failure_code(
                &store
                    .applied_text(device, conflicting, None)
                    .await
                    .unwrap_err()
            ),
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
    async fn canceled_history_commit_keeps_clipboard_application_order_until_blocking_work_finishes()
    -> Result<()> {
        let (_dir, store, device) = fixture().await?;
        let application = Arc::new(tokio::sync::Mutex::new(()));
        let owned = application.clone().lock_owned().await;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        let held = store.clone();
        let holder = tokio::spawn(async move {
            held.call(move |_| {
                let _ = entered.send(());
                blocking.recv()?;
                Ok(())
            })
            .await
        });
        wait.await?;
        let recording = store.clone();
        let peer = device.clone();
        let record =
            tokio::spawn(async move { recording.applied_text(peer, text(2), Some(owned)).await });
        while store.active_jobs() < 2 {
            tokio::task::yield_now().await;
        }
        record.abort();
        let _ = record.await;
        let retained = application.try_lock().is_err();
        release.send(())?;
        holder.await??;
        store.drain().await;
        assert!(retained);
        assert!(application.try_lock().is_ok());
        assert!(store.text_is_stale(device, text(1)).await?);
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
        let directory = tempfile::tempdir()?;
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
        newest
            .finalize(Arc::new(tokio::sync::Mutex::new(())).lock_owned().await)
            .await?;
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
        claim
            .finalize(Arc::new(tokio::sync::Mutex::new(())).lock_owned().await)
            .await?;
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
        fs::write(folder.join(".pneu-staging/orphan"), b"partial")?;
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
    #[test]
    fn save_error_causes_survive_context() {
        for (errno, code) in [
            (libc_errno(28), NackCode::DiskFull),
            (libc_errno(122), NackCode::DiskFull),
            (libc_errno(13), NackCode::NotWritable),
            (libc_errno(30), NackCode::NotWritable),
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
}
