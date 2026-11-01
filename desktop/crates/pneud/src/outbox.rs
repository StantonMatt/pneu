//! Durable origin-only file queue: immutable bytes, frozen recipient authority.
use super::*;
use crate::platform::fs as platform_fs;
use pneu_proto::{FileKind, Message};
use std::io::{Read, Write};

pub const GLOBAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const PEER_BYTES: u64 = 1024 * 1024 * 1024;
pub const GLOBAL_DELIVERIES: usize = 256;
pub const PEER_DELIVERIES: usize = 128;
pub const DISK_RESERVE: u64 = 64 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Targets {
    All(String),
    Captured(Vec<Authority>),
    // Kept for CLI --to and older clients. UI pickers use captured authority.
    Devices(Vec<Id>),
}
impl Targets {
    pub(crate) fn explicit(&self) -> Result<Option<&[Id]>> {
        match self {
            Self::All(s) => {
                ensure!(s == "all", "targets must be all or device IDs");
                Ok(None)
            }
            Self::Captured(_) => anyhow::bail!("captured targets require authority validation"),
            Self::Devices(ids) => {
                ensure!(!ids.is_empty(), "choose at least one device");
                Ok(Some(ids))
            }
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transfer {
    pub operation: Id,
    /// Stable artifact identity; also used as `artifact` in transfer commands.
    pub transfer_id: Id,
    pub device: Id,
    pub file_name: String,
    pub size: u64,
    pub sent: u64,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default = "outgoing_direction")]
    pub direction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip)]
    pub(crate) generation: u64,
}
fn outgoing_direction() -> String {
    "outgoing".into()
}
impl Transfer {
    pub(crate) fn received(device: Id, begin: &FileBegin) -> Self {
        Self {
            operation: Id::new(crate::random_hex(16)).expect("random ID"),
            transfer_id: begin.transfer_id.clone(),
            device,
            file_name: begin.file_name.clone(),
            size: begin.size,
            sent: 0,
            state: "sending".into(),
            error: None,
            direction: "incoming".into(),
            finished_at: None,
            path: None,

            generation: 0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendTarget {
    pub device: Id,
    pub name: String,
    #[serde(default)]
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Hash>,
    pub eligible: bool,
    pub would_queue: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority {
    pub device: Id,
    pub generation: u64,
    pub fingerprint: Hash,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ArtifactStamp {
    created_at: u64,
    hlc: u64,
    source_device: Id,
}
#[derive(Debug)]
pub(crate) struct RecipientRejection {
    pub device: Id,
    pub reason: String,
}
#[derive(Debug)]
pub(crate) struct AdmissionFailure {
    pub reason: String,
    pub rejected: Vec<RecipientRejection>,
    pub recipient_only: bool,
}
impl std::fmt::Display for AdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}
impl std::error::Error for AdmissionFailure {}
#[derive(Clone, Debug)]
pub(crate) struct Delivery {
    pub begin: FileBegin,
    pub authority: Authority,
    pub session: u64,
}
#[cfg(test)]
type SourceHook = Arc<dyn Fn(&Path) + Send + Sync>;
#[cfg(test)]
type TestBarrier = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
type RecoveryClaims = HashMap<(Id, u64, u64), Option<String>>;
#[derive(Clone)]
pub(crate) struct Outbox {
    store: Store,
    folder: Folder,
    reserve: u64,
    admission: Arc<tokio::sync::Mutex<()>>,
    // File copies and replay hashes must not reserve shared Store capacity.
    io: Arc<crate::work::Work>,
    view: Arc<Mutex<Vec<Transfer>>>,
    capabilities: Arc<Mutex<HashMap<Id, (u64, bool)>>>,
    recovery: Arc<Mutex<RecoveryClaims>>,
    operations: Arc<Mutex<Vec<AdmissionOperation>>>,
    // Cancellation outlives visibility/pruning of the operation history until
    // its admission worker has stopped touching sources.
    cancelled_admissions: Arc<Mutex<HashSet<Id>>>,
    #[cfg(test)]
    pub(crate) source_hook: Arc<Mutex<Option<SourceHook>>>,
    #[cfg(test)]
    copy_hook: Arc<Mutex<Option<SourceHook>>>,
    #[cfg(test)]
    pub(crate) claim_barrier: Arc<Mutex<Option<TestBarrier>>>,
    #[cfg(test)]
    pub(crate) reader_barrier: Arc<Mutex<Option<TestBarrier>>>,
    #[cfg(test)]
    projection_barrier: Arc<Mutex<Option<TestBarrier>>>,
}
pub(super) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS outbox_artifacts(id TEXT PRIMARY KEY,operation TEXT NOT NULL,meta TEXT NOT NULL,staged INTEGER NOT NULL DEFAULT 1);
        CREATE TABLE IF NOT EXISTS outbox_deliveries(artifact TEXT NOT NULL REFERENCES outbox_artifacts(id) ON DELETE CASCADE,device TEXT NOT NULL,generation INTEGER NOT NULL,fingerprint TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'queued',error TEXT,retries INTEGER NOT NULL DEFAULT 0,mismatches INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,session INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(artifact,device));
        CREATE INDEX IF NOT EXISTS outbox_peer ON outbox_deliveries(device,state,retry_at);
        CREATE TRIGGER IF NOT EXISTS outbox_revoke BEFORE DELETE ON peers BEGIN UPDATE outbox_deliveries SET state='cancelled',error='Pairing changed' WHERE device=OLD.id AND state NOT IN ('sent','cancelled'); END;
        UPDATE outbox_deliveries SET state='queued' WHERE state='sending';
        CREATE TABLE IF NOT EXISTS outbox_capabilities(device TEXT PRIMARY KEY REFERENCES peers(id) ON DELETE CASCADE,generation INTEGER NOT NULL,files INTEGER NOT NULL);
        PRAGMA user_version=3;")?;
    let has_finished: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('outbox_deliveries') WHERE name='finished_at')", [], |r| r.get(0))?;
    if !has_finished {
        tx.execute_batch("ALTER TABLE outbox_deliveries ADD COLUMN finished_at INTEGER")?;
    }
    tx.execute_batch("CREATE TABLE IF NOT EXISTS outbox_dismissed(operation TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS outbox_artifact_dismissed(artifact TEXT PRIMARY KEY REFERENCES outbox_artifacts(id) ON DELETE CASCADE);
        CREATE TABLE IF NOT EXISTS outbox_requests(request_id TEXT PRIMARY KEY,operation TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS outbox_request_operation ON outbox_requests(operation);
        CREATE TABLE IF NOT EXISTS outbox_operations(id TEXT PRIMARY KEY,request_id TEXT UNIQUE,state TEXT NOT NULL,paths TEXT NOT NULL,recipients TEXT NOT NULL,accepted TEXT NOT NULL DEFAULT '[]',rejected TEXT NOT NULL DEFAULT '[]',dismissed INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL DEFAULT 0,finished_at INTEGER);")?;
    let has_stamp: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('outbox_operations') WHERE name='stamp')",
        [],
        |r| r.get(0),
    )?;
    if !has_stamp {
        tx.execute_batch("ALTER TABLE outbox_operations ADD COLUMN stamp TEXT")?;
    }
    let has_path: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('outbox_artifacts') WHERE name='path')",
        [],
        |r| r.get(0),
    )?;
    if !has_path {
        tx.execute_batch("ALTER TABLE outbox_artifacts ADD COLUMN path TEXT")?;
    }
    tx.execute_batch("INSERT OR IGNORE INTO outbox_requests SELECT request_id,id FROM outbox_operations WHERE request_id IS NOT NULL;INSERT OR IGNORE INTO outbox_dismissed SELECT id FROM outbox_operations WHERE dismissed=1;")?;
    // Interrupted staging is explicit and queryable; accepted artifacts remain
    // queued. Never reopen mutable sources automatically after a daemon restart.
    tx.execute_batch("UPDATE outbox_operations SET state=CASE WHEN json_array_length(accepted)>0 AND NOT EXISTS(SELECT 1 FROM json_each(outbox_operations.paths) p WHERE NOT EXISTS(SELECT 1 FROM json_each(outbox_operations.accepted) a WHERE json_extract(a.value,'$.path')=p.value) AND NOT EXISTS(SELECT 1 FROM json_each(outbox_operations.rejected) r WHERE json_extract(r.value,'$.path')=p.value)) THEN 'admitted' ELSE 'failed' END,finished_at=CAST((julianday('now')-2440587.5)*86400000 AS INTEGER),rejected=(SELECT json_group_array(json(value)) FROM (SELECT value FROM json_each(outbox_operations.rejected) UNION ALL SELECT json_object('path',p.value,'reason','Admission interrupted; choose files again') AS value FROM json_each(outbox_operations.paths) p WHERE NOT EXISTS(SELECT 1 FROM json_each(outbox_operations.accepted) a WHERE json_extract(a.value,'$.path')=p.value) AND NOT EXISTS(SELECT 1 FROM json_each(outbox_operations.rejected) r WHERE json_extract(r.value,'$.path')=p.value))) WHERE state='staging';")?;
    // Timestamp terminal transitions in the same transaction as their state.
    tx.execute_batch("CREATE TRIGGER IF NOT EXISTS outbox_finished AFTER UPDATE OF state ON outbox_deliveries WHEN NEW.state != OLD.state BEGIN UPDATE outbox_deliveries SET finished_at=CASE WHEN NEW.state IN ('sent','failed','cancelled') THEN CAST((julianday('now')-2440587.5)*86400000 AS INTEGER) ELSE NULL END WHERE artifact=NEW.artifact AND device=NEW.device; END;")?;
    // Recover capacity hidden by older Remove implementations too. Startup
    // collection below will release the now-terminal staged payloads.
    tx.execute_batch("UPDATE outbox_deliveries SET state='cancelled',error=NULL WHERE state NOT IN ('sent','cancelled') AND artifact IN (SELECT a.id FROM outbox_artifacts a WHERE EXISTS(SELECT 1 FROM outbox_dismissed h WHERE h.operation=a.operation) OR EXISTS(SELECT 1 FROM outbox_artifact_dismissed h WHERE h.artifact=a.id));")?;
    Ok(())
}
impl Outbox {
    pub(crate) fn open(store: Store, path: &Path) -> Result<Self> {
        let outbox = Self {
            store,
            folder: Folder::open_private(path)?,
            reserve: DISK_RESERVE,
            admission: Arc::default(),
            io: Arc::default(),
            view: Arc::default(),
            capabilities: Arc::default(),
            recovery: Arc::default(),
            operations: Arc::default(),
            cancelled_admissions: Arc::default(),
            #[cfg(test)]
            source_hook: Arc::default(),
            #[cfg(test)]
            copy_hook: Arc::default(),
            #[cfg(test)]
            reader_barrier: Arc::default(),
            #[cfg(test)]
            projection_barrier: Arc::default(),
            #[cfg(test)]
            claim_barrier: Arc::default(),
        };
        outbox.store.locked(|c| {
            let capabilities = {
                let mut q = c.prepare("SELECT device,generation,files FROM outbox_capabilities")?;
                q.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                })?
                .map(|r| {
                    let (id, generation, files) = r?;
                    Ok((Id::new(id)?, (generation as u64, files)))
                })
                .collect::<Result<_>>()?
            };
            *outbox.capabilities.lock().expect("file capabilities") = capabilities;
            Ok(())
        })?;
        outbox.refresh_operations()?;
        outbox.refresh()?;
        outbox.collect()?;
        // Only our random ID entries are swept; never follow paths or links.
        let retained = outbox.store.locked(|c| {
            let mut q = c.prepare("SELECT id FROM outbox_artifacts WHERE staged=1")?;
            Ok(q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<HashSet<_>>>()?)
        })?;
        for entry in platform_fs::entries(&outbox.folder.file)? {
            let name = entry?;
            if name
                .to_str()
                .is_some_and(|s| Id::new(s.to_owned()).is_ok() && !retained.contains(s))
            {
                let file = platform_fs::open_read_at(&outbox.folder.file, name.as_os_str())?;
                crate::platform::private::check(&file)?;
                if file.metadata()?.is_file() {
                    crate::storage::remove_file(&outbox.folder.file, name.as_os_str(), &file)?;
                }
            }
        }
        Ok(outbox)
    }
    pub(crate) fn supports(&self, device: &Id, generation: u64) -> Option<bool> {
        self.capabilities
            .lock()
            .expect("file capabilities")
            .get(device)
            .filter(|(g, _)| *g == generation)
            .map(|(_, files)| *files)
    }
    pub(crate) fn note(&self, device: Id, generation: u64, files: bool) {
        self.capabilities
            .lock()
            .expect("file capabilities")
            .insert(device, (generation, files));
    }
    pub(crate) fn has_work(&self, device: &Id) -> bool {
        self.view
            .lock()
            .expect("outbox view")
            .iter()
            .any(|r| &r.device == device && matches!(r.state.as_str(), "queued" | "sending"))
    }
    fn refresh(&self) -> Result<()> {
        self.store.locked(|c| {
            // Keep every recipient and artifact of each retained operation.
            let mut q=c.prepare("SELECT a.operation,a.meta,d.device,d.generation,d.state,d.error,d.finished_at,a.path FROM outbox_artifacts a JOIN outbox_deliveries d ON d.artifact=a.id WHERE NOT EXISTS(SELECT 1 FROM outbox_dismissed o WHERE o.operation=a.operation) AND NOT EXISTS(SELECT 1 FROM outbox_artifact_dismissed h WHERE h.artifact=a.id) AND (a.operation IN (SELECT operation FROM outbox_artifacts GROUP BY operation ORDER BY max(rowid) DESC LIMIT 50) OR a.operation IN (SELECT a.operation FROM outbox_artifacts a JOIN outbox_deliveries d ON d.artifact=a.id WHERE d.state NOT IN ('sent','cancelled'))) ORDER BY a.rowid,d.device")?;
            let view=q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,Option<i64>>(6)?,r.get::<_,Option<String>>(7)?)))?.map(|r| {
                let (operation,meta,device,generation,state,error,finished_at,path)=r?;let begin:FileBegin=serde_json::from_str(&meta)?;
                Ok(Transfer{operation:Id::new(operation)?,transfer_id:begin.transfer_id,device:Id::new(device)?,file_name:begin.file_name,size:begin.size,sent:if state=="sent" {begin.size} else {0},state,error,generation:generation as u64,direction:outgoing_direction(),finished_at:finished_at.map(|t| t as u64),path:path.map(PathBuf::from)})
            }).collect::<Result<Vec<_>>>()?;
        let mut old = self.view.lock().expect("outbox view");
        let mut view = view;
        for row in &mut view {
            if row.state == "sending"
                && let Some(previous) = old
                    .iter()
                    .find(|p| p.device == row.device && p.transfer_id == row.transfer_id)
            {
                row.sent = previous.sent;
            }
        }
        *old = view;
        Ok(())
        })
    }
    pub(crate) fn snapshot(&self, connected: &[Id]) -> Vec<Transfer> {
        let mut rows = self.view.lock().expect("outbox view").clone();
        for operation in self.operations.lock().expect("outbox operations").iter() {
            for rejection in &operation.rejected {
                let recipient: Option<Id> = rejection
                    .get("device")
                    .and_then(|d| serde_json::from_value(d.clone()).ok());
                // A target rejection applies to the operation. Its per-path
                // detail stays queryable without multiplying snapshot rows.
                let path = if recipient.is_some() {
                    None
                } else {
                    rejection
                        .get("path")
                        .and_then(|p| serde_json::from_value::<PathBuf>(p.clone()).ok())
                };
                let device = recipient.unwrap_or_else(|| Id::from_bytes([0; 16]));
                if rows.iter().any(|r| {
                    r.operation == operation.operation
                        && r.device == device
                        && r.generation == 0
                        && r.path == path
                }) {
                    continue;
                }
                rows.push(Transfer {
                    operation: operation.operation.clone(),
                    transfer_id: operation.operation.clone(),
                    device,
                    file_name: path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| batch_name(&operation.paths)),
                    size: 0,
                    sent: 0,
                    state: "failed".into(),
                    error: rejection
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .map(str::to_owned),
                    direction: outgoing_direction(),
                    finished_at: Some(operation.finished_at.unwrap_or(operation.created_at)),
                    path,

                    generation: 0,
                });
            }
            if operation.state == "staging" {
                rows.push(Transfer {
                    operation: operation.operation.clone(),
                    transfer_id: operation.operation.clone(),
                    device: Id::from_bytes([0; 16]),
                    file_name: batch_name(&operation.paths),
                    size: 0,
                    sent: 0,
                    state: "staging".into(),
                    error: None,
                    direction: outgoing_direction(),
                    finished_at: None,
                    path: None,

                    generation: 0,
                });
            }
        }
        rows = recent_operations(rows);
        rows.into_iter()
            .map(|mut row| {
                if row.generation != 0
                    && !matches!(row.state.as_str(), "sent" | "cancelled")
                    && self.store.pairing_generation(&row.device) != Some(row.generation)
                {
                    row.state = "cancelled".into();
                    row.error = Some("Pairing changed".into());
                } else if row.state == "queued" && !connected.contains(&row.device) {
                    row.state = "waiting_for_device".into();
                }
                row
            })
            .collect()
    }
    pub(crate) fn current(&self, delivery: &Delivery) -> bool {
        self.store.pairing_generation(&delivery.authority.device)
            == Some(delivery.authority.generation)
            && self.view.lock().expect("outbox view").iter().any(|r| {
                r.device == delivery.authority.device
                    && r.transfer_id == delivery.begin.transfer_id
                    && r.state == "sending"
            })
    }
    pub(crate) fn progress(&self, delivery: &Delivery, sent: u64) {
        if let Some(row) = self.view.lock().expect("outbox view").iter_mut().find(|r| {
            r.device == delivery.authority.device
                && r.transfer_id == delivery.begin.transfer_id
                && r.state == "sending"
        }) {
            row.sent = sent;
        }
    }
    fn capacity(c: &Connection, size: u64, recipients: &[Authority]) -> Result<()> {
        let (bytes,count):(i64,i64)=c.query_row("SELECT coalesce(sum(json_extract(meta,'$.size')),0),(SELECT count(*) FROM outbox_deliveries WHERE state NOT IN ('sent','cancelled')) FROM outbox_artifacts WHERE staged=1",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            (bytes as u64).saturating_add(size) <= GLOBAL_BYTES
                && count + recipients.len() as i64 <= GLOBAL_DELIVERIES as i64,
            "outbox is full"
        );
        Ok(())
    }
    fn peer_has_capacity(c: &Connection, size: u64, peer: &Authority) -> Result<bool> {
        let (bytes, count): (i64, i64) = c.query_row(
            "SELECT coalesce(sum(json_extract(a.meta,'$.size')),0),count(*) FROM outbox_deliveries d JOIN outbox_artifacts a ON a.id=d.artifact WHERE device=? AND state NOT IN ('sent','cancelled')",
            [peer.device.as_str()], |r| Ok((r.get(0)?,r.get(1)?)))?;
        Ok((bytes as u64).saturating_add(size) <= PEER_BYTES && count < PEER_DELIVERIES as i64)
    }
    fn select_recipients(
        &self,
        c: &Connection,
        operation: &Id,
        size: u64,
        recipients: &mut Vec<Authority>,
        rejected: &mut Vec<RecipientRejection>,
    ) -> Result<()> {
        let mut kept = Vec::new();
        for peer in recipients.iter() {
            if !self.operation_allows(c, operation, peer)? {
                continue;
            }
            let reason = if !self.authorized(c, peer)? {
                Some("Pairing changed")
            } else if !Self::peer_has_capacity(c, size, peer)? {
                Some("device outbox is full")
            } else {
                None
            };
            if let Some(reason) = reason {
                rejected.push(RecipientRejection {
                    device: peer.device.clone(),
                    reason: reason.into(),
                });
            } else {
                kept.push(peer.clone());
            }
        }
        *recipients = kept;
        Ok(())
    }
    /// Separate bounded I/O capacity, still tracked by Store::drain even if
    /// the awaiting caller is cancelled after submission.
    pub(crate) async fn file_io<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let reservation = self.io.reserve().await;
        let tracked = crate::work::track(vec![self.store.work.clone()]);
        reservation
            .submit(move || {
                let _tracked = tracked;
                f()
            })
            .await?
    }
    #[cfg(test)]
    pub(crate) async fn admit(
        &self,
        operation: Id,
        path: PathBuf,
        recipients: Vec<Authority>,
        limit: u64,
    ) -> Result<Id> {
        self.admit_selected(operation, path, recipients, limit, Id::from_bytes([99; 16]))
            .await
            .map(|(id, _)| id)
    }
    pub(crate) async fn admit_selected(
        &self,
        operation: Id,
        path: PathBuf,
        recipients: Vec<Authority>,
        limit: u64,
        source_device: Id,
    ) -> Result<(Id, Vec<RecipientRejection>)> {
        // Serialize before reserving any worker; move ownership into the job
        // so cancellation cannot start a competing copy.
        let admission = self.admission.clone().lock_owned().await;
        let this = self.clone();
        self.file_io(move || {
            let _admission = admission;
            this.collect()?;
            let mut recipients=recipients;
            let mut rejected=Vec::new();
            ensure!(!this.admission_cancelled(&operation),"operation cancelled");
            let mut source=crate::storage::open_regular_file(&path)?;
            #[cfg(test)]
            if let Some(hook)=this.source_hook.lock().unwrap().take() {hook(&path);}
            ensure!(!this.admission_cancelled(&operation),"operation cancelled");
            let size=source.metadata()?.len();
            ensure!(size<=limit.min(PEER_BYTES),"file exceeds send size limit");
            this.store.locked(|c|this.select_recipients(c,&operation,size,&mut recipients,&mut rejected))?;
            if recipients.is_empty() {return Err(AdmissionFailure {reason:"No eligible devices".into(),rejected,recipient_only:true}.into());}
            if let Err(error)=this.store.locked(|c|Self::capacity(c,size,&recipients)) {return Err(AdmissionFailure {reason:error.to_string(),rejected,recipient_only:false}.into());}
            ensure!(this.folder.space()?.saturating_sub(size)>=this.reserve,"outbox disk reserve");
            // Stamp the originating image before its slow copy, without
            // changing the sender's applied clipboard register. Persist the
            // same stamp in the immutable artifact for every recipient/replay.
            let stamp = if mime(&path).starts_with("image/") {
                Some(this.store.locked(|c| {
                    let tx = c.transaction()?;
                    let persisted: Option<String> = tx.query_row("SELECT stamp FROM outbox_operations WHERE id=?", [operation.as_str()], |r| r.get(0)).optional()?.flatten();
                    let mut stamps = persisted.map(|stamp| serde_json::from_str::<HashMap<PathBuf, ArtifactStamp>>(&stamp)).transpose()?.unwrap_or_default();
                    let stamp = match stamps.get(&path) {
                        Some(stamp) => stamp.clone(),
                        None => {
                            let (created_at, hlc) = allocate_stamp(&tx)?;
                            let stamp = ArtifactStamp { created_at, hlc, source_device: source_device.clone() };
                            stamps.insert(path.clone(), stamp.clone());
                            tx.execute("UPDATE outbox_operations SET stamp=? WHERE id=?", params![serde_json::to_string(&stamps)?, operation.as_str()])?;
                            stamp
                        },
                    };
                    ensure!(stamp.source_device == source_device, "image origin changed");
                    tx.commit()?;
                    Ok(stamp)
                })?)
            } else { None };
            let id=Id::new(crate::random_hex(16))?;
            let mut staged=platform_fs::create_private_at(&this.folder.file, id.as_str(), platform_fs::Access::ReadWrite)?;
            crate::platform::private::created(&staged)?;
            let result=(|| {
                #[cfg(test)]
                if let Some(hook)=this.copy_hook.lock().unwrap().take() {hook(&path);}
                let mut hash=Sha256::new();let mut copied=0u64;let mut buffer=vec![0; pneu_proto::CHUNK_SIZE];
                loop {ensure!(!this.admission_cancelled(&operation),"operation cancelled");let n=source.read(&mut buffer)?;if n==0 {break;} copied+=n as u64;ensure!(copied<=size,"source grew while staging");ensure!(this.folder.space()?.saturating_sub(n as u64)>=this.reserve,"outbox disk reserve");staged.write_all(&buffer[..n])?;hash.update(&buffer[..n]);}
                ensure!(copied==size,"source changed while staging");
                platform_fs::set_read_only(&staged)?;staged.sync_all()?;this.folder.file.sync_all()?;
                let begin=FileBegin{
            sensitive: false,transfer_id:id.clone(),file_name:path.file_name().and_then(|s|s.to_str()).context("file name must be UTF-8")?.to_owned(),mime:mime(&path).into(),size,sha256:Hash::new(hex::encode(hash.finalize()))?,kind:FileKind::File,created_at:stamp.as_ref().map_or_else(crate::now_ms, |s| s.created_at),source_media_id:None,hlc:stamp.as_ref().map(|s| s.hlc),source_device:stamp.map(|s| s.source_device)};
                pneu_proto::encode_frame(&Message::FileBegin(begin.clone()))?;
                this.store.locked(|c| {let tx=c.transaction()?;
                    this.select_recipients(&tx,&operation,size,&mut recipients,&mut rejected)?;
                    ensure!(!recipients.is_empty(),"No eligible devices");
                    let cancelled:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM outbox_operations WHERE id=? AND state<>'staging')",[operation.as_str()],|r|r.get(0))?;
                    ensure!(!cancelled,"operation cancelled");
                    Self::capacity(&tx,size,&recipients)?;
                    tx.execute("INSERT INTO outbox_artifacts(id,operation,meta,path) VALUES(?,?,?,?)",params![id.as_str(),operation.as_str(),serde_json::to_string(&begin)?,path.to_str().context("path must be UTF-8")?])?;
                    for peer in &recipients {tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,?,?)",params![id.as_str(),peer.device.as_str(),peer.generation as i64,peer.fingerprint.as_str()])?;}
                    tx.execute("UPDATE outbox_operations SET accepted=json_insert(accepted,'$[#]',json_object('path',?,'transfer_id',?)) WHERE id=?",params![path.to_str(),id.as_str(),operation.as_str()])?;
                    for rejection in &rejected {
                        tx.execute("UPDATE outbox_operations SET rejected=json_insert(rejected,'$[#]',json_object('path',?,'device',?,'reason',?)) WHERE id=?",params![path.to_str(),rejection.device.as_str(),rejection.reason,operation.as_str()])?;
                    }
                    tx.commit()?;
                    // Once committed, admission must not turn into a rejection
                    // because a subsequent projection read failed.
                    let mut view=this.view.lock().expect("outbox view");
                    for peer in &recipients {view.push(Transfer{operation:operation.clone(),transfer_id:id.clone(),device:peer.device.clone(),file_name:begin.file_name.clone(),size,sent:0,state:"queued".into(),error:None,generation:peer.generation,direction:outgoing_direction(),finished_at:None,path:Some(path.clone())});}
                    Ok(())
                })
            })();
            if result.is_err() {let _=crate::storage::remove_file(&this.folder.file,id.as_str(),&staged);}
            if let Err(error)=result {return Err(AdmissionFailure {recipient_only:recipients.is_empty(),reason:error.to_string(),rejected}.into());}Ok((id,rejected))
        }).await
    }
    #[cfg(test)]
    pub(crate) async fn claim(
        &self,
        device: Id,
        generation: u64,
        session: u64,
    ) -> Result<Option<Delivery>> {
        self.claim_while(device, generation, session, || true).await
    }
    pub(crate) async fn claim_while(
        &self,
        device: Id,
        generation: u64,
        session: u64,
        current: impl FnOnce() -> bool + Send + 'static,
    ) -> Result<Option<Delivery>> {
        let this = self.clone();
        self.store.blocking(move |_| {
            let delivery=this.store.locked(|c| {
                // Recheck after acquiring the DB lock: a replaced session may
                // have waited behind the replacement's reconciliation.
                if !current() {return Ok(None);}
                let tx=c.transaction()?;
                tx.execute("UPDATE outbox_deliveries SET state='queued',error='Connection interrupted',retry_at=0 WHERE device=? AND generation=? AND session<>? AND state='sending'",params![device.as_str(),generation as i64,session as i64])?;
                let row=tx.query_row("SELECT a.meta,d.fingerprint FROM outbox_deliveries d JOIN outbox_artifacts a ON a.id=d.artifact JOIN peers p ON p.id=d.device AND p.generation=d.generation AND p.fp=d.fingerprint WHERE d.device=? AND d.generation=? AND d.state='queued' AND d.retry_at<=? ORDER BY a.rowid LIMIT 1",params![device.as_str(),generation as i64,crate::now_ms() as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
                let Some((meta,fp))=row else {tx.commit()?;return Ok(None)};let begin:FileBegin=serde_json::from_str(&meta)?;let fingerprint=Hash::new(fp)?;
                tx.execute("UPDATE outbox_deliveries SET state='sending',retries=retries+1,error=NULL,session=? WHERE artifact=? AND device=?",params![session as i64,begin.transfer_id.as_str(),device.as_str()])?;tx.commit()?;
                #[cfg(test)]
                if let Some((entered,release))=this.claim_barrier.lock().unwrap().take() {let _=entered.send(());let _=release.recv_timeout(std::time::Duration::from_secs(10));}
                if let Some(row)=this.view.lock().expect("outbox view").iter_mut().find(|r|r.device==device && r.transfer_id==begin.transfer_id) {row.state="sending".into();row.sent=0;row.error=None;}
                Ok(Some(Delivery {begin,session,authority:Authority {device,generation,fingerprint}}))
            })?;
            // Projection reads cannot discard a claim that already committed.
            let _=this.refresh();Ok(delivery)
        }).await
    }
    pub(crate) async fn reader(&self, delivery: Delivery) -> Result<tokio::fs::File> {
        let this = self.clone();
        self.file_io(move || {
            ensure!(this.current(&delivery), "file delivery cancelled");
            let mut file =
                platform_fs::open_read_at(&this.folder.file, delivery.begin.transfer_id.as_str())?;
            crate::platform::private::check(&file)?;
            ensure!(
                file.metadata()?.is_file() && file.metadata()?.len() == delivery.begin.size,
                "staged file changed"
            );
            #[cfg(test)]
            if let Some((entered, release)) = this.reader_barrier.lock().unwrap().take() {
                let _ = entered.send(());
                let _ = release.recv_timeout(std::time::Duration::from_secs(10));
            }
            // Every replay rechecks exact staged bytes, including mismatch NACKs.
            let mut hash = Sha256::new();
            let mut buffer = vec![0; pneu_proto::CHUNK_SIZE];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            ensure!(
                hex::encode(hash.finalize()) == delivery.begin.sha256.as_str(),
                "staged file hash mismatch"
            );
            std::io::Seek::rewind(&mut file)?;
            Ok(tokio::fs::File::from_std(file))
        })
        .await
    }
    pub(crate) async fn receipt(&self, delivery: Delivery, code: Option<NackCode>) -> Result<()> {
        let this = self.clone();
        self.store.blocking(move |_| {
            this.store.locked(|c| {
                let mismatch=matches!(code,Some(NackCode::HashMismatch|NackCode::SizeMismatch));
                let (attempts,mismatches):(u32,u32)=c.query_row("SELECT retries,mismatches FROM outbox_deliveries WHERE artifact=? AND device=?",params![delivery.begin.transfer_id.as_str(),delivery.authority.device.as_str()],|r|Ok((r.get(0)?,r.get(1)?)))?;
                let mismatches=mismatches+u32::from(mismatch);
                let state=match code {None=>"sent",Some(code) if code.retryable() && (!mismatch || mismatches<=3)=>"queued",_=>"failed"};
                let delay=if mismatch { (2u64.pow(attempts.min(5))).min(60)*1000 } else {60_000};
                let jitter=rand::random_range(80..=120);
                c.execute("UPDATE outbox_deliveries SET state=?,error=?,mismatches=?,retry_at=? WHERE artifact=? AND device=? AND generation=? AND session=? AND state='sending'",params![state,code.map(|c|format!("{c:?}")),mismatches,(crate::now_ms()+delay*jitter/100) as i64,delivery.begin.transfer_id.as_str(),delivery.authority.device.as_str(),delivery.authority.generation as i64,delivery.session as i64])?;Ok(())
            })?;this.refresh()?;this.collect()
        }).await
    }
    pub(crate) async fn fail(&self, delivery: Delivery, error: String) -> Result<()> {
        let this = self.clone();
        self.store.blocking(move |_| {
            this.store.locked(|c|{c.execute("UPDATE outbox_deliveries SET state='failed',error=? WHERE artifact=? AND device=? AND generation=? AND session=? AND state='sending'",params![error,delivery.begin.transfer_id.as_str(),delivery.authority.device.as_str(),delivery.authority.generation as i64,delivery.session as i64])?;Ok(())})?;this.refresh()
        }).await
    }
    pub(crate) async fn recover(
        &self,
        device: Id,
        generation: u64,
        session: u64,
        error: Option<String>,
    ) -> Result<()> {
        self.defer_recovery(device.clone(), generation, session, error.clone());
        let this = self.clone();
        self.store.blocking(move |_| {
            this.store.locked(|c| {c.execute("UPDATE outbox_deliveries SET state='queued',error=?,retry_at=?+min(60000,2000*(1 << min(max(retries-1,0),5))) WHERE device=? AND generation=? AND session=? AND state='sending'",params![error,crate::now_ms() as i64,device.as_str(),generation as i64,session as i64])?;Ok(())})?;this.refresh()?;this.recovery.lock().expect("outbox recovery").remove(&(device,generation,session));Ok(())
        }).await
    }
    pub(crate) async fn change(
        &self,
        operation: Id,
        artifact: Option<Id>,
        device: Option<Id>,
        retry: bool,
    ) -> Result<()> {
        let this = self.clone();
        self.store.blocking(move |_| {
            this.store.locked(|c| {
                let tx=c.transaction()?;
                this.validate_artifact(&tx, &operation, artifact.as_ref())?;
                let staging=if !retry && artifact.is_none() {
                    if let Some(device)=&device {
                        tx.execute("UPDATE outbox_operations SET recipients=(SELECT json_group_array(json(value)) FROM json_each(outbox_operations.recipients) WHERE json_extract(value,'$.device')<>?) WHERE id=? AND state='staging'",params![device.as_str(),operation.as_str()])?
                    } else {tx.execute("UPDATE outbox_operations SET state='cancelled' WHERE id=? AND state='staging'",[operation.as_str()])?}
                } else {0};
                // Removing the final staging recipient is a deliberate batch
                // cancellation, not a path admission failure.
                if staging > 0 { tx.execute("UPDATE outbox_operations SET state='cancelled' WHERE id=? AND state='staging' AND json_array_length(recipients)=0",[operation.as_str()])?; }
                if retry {tx.execute("DELETE FROM outbox_artifact_dismissed WHERE artifact IN (SELECT id FROM outbox_artifacts WHERE operation=? AND (? IS NULL OR id=?))",params![operation.as_str(),artifact.as_ref().map(Id::as_str),artifact.as_ref().map(Id::as_str)])?;tx.execute("DELETE FROM outbox_dismissed WHERE operation=?",[operation.as_str()])?;tx.execute("UPDATE outbox_operations SET dismissed=0 WHERE id=?",[operation.as_str()])?;}
                let c=&tx;
                let changed=if retry {c.execute("UPDATE outbox_deliveries SET state='queued',error=NULL,mismatches=0,retry_at=0 WHERE artifact IN (SELECT id FROM outbox_artifacts WHERE operation=? AND staged=1 AND (? IS NULL OR id=?)) AND device=? AND state IN ('failed','queued') AND generation=(SELECT generation FROM peers WHERE id=device) AND fingerprint=(SELECT fp FROM peers WHERE id=device)",params![operation.as_str(),artifact.as_ref().map(Id::as_str),artifact.as_ref().map(Id::as_str),device.as_ref().context("retry needs a device")?.as_str()])?}
                else {c.execute("UPDATE outbox_deliveries SET state='cancelled',error=NULL WHERE artifact IN (SELECT id FROM outbox_artifacts WHERE operation=? AND (? IS NULL OR id=?)) AND (? IS NULL OR device=?) AND state NOT IN ('sent','cancelled')",params![operation.as_str(),artifact.as_ref().map(Id::as_str),artifact.as_ref().map(Id::as_str),device.as_ref().map(Id::as_str),device.as_ref().map(Id::as_str)])?};
                ensure!(changed+staging>0,"no matching transfer can be changed");
                let cancelled = staging > 0 && tx.query_row("SELECT state='cancelled' FROM outbox_operations WHERE id=?",[operation.as_str()],|r|r.get::<_,bool>(0))?;
                tx.commit()?;
                if cancelled { this.cancelled_admissions.lock().expect("cancelled admissions").insert(operation); }
                Ok(())
            })?;this.refresh_operations()?;this.refresh()?;this.collect()
        }).await
    }
    pub(crate) fn defer_recovery(
        &self,
        device: Id,
        generation: u64,
        session: u64,
        error: Option<String>,
    ) {
        self.recovery
            .lock()
            .expect("outbox recovery")
            .insert((device, generation, session), error);
    }
    pub(crate) async fn retry_recovery(&self) -> Result<()> {
        let pending = self.recovery.lock().expect("outbox recovery").clone();
        for ((device, generation, session), error) in pending {
            self.recover(device, generation, session, error).await?;
        }
        Ok(())
    }
    pub(crate) async fn reconcile(&self) -> Result<()> {
        #[cfg(test)]
        self.store.fault("outbox_reconcile")?;
        let this = self.clone();
        self.store
            .blocking(move |_| {
                this.refresh()?;
                this.collect()
            })
            .await
    }
    fn collect(&self) -> Result<()> {
        let ids=self.store.locked(|c|{let mut q=c.prepare("SELECT id FROM outbox_artifacts a WHERE staged=1 AND NOT EXISTS(SELECT 1 FROM outbox_deliveries d WHERE d.artifact=a.id AND state NOT IN ('sent','cancelled'))")?;Ok(q.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)})?;
        for id in ids {
            match platform_fs::open_read_at(&self.folder.file, id.as_str()) {
                Ok(fd) => {
                    let file = fd;
                    crate::platform::private::check(&file)?;
                    crate::storage::remove_file(&self.folder.file, id.as_str(), &file)?
                }
                Err(error) if platform_fs::is_missing(&error) => {}
                Err(e) => return Err(e),
            }
            self.folder.file.sync_all()?;
            self.store.locked(|c| {
                c.execute("UPDATE outbox_artifacts SET staged=0 WHERE id=?", [id])?;
                Ok(())
            })?;
        }
        self.store.locked(|c|{c.execute("DELETE FROM outbox_artifacts WHERE operation NOT IN (SELECT operation FROM outbox_artifacts WHERE staged=1) AND operation NOT IN (SELECT operation FROM outbox_artifacts GROUP BY operation ORDER BY max(rowid) DESC LIMIT 50)",[])?;Ok(())})
    }
}
fn batch_name(paths: &[PathBuf]) -> String {
    if let [path] = paths {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        format!("{} files", paths.len())
    }
}
/// Bound terminal presentation by whole operations. Active or retryable rows
/// are charged to the durable queue caps; never hide a sibling of those rows.
fn recent_operations(rows: Vec<Transfer>) -> Vec<Transfer> {
    struct Group {
        id: Id,
        active: bool,
        finished: u64,
        rows: Vec<Transfer>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for row in rows {
        let index = groups
            .iter()
            .position(|g| g.id == row.operation)
            .unwrap_or_else(|| {
                groups.push(Group {
                    id: row.operation.clone(),
                    active: false,
                    finished: 0,
                    rows: Vec::new(),
                });
                groups.len() - 1
            });
        let group = &mut groups[index];
        group.active |= matches!(row.state.as_str(), "staging" | "queued" | "sending")
            || (row.generation != 0 && row.state == "failed");
        group.finished = group.finished.max(row.finished_at.unwrap_or_default());
        group.rows.push(row);
    }
    groups.sort_by_key(|g| g.finished);
    let mut terminal = 0;
    let mut operations = 0;
    let mut kept = Vec::new();
    for group in groups.into_iter().rev() {
        if group.active {
            kept.push(group.rows);
            continue;
        }
        // Keep the latest operation intact; IPC pages this projection by
        // encoded bytes, including when one operation exceeds the soft row cap.
        if operations == 0 || (operations < 50 && terminal + group.rows.len() <= 128) {
            operations += 1;
            terminal += group.rows.len();
            kept.push(group.rows);
        }
    }
    kept.into_iter().rev().flatten().collect()
}
fn mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "heic" => "image/heic",
        "mp4" => "video/mp4",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }
    pub(super) async fn fixture() -> Result<(tempfile::TempDir, Outbox, Vec<Authority>, PathBuf)> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let mut peers = Vec::new();
        for n in [1, 2] {
            let fingerprint = Hash::new(format!("{n:064x}"))?;
            store.pair(id(n), fingerprint.clone()).await?;
            peers.push(Authority {
                device: id(n),
                generation: store.pairing_generation(&id(n)).unwrap(),
                fingerprint,
            });
        }
        let queue = Outbox::open(store, &dir.path().join("outbox"))?;
        let source = dir.path().join("photo.png");
        fs::write(&source, b"immutable bytes")?;
        Ok((dir, queue, peers, source))
    }
    #[tokio::test]
    async fn pending_admissions_and_replay_hashes_leave_store_capacity_available() -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let entered = Mutex::new(Some(entered));
        let blocked = Mutex::new(blocked);
        *q.copy_hook.lock().unwrap() = Some(Arc::new(move |_| {
            let _ = entered.lock().unwrap().take().unwrap().send(());
            blocked
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        }));
        let mut admissions = tokio::task::JoinSet::new();
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let mut first_operation = None;
        for _ in 0..16 {
            let (operation, _) = q
                .start_operation(vec![source.clone()], peers.clone(), Vec::new(), None)
                .await?;
            first_operation.get_or_insert_with(|| operation.clone());
            let (q, peers, source, started) =
                (q.clone(), peers.clone(), source.clone(), started.clone());
            admissions.spawn(async move {
                started.send(()).unwrap();
                q.admit(operation, source, peers, PEER_BYTES).await
            });
        }
        wait.await?;
        for _ in 0..16 {
            starts.recv().await.unwrap();
        }
        let status = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            q.store
                .call(|c| {
                    // A real persisted write uses the same shared workers as settings.
                    c.execute(
                        "UPDATE local_clock SET last_outgoing_at=last_outgoing_at+1",
                        [],
                    )?;
                    Ok(())
                })
                .await?;
            q.operation_status(first_operation.unwrap()).await
        })
        .await;
        release.send(())?;
        while let Some(result) = admissions.join_next().await {
            result??;
        }
        assert_eq!(status??["state"], "staging");

        let delivery = q.claim(id(1), peers[0].generation, 10).await?.unwrap();
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        *q.reader_barrier.lock().unwrap() = Some((entered, blocked));
        let mut hashes = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let (q, delivery) = (q.clone(), delivery.clone());
            hashes.spawn(async move { q.reader(delivery).await });
        }
        wait.await?;
        let all_pending = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while q.io.admitted() < 16 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let status = tokio::time::timeout(std::time::Duration::from_secs(1), q.store.peers()).await;
        release.send(())?;
        while let Some(result) = hashes.join_next().await {
            result??;
        }
        all_pending?;
        status??;
        Ok(())
    }

    #[tokio::test]
    async fn image_batch_freezes_ordered_stamps_before_staging_wait_and_request_replay()
    -> Result<()> {
        let (dir, q, peers, first_path) = fixture().await?;
        let second_path = dir.path().join("second.jpg");
        fs::write(&second_path, b"second image")?;
        let paths = vec![first_path.clone(), second_path.clone()];
        let (operation, new) = q
            .start_operation(
                paths.clone(),
                peers.clone(),
                Vec::new(),
                Some("image-batch".into()),
            )
            .await?;
        assert!(new);
        let before: String = q
            .store
            .call(|c| Ok(c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?))
            .await?;
        // Newer clipboard activity while this operation waits cannot restamp it.
        let (_, newer) = q.store.local_clipboard(id(99)).await?;
        assert!(newer > before.parse::<u64>()?);
        let replay = q
            .start_operation(paths, peers.clone(), Vec::new(), Some("image-batch".into()))
            .await?;
        assert_eq!(replay, (operation.clone(), false));
        let mut previous = 0;
        for path in [first_path, second_path] {
            q.admit(operation.clone(), path, peers.clone(), PEER_BYTES)
                .await?;
            let delivery = q.claim(id(1), peers[0].generation, 1).await?.unwrap();
            let stamp = delivery.begin.hlc.unwrap();
            assert!(stamp > previous && stamp < newer);
            previous = stamp;
            assert_eq!(delivery.begin.source_device, Some(id(99)));
            q.receipt(delivery, None).await?;
        }
        let seen: String = q
            .store
            .call(|c| Ok(c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?))
            .await?;
        assert_eq!(
            seen,
            newer.to_string(),
            "neither admission nor request replay allocates another stamp"
        );
        q.store
            .call(|c| {
                c.execute("UPDATE mesh_clock SET last_seen=?", [u64::MAX.to_string()])?;
                Ok(())
            })
            .await?;
        let oversized_clock = q
            .start_operation(vec![dir.path().join("next.png")], peers, Vec::new(), None)
            .await;
        assert!(
            oversized_clock
                .unwrap_err()
                .to_string()
                .contains("clock exhausted")
        );
        Ok(())
    }

    #[tokio::test]
    async fn every_supported_file_image_format_has_a_persisted_origin_stamp() -> Result<()> {
        let (dir, q, peers, _) = fixture().await?;
        for (n, extension) in ["png", "JPG", "jpeg", "gif", "webp", "BMP"]
            .into_iter()
            .enumerate()
        {
            let path = dir.path().join(format!("image.{extension}"));
            fs::write(&path, b"image bytes")?;
            q.admit(id(n as u8 + 10), path, peers.clone(), PEER_BYTES)
                .await?;
            let delivery = q.claim(id(1), peers[0].generation, 1).await?.unwrap();
            assert!(delivery.begin.mime.starts_with("image/"), "{extension}");
            assert!(delivery.begin.hlc.is_some(), "{extension}");
            assert_eq!(delivery.begin.source_device, Some(id(99)));
            q.receipt(delivery, None).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn image_artifacts_keep_their_origin_stamp_across_recipients_retries_and_restart()
    -> Result<()> {
        let (dir, q, peers, source) = fixture().await?;
        let source_device = id(99);
        let (_, applied) = q.store.local_clipboard(source_device.clone()).await?;
        q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        let first = q.claim(id(1), peers[0].generation, 1).await?.unwrap();
        let second = q.claim(id(2), peers[1].generation, 2).await?.unwrap();
        assert_eq!(first.begin, second.begin);
        assert!(first.begin.hlc.unwrap() > applied);
        assert_eq!(first.begin.source_device, Some(source_device));
        let (seen, register): (String, String) = q
            .store
            .call(|c| {
                Ok((
                    c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?,
                    c.query_row("SELECT hlc FROM clipboard_clock", [], |r| r.get(0))?,
                ))
            })
            .await?;
        assert_eq!(seen, first.begin.hlc.unwrap().to_string());
        assert_eq!(
            register,
            applied.to_string(),
            "sending a file is not a local clipboard effect"
        );
        q.receipt(first.clone(), Some(NackCode::UnsupportedKind))
            .await?;
        q.change(
            id(9),
            Some(first.begin.transfer_id.clone()),
            Some(id(1)),
            true,
        )
        .await?;
        let retried = q.claim(id(1), peers[0].generation, 3).await?.unwrap();
        assert_eq!(retried.begin, first.begin);
        q.store.drain().await;
        drop(q);
        let q = Outbox::open(
            Store::open(&dir.path().join("state.sqlite3"))?,
            &dir.path().join("outbox"),
        )?;
        let restarted = q.claim(id(1), peers[0].generation, 4).await?.unwrap();
        assert_eq!(restarted.begin, first.begin);
        let plain = dir.path().join("report.txt");
        fs::write(&plain, b"not an image")?;
        q.admit(id(10), plain, peers.clone(), PEER_BYTES).await?;
        q.receipt(restarted, None).await?;
        let plain = q.claim(id(1), peers[0].generation, 5).await?.unwrap();
        assert_eq!(plain.begin.hlc, None);
        assert_eq!(plain.begin.source_device, None);
        Ok(())
    }

    #[tokio::test]
    async fn independent_receipts_retry_cancel_and_immutable_staging() -> Result<()> {
        let (dir, q, peers, source) = fixture().await?;
        let transfer = q
            .admit(id(9), source.clone(), peers.clone(), PEER_BYTES)
            .await?;
        fs::write(source, b"different source")?;
        let a = q.claim(id(1), peers[0].generation, 10).await?.unwrap();
        let b = q.claim(id(2), peers[1].generation, 20).await?.unwrap();
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut q.reader(a.clone()).await?, &mut bytes).await?;
        assert_eq!(bytes, b"immutable bytes");
        let before = crate::now_ms().saturating_sub(1000);
        q.receipt(a, None).await?;
        assert!(
            q.snapshot(&[])[0]
                .finished_at
                .is_some_and(|t| t >= before && t <= crate::now_ms())
        );
        q.receipt(b.clone(), Some(NackCode::UnsupportedKind))
            .await?;
        assert!(dir.path().join("outbox").join(transfer.as_str()).exists());
        assert!(q.snapshot(&[])[1].finished_at.is_some());
        q.change(id(9), None, Some(id(2)), true).await?;
        assert!(q.snapshot(&[])[1].finished_at.is_none());
        let retry = q.claim(id(2), peers[1].generation, 21).await?.unwrap();
        assert_eq!(retry.begin, b.begin);
        q.change(id(9), None, Some(id(2)), false).await?;
        q.receipt(retry, None).await?;
        assert_eq!(
            q.snapshot(&[])
                .iter()
                .map(|r| r.state.as_str())
                .collect::<Vec<_>>(),
            vec!["sent", "cancelled"]
        );
        assert!(!dir.path().join("outbox").join(transfer.as_str()).exists());
        let finished = q
            .snapshot(&[])
            .iter()
            .map(|t| t.finished_at)
            .collect::<Vec<_>>();
        assert!(finished.iter().all(Option::is_some));
        q.store.drain().await;
        drop(q);
        let q = Outbox::open(
            Store::open(&dir.path().join("state.sqlite3"))?,
            &dir.path().join("outbox"),
        )?;
        assert_eq!(
            q.snapshot(&[])
                .iter()
                .map(|t| t.finished_at)
                .collect::<Vec<_>>(),
            finished
        );
        assert!(q.change(id(9), None, Some(id(2)), true).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn restart_replays_exact_id_and_binding_change_cannot_retarget() -> Result<()> {
        let (dir, q, peers, source) = fixture().await?;
        let transfer = q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        let in_flight = q.claim(id(1), peers[0].generation, 10).await?.unwrap();
        let mut reader = q.reader(in_flight.clone()).await?;
        let mut prefix = [0; 7];
        tokio::io::AsyncReadExt::read_exact(&mut reader, &mut prefix).await?;
        q.progress(&in_flight, 7);
        assert_eq!(q.snapshot(&[id(1)])[0].sent, 7);
        drop(reader);
        q.store.drain().await;
        drop(q);
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let q = Outbox::open(store, &dir.path().join("outbox"))?;
        let replay = q.claim(id(1), peers[0].generation, 11).await?.unwrap();
        assert_eq!(replay.begin.transfer_id, transfer);
        assert_eq!(q.snapshot(&[id(1)])[0].sent, 0);
        // A displaced connection must not recover the replacement's claim.
        q.recover(id(1), peers[0].generation, 10, None).await?;
        assert!(q.current(&replay));
        q.store.pair(id(2), peers[1].fingerprint.clone()).await?;
        let generation = q.store.pairing_generation(&id(2)).unwrap();
        assert!(q.claim(id(2), generation, 22).await?.is_none());
        assert_eq!(q.snapshot(&[])[1].state, "cancelled");
        assert!(q.change(id(9), None, Some(id(2)), true).await.is_err());
        q.receipt(replay, None).await?;
        assert!(!dir.path().join("outbox").join(transfer.as_str()).exists());
        Ok(())
    }
    #[tokio::test]
    async fn retry_backoff_and_mismatch_budget_are_recipient_scoped() -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        for n in 0..4 {
            let d = q.claim(id(1), peers[0].generation, 10).await?.unwrap();
            q.receipt(d, Some(NackCode::HashMismatch)).await?;
            assert!(q.claim(id(1), peers[0].generation, 10).await?.is_none());
            if n < 3 {
                q.store
                    .call(|c| {
                        c.execute("UPDATE outbox_deliveries SET retry_at=0", [])?;
                        Ok(())
                    })
                    .await?;
            }
        }
        assert_eq!(q.snapshot(&[id(1), id(2)])[0].state, "failed");
        assert_eq!(q.snapshot(&[id(1), id(2)])[1].state, "queued");
        q.change(id(9), None, Some(id(1)), true).await?;
        assert!(q.claim(id(1), peers[0].generation, 10).await?.is_some());
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn caps_reserve_symlinks_and_failed_admission_are_enforced() -> Result<()> {
        let (dir, q, peers, source) = fixture().await?;
        let mut constrained = q.clone();
        constrained.reserve = u64::MAX;
        assert!(
            constrained
                .admit(id(9), source.clone(), peers.clone(), PEER_BYTES)
                .await
                .is_err()
        );
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&source, &link)?;
        assert!(crate::storage::open_regular_file(&link).is_err());
        let parent = dir.path().join("linked-parent");
        std::os::unix::fs::symlink(dir.path(), &parent)?;
        assert!(crate::storage::open_regular_file(&parent.join("photo.png")).is_err());
        assert!(crate::storage::open_regular_file(dir.path()).is_err());
        assert!(
            q.admit(id(9), source.clone(), peers.clone(), 1)
                .await
                .is_err()
        );
        let authority = peers[0].clone();
        q.store.call(move |c| {let tx=c.transaction()?;
            let begin=FileBegin{
            sensitive: false,transfer_id:id(50),file_name:"full".into(),mime:"application/octet-stream".into(),size:PEER_BYTES,sha256:Hash::new("0".repeat(64))?,kind:FileKind::File,created_at:0,source_media_id:None,hlc:None,source_device:None};
            tx.execute("INSERT INTO outbox_artifacts(id,operation,meta,staged) VALUES(?,?,?,1)",params![id(50).as_str(),id(51).as_str(),serde_json::to_string(&begin)?])?;
            tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,?,?)",params![id(50).as_str(),authority.device.as_str(),authority.generation as i64,authority.fingerprint.as_str()])?;
            assert!(!Outbox::peer_has_capacity(&tx,1,&authority)?);
            tx.execute("UPDATE outbox_artifacts SET meta=json_set(meta,'$.size',?)",[GLOBAL_BYTES as i64])?;
            assert!(Outbox::capacity(&tx,1,std::slice::from_ref(&authority)).unwrap_err().to_string().starts_with("outbox is full"));
            tx.execute("UPDATE outbox_artifacts SET meta=json_set(meta,'$.size',0)",[])?;
            for n in 2..=GLOBAL_DELIVERIES {tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,?,?)",params![id(50).as_str(),format!("{n:032x}"),1,"0".repeat(64)])?;}
            assert!(Outbox::capacity(&tx,0,std::slice::from_ref(&authority)).unwrap_err().to_string().starts_with("outbox is full"));
            tx.execute("DELETE FROM outbox_artifacts",[])?;
            for n in 1..=PEER_DELIVERIES {
                let transfer=format!("{n:032x}");
                tx.execute("INSERT INTO outbox_artifacts(id,operation,meta,staged) VALUES(?,?,json_set(?,'$.size',0),1)",params![transfer,id(51).as_str(),serde_json::to_string(&begin)?])?;
                tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,?,?)",params![transfer,authority.device.as_str(),authority.generation as i64,authority.fingerprint.as_str()])?;
            }
            assert!(!Outbox::peer_has_capacity(&tx,0,&authority)?);Ok(())}).await?;
        q.store.call(|c|{c.execute("DELETE FROM outbox_artifacts",[])?;c.execute_batch("CREATE TRIGGER refuse_outbox BEFORE INSERT ON outbox_deliveries BEGIN SELECT RAISE(ABORT,'injected storage failure'); END;")?;Ok(())}).await?;
        assert!(q.admit(id(9), source, peers, PEER_BYTES).await.is_err());
        assert_eq!(fs::read_dir(dir.path().join("outbox"))?.count(), 0);
        assert!(q.snapshot(&[]).is_empty());
        Ok(())
    }
}

