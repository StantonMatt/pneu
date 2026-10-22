// Released 2.0.2 startup fixture, from 55c8068:desktop/crates/pneud/src/store.rs.
// Store fields and Store::open are copied verbatim; supporting types come from the parent.
use super::*;

#[allow(dead_code)]
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
}