#[path = "outbox_operations.rs"]
mod operations;
pub use operations::AdmissionOperation;

#[cfg(all(test, unix))]
mod regression_tests {
    use super::tests::{fixture, id};
    use super::*;

    #[tokio::test]
    async fn growing_between_validation_and_size_read_and_during_copy_never_exceeds_limit()
    -> Result<()> {
        for phase in [0, 1, 2] {
            let (dir, q, peers, source) = fixture().await?;
            fs::write(&source, b"x")?;
            let hook: Arc<dyn Fn(&Path) + Send + Sync> = Arc::new(move |path| {
                fs::write(path, if phase == 2 { &b""[..] } else { &b"xy"[..] }).unwrap();
            });
            if phase == 0 {
                *q.source_hook.lock().unwrap() = Some(hook);
            } else {
                *q.copy_hook.lock().unwrap() = Some(hook);
            }
            let result = q.admit(id(9), source, peers, 1).await;
            assert!(result.is_err(), "phase {phase} accepted changed source");
            assert!(q.snapshot(&[]).is_empty());
            assert_eq!(fs::read_dir(dir.path().join("outbox"))?.count(), 0);
        }
        Ok(())
    }
    #[test]
    #[cfg(unix)]
    fn descriptor_relative_parent_paths_reject_links_even_before_parent_component() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        fs::create_dir(dir.path().join("child"))?;
        fs::write(dir.path().join("file"), b"safe")?;
        let mut file = crate::storage::open_regular_file(&dir.path().join("child/../file"))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"safe");
        std::os::unix::fs::symlink(dir.path().join("child"), dir.path().join("link"))?;
        assert!(crate::storage::open_regular_file(&dir.path().join("link/../file")).is_err());
        assert!(
            crate::ipc::send_command(&[dir
                .path()
                .join("child/../file")
                .to_string_lossy()
                .into_owned()])
            .is_ok()
        );
        Ok(())
    }
    #[tokio::test]
    async fn failed_recovery_is_retained_and_new_session_reconciles_claim_before_sending()
    -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        let first = q.claim(id(1), peers[0].generation, 10).await?.unwrap();
        q.store.call(|c| {c.execute_batch("CREATE TRIGGER refuse_recovery BEFORE UPDATE OF state ON outbox_deliveries WHEN OLD.state='sending' AND NEW.state='queued' BEGIN SELECT RAISE(ABORT,'recovery failure'); END;")?;Ok(())}).await?;
        assert!(
            q.recover(id(1), peers[0].generation, 10, None)
                .await
                .is_err()
        );
        assert_eq!(q.recovery.lock().unwrap().len(), 1);
        assert!(q.claim(id(1), peers[0].generation, 11).await.is_err());
        q.store
            .call(|c| {
                c.execute_batch("DROP TRIGGER refuse_recovery")?;
                Ok(())
            })
            .await?;
        let replacement = q.claim(id(1), peers[0].generation, 11).await?.unwrap();
        assert_eq!(replacement.begin, first.begin);
        q.retry_recovery().await?;
        assert!(q.current(&replacement));
        assert!(q.recovery.lock().unwrap().is_empty());
        // A stale receipt must not complete a replacement's claim.
        q.receipt(first, None).await?;
        assert_eq!(q.snapshot(&[])[0].state, "sending");
        q.receipt(replacement, None).await?;
        assert_eq!(q.snapshot(&[])[0].state, "sent");
        Ok(())
    }
    #[tokio::test]
    async fn explicit_retry_recovers_abandoned_sending_without_touching_live_sibling() -> Result<()>
    {
        let (_dir, q, peers, source) = fixture().await?;
        q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        q.claim(id(1), peers[0].generation, 10).await?.unwrap();
        let sibling = q.claim(id(2), peers[1].generation, 20).await?.unwrap();
        let generation = peers[1].generation;
        q.reconcile_sessions(move || vec![(id(2), generation, 20)])
            .await?;
        q.change(id(9), None, Some(id(1)), true).await?;
        assert!(q.current(&sibling));
        let retry = q.claim(id(1), peers[0].generation, 11).await?.unwrap();
        q.receipt(retry, None).await?;
        q.change(id(9), None, Some(id(2)), false).await?;
        assert_eq!(q.snapshot(&[])[0].state, "sent");
        assert_eq!(q.snapshot(&[])[1].state, "cancelled");
        Ok(())
    }
    #[tokio::test]
    async fn history_pruning_retains_whole_operations_across_recipient_boundary() -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        // 65 two-recipient terminal operations previously kept half of the
        // oldest operation at the independent 128-row projection boundary.
        let metadata = FileBegin {
            sensitive: false,
            transfer_id: id(100),
            file_name: "history".into(),
            mime: "text/plain".into(),
            size: 0,
            sha256: Hash::new("0".repeat(64))?,
            kind: FileKind::File,
            created_at: 0,
            source_media_id: None,
            hlc: None,
            source_device: None,
        };
        q.store.call(move |c| {let tx=c.transaction()?;for n in 1..=65u8 {let op=format!("{n:032x}");for artifact in 0..2u8 {let mut meta=metadata.clone();meta.transfer_id=Id::from_bytes([n+artifact*70;16]);tx.execute("INSERT INTO outbox_artifacts(id,operation,meta,staged) VALUES(?,?,?,0)",params![meta.transfer_id.as_str(),op,serde_json::to_string(&meta)?])?;for peer in &peers {tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint,state) VALUES(?,?,?,?,'sent')",params![meta.transfer_id.as_str(),peer.device.as_str(),peer.generation as i64,peer.fingerprint.as_str()])?;}}}tx.commit()?;Ok(())}).await?;
        q.reconcile().await?;
        let rows = q.snapshot(&[]);
        assert_eq!(rows.len(), 128);
        let mut counts = HashMap::new();
        for row in rows {
            *counts.entry(row.operation).or_insert(0) += 1;
        }
        assert_eq!(counts.len(), 32);
        assert!(counts.values().all(|n| *n == 4));
        fs::remove_file(source)?;
        Ok(())
    }
}

#[cfg(test)]
mod admission_regression_tests {
    use super::tests::{fixture, id};
    use super::*;
    fn fill_recipient_capacity(store: &Store, peer: &Authority, by_bytes: bool) -> Result<()> {
        store.locked(|c| {
            let tx=c.transaction()?;
            let count=if by_bytes {1} else {PEER_DELIVERIES};
            for n in 0..count {
                let artifact=Id::new(format!("{:032x}",1000+n))?;
                let begin=FileBegin {
            sensitive: false,transfer_id:artifact.clone(),file_name:"existing.bin".into(),mime:"application/octet-stream".into(),size:if by_bytes {PEER_BYTES} else {0},sha256:Hash::new("ab".repeat(32))?,kind:FileKind::File,created_at:0,source_media_id:None,hlc:None,source_device:None};
                tx.execute("INSERT INTO outbox_artifacts(id,operation,meta) VALUES(?,?,?)",params![artifact.as_str(),id(80).as_str(),serde_json::to_string(&begin)?])?;
                tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,?,?)",params![artifact.as_str(),peer.device.as_str(),peer.generation as i64,peer.fingerprint.as_str()])?;
            }
            if !by_bytes {
                // Only one global delivery slot remains. A rejected peer must
                // not consume it and prevent the eligible sibling's admission.
                for n in 0..GLOBAL_DELIVERIES-PEER_DELIVERIES-1 {
                    tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,?,?)",params![format!("{:032x}",1000),format!("{:032x}",2000+n),1,peer.fingerprint.as_str()])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    #[tokio::test]
    async fn full_recipient_count_and_bytes_are_isolated_before_and_during_staging() -> Result<()> {
        for by_bytes in [false, true] {
            for during_copy in [false, true] {
                let (dir, q, peers, source) = fixture().await?;
                let (operation, _) = q
                    .start_operation(vec![source.clone()], peers.clone(), Vec::new(), None)
                    .await?;
                if during_copy {
                    let store = q.store.clone();
                    let peer = peers[0].clone();
                    *q.copy_hook.lock().unwrap() = Some(Arc::new(move |_| {
                        fill_recipient_capacity(&store, &peer, by_bytes).unwrap();
                    }));
                } else {
                    fill_recipient_capacity(&q.store, &peers[0], by_bytes)?;
                }
                let (artifact, rejected) = q
                    .admit_selected(
                        operation.clone(),
                        source.clone(),
                        peers.clone(),
                        PEER_BYTES,
                        id(99),
                    )
                    .await?;
                assert_eq!(rejected.len(), 1);
                assert_eq!(rejected[0].device, peers[0].device);
                assert_eq!(rejected[0].reason, "device outbox is full");
                // Simulate a crash before the admission worker records its result.
                // The artifact transaction must retain recipient rejections too.
                q.store.drain().await;
                drop(q);
                let q = Outbox::open(
                    Store::open(&dir.path().join("state.sqlite3"))?,
                    &dir.path().join("outbox"),
                )?;
                let status = q.operation_status(operation.clone()).await?;
                assert_eq!(status["state"], "admitted");
                assert_eq!(
                    status["accepted"],
                    serde_json::json!([{"path":source,"transfer_id":artifact}])
                );
                assert_eq!(
                    status["rejected"],
                    serde_json::json!([{"path":source,"device":peers[0].device,"reason":"device outbox is full"}])
                );
                let rows: Vec<_> = q
                    .snapshot(&[peers[1].device.clone()])
                    .into_iter()
                    .filter(|r| r.operation == operation)
                    .collect();
                assert_eq!(rows.len(), 2);
                assert!(rows.iter().any(|r| r.device == peers[1].device
                    && r.state == "queued"
                    && r.transfer_id == artifact));
                assert!(rows.iter().any(|r| r.device == peers[0].device
                    && r.state == "failed"
                    && r.file_name == "photo.png"
                    && r.path.is_none()));
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn recipient_rejections_keep_recent_bounded_and_status_path_specific() -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        let paths: Vec<_> = (0..128)
            .map(|n| source.with_file_name(format!("file-{n}.bin")))
            .collect();
        let rejected:Vec<_>=paths.iter().flat_map(|path|peers.iter().map(move |peer|serde_json::json!({"path":path,"device":peer.device,"reason":"device outbox is full"}))).collect();
        let (operation, _) = q
            .start_operation(paths, Vec::new(), rejected.clone(), None)
            .await?;
        q.admission_result(operation.clone(), Vec::new(), rejected.clone(), true)
            .await?;
        assert_eq!(
            q.operation_status(operation).await?["rejected"],
            serde_json::json!(rejected)
        );
        let rows = q.snapshot(&[]);
        assert_eq!(rows.len(), peers.len());
        for peer in peers {
            let row = rows.iter().find(|r| r.device == peer.device).unwrap();
            assert_eq!(row.file_name, "128 files");
            assert_eq!(row.state, "failed");
            assert_eq!(row.error.as_deref(), Some("device outbox is full"));
        }
        Ok(())
    }
    #[tokio::test]
    async fn stale_recipient_at_staging_commit_is_rejected_without_losing_valid_sibling()
    -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        let store = q.store.clone();
        let peer = peers[0].clone();
        *q.copy_hook.lock().unwrap() = Some(Arc::new(move |_| {
            tokio::runtime::Handle::current()
                .block_on(store.pair(peer.device.clone(), peer.fingerprint.clone()))
                .unwrap();
        }));
        let (transfer, rejected) = q
            .admit_selected(id(9), source, peers.clone(), PEER_BYTES, id(99))
            .await?;
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].device, peers[0].device);
        assert_eq!(rejected[0].reason, "Pairing changed");
        let rows = q.snapshot(&[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device, peers[1].device);
        assert_eq!(rows[0].transfer_id, transfer);
        Ok(())
    }
    #[tokio::test]
    async fn admission_restart_preserves_committed_paths_and_rejects_only_unstaged_paths()
    -> Result<()> {
        let (dir, q, peers, source) = fixture().await?;
        let missing = dir.path().join("not-staged");
        let (operation, _) = q
            .start_operation(
                vec![source.clone(), missing.clone()],
                peers.clone(),
                Vec::new(),
                Some("restart-request".into()),
            )
            .await?;
        let (transfer, _) = q
            .admit_selected(operation.clone(), source.clone(), peers, PEER_BYTES, id(99))
            .await?;
        // Crash between the artifact transaction and the admission completion.
        q.store.drain().await;
        drop(q);
        let q = Outbox::open(
            Store::open(&dir.path().join("state.sqlite3"))?,
            &dir.path().join("outbox"),
        )?;
        let status = q.operation_status(operation.clone()).await?;
        assert_eq!(
            status["accepted"],
            serde_json::json!([{"path":source,"transfer_id":transfer}])
        );
        assert_eq!(status["rejected"].as_array().unwrap().len(), 1);
        assert_eq!(status["rejected"][0]["path"], serde_json::json!(missing));
        assert_eq!(status["state"], "failed");
        assert_eq!(
            q.request_operation("restart-request".into()).await?,
            Some(operation)
        );
        Ok(())
    }
    #[tokio::test]
    async fn dismissed_history_stays_hidden_after_operation_pruning_and_restart() -> Result<()> {
        let (dir, q, peers, source) = fixture().await?;
        let (operation, _) = q
            .start_operation(
                vec![source.clone()],
                peers.clone(),
                Vec::new(),
                Some("old-request".into()),
            )
            .await?;
        q.admit_selected(
            operation.clone(),
            source.clone(),
            peers.clone(),
            PEER_BYTES,
            id(99),
        )
        .await?;
        q.admission_result(operation.clone(), Vec::new(), Vec::new(), true)
            .await?;
        q.change(operation.clone(), None, None, false).await?;
        q.dismiss(operation.clone(), None).await?;
        for n in 0..51 {
            let (op, _) = q
                .start_operation(
                    vec![source.clone()],
                    Vec::new(),
                    Vec::new(),
                    Some(format!("later-{n}")),
                )
                .await?;
            q.admission_result(
                op,
                Vec::new(),
                vec![serde_json::json!({"path":source,"reason":"no recipients"})],
                true,
            )
            .await?;
        }
        assert!(q.snapshot(&[]).iter().all(|t| t.operation != operation));
        assert_eq!(
            q.request_operation("old-request".into()).await?,
            Some(operation.clone())
        );
        assert_eq!(
            q.operation_status(operation.clone()).await?["state"],
            "expired"
        );
        q.store.drain().await;
        drop(q);
        let q = Outbox::open(
            Store::open(&dir.path().join("state.sqlite3"))?,
            &dir.path().join("outbox"),
        )?;
        assert!(q.snapshot(&[]).iter().all(|t| t.operation != operation));
        assert_eq!(
            q.request_operation("old-request".into()).await?,
            Some(operation)
        );
        Ok(())
    }
}

#[cfg(test)]
mod projection_regression_tests {
    use super::tests::{fixture, id};
    use super::*;
    #[tokio::test]
    async fn refresh_publication_serializes_registration_completion_and_cancellation() -> Result<()>
    {
        for mutation in ["registration", "completion", "cancellation"] {
            let (_dir, q, peers, source) = fixture().await?;
            let (operation, _) = q
                .start_operation(vec![source.clone()], peers.clone(), Vec::new(), None)
                .await?;
            let artifact = q
                .admit(operation.clone(), source.clone(), peers.clone(), PEER_BYTES)
                .await?;
            let accepted = vec![serde_json::json!({"path":source,"transfer_id":artifact})];
            let (entered, wait) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            *q.projection_barrier.lock().unwrap() = Some((entered, blocked));
            let stale = q.clone();
            let refresh = tokio::task::spawn_blocking(move || stale.refresh_operations());
            wait.await?; // Old database snapshot read; publication deliberately paused.
            let newer = q.clone();
            let original = operation.clone();
            let change = tokio::spawn(async move {
                match mutation {
                    "registration" => newer
                        .start_operation(vec![source], peers, Vec::new(), None)
                        .await
                        .map(|(id, _)| id),
                    "completion" => newer
                        .admission_result(original.clone(), accepted, Vec::new(), true)
                        .await
                        .map(|_| original),
                    "cancellation" => newer
                        .change(original.clone(), None, None, false)
                        .await
                        .map(|_| original),
                    _ => unreachable!(),
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while q.store.active_jobs() == 0 && !change.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            // The paused read must still own the DB lock. This assertion is
            // deterministic; it doesn't rely on the writer winning a race.
            let db_held = q.store.connection.try_lock().is_err();
            release.send(())?;
            refresh.await??;
            let updated = change.await??;
            assert!(
                db_held,
                "{mutation}: read lock released before cache publication"
            );
            let operations = q.operations.lock().unwrap();
            let row = operations.iter().find(|o| o.operation == updated).unwrap();
            assert_eq!(
                row.state,
                match mutation {
                    "completion" => "admitted",
                    "cancellation" => "cancelled",
                    _ => "staging",
                }
            );
            drop(operations);
            let view = q.snapshot(&[]);
            if mutation == "registration" {
                assert!(
                    view.iter()
                        .any(|r| r.operation == updated && r.state == "staging")
                );
            } else {
                assert!(
                    !view
                        .iter()
                        .any(|r| r.operation == operation && r.state == "staging")
                );
                if mutation == "cancellation" {
                    assert!(
                        view.iter()
                            .filter(|r| r.operation == operation)
                            .all(|r| r.state == "cancelled")
                    );
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn oversized_operation_is_retained_whole_and_active_siblings_remain_visible() -> Result<()>
    {
        let (_dir, q, peers, source) = fixture().await?;
        q.admit(id(9), source, peers, PEER_BYTES).await?;
        let template = q.snapshot(&[])[0].clone();
        let mut rows = Vec::new();
        for n in 0..129u8 {
            let mut row = template.clone();
            row.device = id(n);
            row.state = "sent".into();
            row.finished_at = Some(200);
            rows.push(row);
        }
        let mut older = template;
        older.operation = id(8);
        older.state = "sent".into();
        older.finished_at = Some(100);
        rows.push(older.clone());
        let projected = recent_operations(rows.clone());
        assert_eq!(projected.len(), 129);
        assert!(projected.iter().all(|r| r.operation == id(9)));
        rows.last_mut().unwrap().state = "failed".into();
        assert_eq!(recent_operations(rows).len(), 130);
        Ok(())
    }
    #[tokio::test]
    async fn staging_projection_does_not_multiply_paths_by_recipient_count() -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        let mut recipients = Vec::new();
        for n in 0..128 {
            let mut peer = peers[0].clone();
            peer.device = id(n);
            recipients.push(peer);
        }
        let paths = (0..128)
            .map(|n| source.with_file_name(format!("{n}-{}", "x".repeat(200))))
            .collect();
        let (operation, _) = q
            .start_operation(paths, recipients, Vec::new(), Some("large-picker".into()))
            .await?;
        let view = q.snapshot(&[]);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].operation, operation);
        assert_eq!(view[0].state, "staging");
        assert!(serde_json::to_vec(&view)?.len() < 1024);
        Ok(())
    }
    #[tokio::test]
    async fn removing_failed_history_cancels_deliveries_and_releases_capacity() -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        let artifact = q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        for (index, peer) in peers.iter().enumerate() {
            let delivery = q
                .claim(peer.device.clone(), peer.generation, index as u64 + 1)
                .await?
                .unwrap();
            q.fail(delivery, "unavailable".into()).await?;
        }
        q.dismiss(id(9), None).await?;
        assert!(q.snapshot(&[]).is_empty());
        assert!(!q.folder.path()?.join(artifact.as_str()).exists());
        q.store.call(|c| {
            assert_eq!(c.query_row("SELECT count(*) FROM outbox_deliveries WHERE state NOT IN ('sent','cancelled')", [], |r| r.get::<_,i64>(0))?, 0);
            assert_eq!(c.query_row("SELECT count(*) FROM outbox_artifacts WHERE staged=1", [], |r| r.get::<_,i64>(0))?, 0);
            Ok(())
        }).await?;
        assert!(q.change(id(9), None, Some(id(1)), true).await.is_err());
        assert!(
            q.snapshot(&[]).is_empty(),
            "failed retry must not unhide removed history"
        );
        Ok(())
    }
}

#[cfg(test)]
mod session_reconciliation_regressions {
    use super::tests::{fixture, id};
    use super::*;
    #[tokio::test]
    async fn queued_reconciliation_reads_live_authority_after_replacement_claim_commits()
    -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        q.claim(id(1), peers[0].generation, 10).await?.unwrap();
        let generation = peers[0].generation;
        let live = Arc::new(Mutex::new(vec![(id(1), generation, 10)]));
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let store = q.store.clone();
        let holder = tokio::spawn(async move {
            store.call(move |c| {
            let _=entered.send(());blocked.recv_timeout(std::time::Duration::from_secs(10))?;
            // Model the replacement's committed claim while a reconciliation
            // submitted earlier is still waiting for this DB connection.
            c.execute("UPDATE outbox_deliveries SET session=11 WHERE device=? AND state='sending'",[id(1).as_str()])?;Ok(())
        }).await
        });
        waiting.await?;
        let queue = q.clone();
        let observed = live.clone();
        let reconcile = tokio::spawn(async move {
            queue
                .reconcile_sessions(move || observed.lock().unwrap().clone())
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while q.store.active_jobs() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        *live.lock().unwrap() = vec![(id(1), generation, 11)];
        release.send(())?;
        holder.await??;
        reconcile.await??;
        assert_eq!(q.snapshot(&[])[0].state, "sending");
        assert_eq!(
            q.store
                .call(|c| Ok(c.query_row(
                    "SELECT session FROM outbox_deliveries WHERE device=?",
                    [id(1).as_str()],
                    |r| r.get::<_, i64>(0)
                )?))
                .await?,
            11
        );
        Ok(())
    }
}

#[cfg(test)]
mod artifact_action_regressions {
    use super::tests::{fixture, id};
    use super::*;

    #[tokio::test]
    async fn artifact_actions_preserve_sibling_files_and_recipient_deliveries() -> Result<()> {
        let (dir, q, peers, source) = fixture().await?;
        let a = q
            .admit(id(9), source.clone(), peers.clone(), PEER_BYTES)
            .await?;
        let b = q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        let first = q.claim(id(1), peers[0].generation, 1).await?.unwrap();
        let other = q.claim(id(2), peers[1].generation, 2).await?.unwrap();
        assert_eq!(first.begin.transfer_id, a);
        q.fail(first, "failure".into()).await?;
        q.fail(other, "failure".into()).await?;
        q.change(id(9), Some(a.clone()), Some(id(1)), true).await?;
        let rows = q.snapshot(&[]);
        assert_eq!(
            rows.iter()
                .find(|r| r.transfer_id == a && r.device == id(1))
                .unwrap()
                .state,
            "waiting_for_device"
        );
        assert_eq!(
            rows.iter()
                .find(|r| r.transfer_id == a && r.device == id(2))
                .unwrap()
                .state,
            "failed"
        );
        assert!(
            rows.iter()
                .filter(|r| r.transfer_id == b)
                .all(|r| r.state == "waiting_for_device")
        );
        q.change(id(9), Some(a.clone()), Some(id(1)), false).await?;
        assert!(
            q.folder.path()?.join(a.as_str()).exists(),
            "other recipient still needs bytes"
        );
        q.change(id(9), Some(a.clone()), None, false).await?;
        assert!(!q.folder.path()?.join(a.as_str()).exists());
        assert!(q.folder.path()?.join(b.as_str()).exists());
        q.dismiss(id(9), Some(a.clone())).await?;
        assert_eq!(q.snapshot(&[]).len(), 2);
        assert!(q.snapshot(&[]).iter().all(|r| r.transfer_id == b));
        q.store.drain().await;
        drop(q);
        let q = Outbox::open(
            Store::open(&dir.path().join("state.sqlite3"))?,
            &dir.path().join("outbox"),
        )?;
        let rows = q.snapshot(&[]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.transfer_id == b));
        assert_eq!(
            serde_json::to_value(&rows[0])?["transfer_id"],
            serde_json::json!(b)
        );
        Ok(())
    }

    #[tokio::test]
    async fn dismiss_failed_artifact_reclaims_only_its_capacity_while_siblings_are_active()
    -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        let a = q
            .admit(id(9), source.clone(), peers.clone(), PEER_BYTES)
            .await?;
        let b = q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        for (index, peer) in peers.iter().enumerate() {
            let delivery = q
                .claim(peer.device.clone(), peer.generation, index as u64 + 1)
                .await?
                .unwrap();
            q.fail(delivery, "unavailable".into()).await?;
        }
        let sibling = q.claim(id(1), peers[0].generation, 3).await?.unwrap();
        assert_eq!(sibling.begin.transfer_id, b);
        assert!(q.dismiss(id(9), None).await.is_err());
        q.dismiss(id(9), Some(a.clone())).await?;
        assert!(q.current(&sibling));
        assert!(!q.folder.path()?.join(a.as_str()).exists());
        assert!(q.folder.path()?.join(b.as_str()).exists());
        q.store.call(move |c| {
            assert_eq!(c.query_row("SELECT count(*) FROM outbox_deliveries WHERE state NOT IN ('sent','cancelled')", [], |r| r.get::<_,i64>(0))?, 2);
            assert_eq!(c.query_row("SELECT count(*) FROM outbox_artifacts WHERE staged=1", [], |r| r.get::<_,i64>(0))?, 1);
            Ok(())
        }).await?;
        assert_eq!(q.snapshot(&[]).len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn artifact_commands_reject_foreign_and_unknown_ids_without_batch_fallback() -> Result<()>
    {
        let (_dir, q, peers, source) = fixture().await?;
        let a = q
            .admit(id(9), source.clone(), peers.clone(), PEER_BYTES)
            .await?;
        let foreign = q.admit(id(8), source, peers, PEER_BYTES).await?;
        for artifact in [foreign, id(77)] {
            for retry in [false, true] {
                let error = q
                    .change(id(9), Some(artifact.clone()), Some(id(1)), retry)
                    .await
                    .unwrap_err();
                assert_eq!(error.to_string(), "artifact does not belong to operation");
            }
            assert_eq!(
                q.dismiss(id(9), Some(artifact))
                    .await
                    .unwrap_err()
                    .to_string(),
                "artifact does not belong to operation"
            );
        }
        assert_eq!(q.snapshot(&[]).len(), 4);
        assert!(
            q.snapshot(&[])
                .iter()
                .all(|r| r.state == "waiting_for_device")
        );
        assert!(q.folder.path()?.join(a.as_str()).exists());
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_one_artifact_during_staging_does_not_cancel_later_paths() -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        let (operation, _) = q
            .start_operation(vec![source.clone(); 2], peers.clone(), Vec::new(), None)
            .await?;
        let a = q
            .admit(operation.clone(), source.clone(), peers.clone(), PEER_BYTES)
            .await?;
        q.change(operation.clone(), Some(a.clone()), None, false)
            .await?;
        assert_eq!(
            q.operation_status(operation.clone()).await?["state"],
            "staging"
        );
        let b = q
            .admit(operation.clone(), source, peers, PEER_BYTES)
            .await?;
        assert_ne!(a, b);
        assert!(
            q.snapshot(&[])
                .iter()
                .filter(|r| r.transfer_id == a)
                .all(|r| r.state == "cancelled")
        );
        assert!(
            q.snapshot(&[])
                .iter()
                .filter(|r| r.transfer_id == b)
                .all(|r| r.state == "waiting_for_device")
        );
        Ok(())
    }
}

#[cfg(test)]
mod cancelled_recipient_regressions {
    use super::tests::{fixture, id};
    use super::*;

    #[tokio::test]
    async fn cancelling_staging_recipient_during_copy_is_not_an_admission_rejection() -> Result<()>
    {
        let (_dir, q, peers, source) = fixture().await?;
        let (operation, _) = q
            .start_operation(vec![source.clone(); 2], peers.clone(), Vec::new(), None)
            .await?;
        let worker = q.clone();
        let target = operation.clone();
        *q.copy_hook.lock().unwrap() = Some(Arc::new(move |_| {
            tokio::runtime::Handle::current()
                .block_on(worker.change(target.clone(), None, Some(id(1)), false))
                .unwrap();
        }));
        for _ in 0..2 {
            let (_, rejected) = q
                .admit_selected(
                    operation.clone(),
                    source.clone(),
                    peers.clone(),
                    PEER_BYTES,
                    id(99),
                )
                .await?;
            assert!(rejected.is_empty());
        }
        let rows = q.snapshot(&[]);
        assert_eq!(rows.iter().filter(|r| r.generation != 0).count(), 2);
        assert!(
            rows.iter()
                .filter(|r| r.generation != 0)
                .all(|r| r.device == id(2))
        );
        assert_eq!(
            q.operation_status(operation).await?["rejected"],
            serde_json::json!([])
        );
        Ok(())
    }
}

#[cfg(test)]
mod removed_history_recovery_regressions {
    use super::tests::{fixture, id};
    use super::*;
    #[tokio::test]
    async fn restart_reclaims_payloads_hidden_by_the_old_remove_path() -> Result<()> {
        let (dir, q, peers, source) = fixture().await?;
        let artifact = q.admit(id(9), source, peers.clone(), PEER_BYTES).await?;
        for (index, peer) in peers.iter().enumerate() {
            let delivery = q
                .claim(peer.device.clone(), peer.generation, index as u64 + 1)
                .await?
                .unwrap();
            q.fail(delivery, "failed before Remove".into()).await?;
        }
        // The former Remove implementation persisted only this marker.
        q.store
            .call(|c| {
                c.execute(
                    "INSERT INTO outbox_dismissed(operation) VALUES(?)",
                    [id(9).as_str()],
                )?;
                Ok(())
            })
            .await?;
        assert!(q.folder.path()?.join(artifact.as_str()).exists());
        q.store.drain().await;
        drop(q);
        let q = Outbox::open(
            Store::open(&dir.path().join("state.sqlite3"))?,
            &dir.path().join("outbox"),
        )?;
        assert!(q.snapshot(&[]).is_empty());
        assert!(!q.folder.path()?.join(artifact.as_str()).exists());
        q.store.call(|c| {
            assert_eq!(c.query_row("SELECT count(*) FROM outbox_deliveries WHERE state NOT IN ('sent','cancelled')", [], |r| r.get::<_,i64>(0))?, 0);
            assert_eq!(c.query_row("SELECT count(*) FROM outbox_artifacts WHERE staged=1", [], |r| r.get::<_,i64>(0))?, 0);
            Ok(())
        }).await?;
        Ok(())
    }
}

#[cfg(test)]
mod dismissed_cancellation_regressions {
    use super::tests::fixture;
    use super::*;
    #[tokio::test]
    async fn removing_cancelled_staging_history_does_not_resume_source_admission() -> Result<()> {
        let (_dir, q, peers, source) = fixture().await?;
        let (operation, _) = q
            .start_operation(vec![source.clone(); 128], peers.clone(), Vec::new(), None)
            .await?;
        q.change(operation.clone(), None, None, false).await?;
        q.dismiss(operation.clone(), None).await?;
        assert!(q.snapshot(&[]).is_empty());
        *q.source_hook.lock().unwrap() = Some(Arc::new(|_| {
            panic!("cancelled admission reopened its next source")
        }));
        assert_eq!(
            q.admit_selected(
                operation.clone(),
                source,
                peers,
                PEER_BYTES,
                Id::from_bytes([99; 16])
            )
            .await
            .unwrap_err()
            .to_string(),
            "operation cancelled"
        );
        q.admission_result(
            operation.clone(),
            Vec::new(),
            vec![serde_json::json!({"reason":"cancellation raced a path error"})],
            true,
        )
        .await?;
        assert_eq!(
            q.operation_status(operation).await?["rejected"],
            serde_json::json!([])
        );
        assert!(q.snapshot(&[]).is_empty());
        Ok(())
    }
}
