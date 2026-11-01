//! Durable, binding-scoped file prefixes; catalogue ownership follows data fsync.
use super::*;
use crate::platform::fs as pfs;
use std::{
    io::{Read, Seek, SeekFrom},
    sync::Weak,
    time::{Duration, Instant},
};
const TTL: u64 = 24 * 60 * 60 * 1000;
const ROLLBACK: u64 = 60 * 60 * 1000;
const BYTE_LIMIT: u64 = 16 * 1024 * 1024 * 1024;
type PendingCancel = Weak<tokio::sync::watch::Sender<bool>>;
#[derive(Default)]
pub(crate) struct Runtime {
    live: HashMap<(Id, Id), Weak<crate::storage::StageInner>>,
    progress: HashMap<(Id, Id), Instant>,
    pending: HashMap<(Id, Id), Vec<PendingCancel>>,
    expired: HashSet<(Id, Id)>,
}
impl Runtime {
    fn lease(&mut self, key: (Id, Id), stage: &Stage) -> Result<()> {
        if self
            .pending
            .get(&key)
            .is_some_and(|tokens| tokens.iter().filter_map(Weak::upgrade).any(|c| *c.borrow()))
        {
            stage.0.cancelled.store(true, Ordering::SeqCst);
        }
        if let Some(active) = self.live.get(&key).and_then(Weak::upgrade) {
            ensure!(Arc::ptr_eq(&active, &stage.0), "resume stage leased");
        }
        self.live.insert(key, Arc::downgrade(&stage.0));
        Ok(())
    }
    fn forget(&mut self, key: &(Id, Id)) {
        self.live.remove(key);
        self.progress.remove(key);
        self.expired.remove(key);
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Partial {
    device: Id,
    generation: u64,
    begin: FileBegin,
    root: PathBuf,
    root_id: pfs::Identity,
    directory_id: pfs::Identity,
    file_id: pfs::Identity,
    name: String,
    destination: String,
    offset: u64,
    checksum: String,
    progress: u64,
    #[serde(default)]
    deleting: bool,
}
#[derive(Debug, thiserror::Error)]
#[error("resume identity changed")]
struct IdentityChanged;
#[derive(Debug, thiserror::Error)]
#[error("resume stage missing")]
struct StageMissing;
impl Partial {
    fn open(&self) -> Result<Stage> {
        ensure!(
            self.name.len() == 32 && self.name.bytes().all(|b| b.is_ascii_hexdigit()),
            IdentityChanged
        );
        let folder = Folder::open(&self.root, false)?;
        ensure!(folder.identity()? == self.root_id, IdentityChanged);
        let directory = staging_directory(&folder)?;
        ensure!(directory.identity()? == self.directory_id, IdentityChanged);
        let file = match pfs::open_read_write_at(&directory.file, &self.name) {
            Ok(file) => file,
            Err(error) if pfs::is_missing(&error) => return Err(StageMissing.into()),
            Err(error) => return Err(error),
        };
        crate::platform::private::check(&file)?;
        pfs::single_link(&file)?;
        ensure!(pfs::is_owned_private(&file)?, IdentityChanged);
        ensure!(
            pfs::identity(&file)? == self.file_id && file.metadata()?.is_file(),
            IdentityChanged
        );
        Ok(Stage::reopen(
            folder,
            directory,
            file,
            self.name.clone(),
            self.destination.clone(),
        ))
    }
    fn rehash(
        &self,
        stage: &Stage,
        cancel: Option<&tokio::sync::watch::Sender<bool>>,
    ) -> Result<Sha256> {
        ensure!(
            self.offset <= self.begin.size && stage.0.file.metadata()?.len() >= self.offset,
            "short checkpoint"
        );
        let mut input = stage.0.file.try_clone()?;
        input.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut remaining = self.offset;
        let mut buffer = vec![0; pneu_proto::CHUNK_SIZE];
        let deadline = Instant::now() + Duration::from_secs(90);
        while remaining > 0 {
            ensure!(Instant::now() < deadline, "resume rehash timeout");
            ensure!(
                !stage.0.cancelled.load(Ordering::SeqCst) && !cancel.is_some_and(|c| *c.borrow()),
                "resume cancelled"
            );
            let count =
                input.read(&mut buffer[..remaining.min(pneu_proto::CHUNK_SIZE as u64) as usize])?;
            ensure!(count > 0, "short checkpoint");
            hash.update(&buffer[..count]);
            remaining -= count as u64;
        }
        ensure!(
            hex::encode(hash.clone().finalize()) == self.checksum,
            "checkpoint checksum mismatch"
        );
        stage.0.file.set_len(self.offset)?;
        input.seek(SeekFrom::Start(self.offset))?;
        Ok(hash)
    }
}
impl Store {
    pub(crate) async fn maintain_partials(&self) -> Result<()> {
        self.blocking(|store| store.prune_partials()).await
    }
    fn partials(&self) -> Result<Vec<Partial>> {
        self.locked(|c| {
            let mut q = c.prepare("SELECT record FROM file_partials")?;
            q.query_map([], |r| r.get::<_, String>(0))?
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect()
        })
    }
    fn partial_binding_current(&self, p: &Partial) -> Result<bool> {
        self.locked(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM peers WHERE id=? AND generation=?)",
                params![p.device.as_str(), p.generation as i64],
                |r| r.get(0),
            )?)
        })
    }
    fn delete_partial(&self, p: &Partial) -> Result<bool> {
        // Inaccessible storage keeps its identity-bearing, budgeted deletion
        // record. Substituted entries are protected from orphan sweeping.
        match p.open().and_then(|stage| stage.cleanup()) {
            Ok(()) => {}
            Err(error) if error.downcast_ref::<StageMissing>().is_some() => {}
            Err(error)
                if error.downcast_ref::<IdentityChanged>().is_some()
                    || pfs::is_link(&error)
                    || pfs::is_not_directory(&error) =>
            {
                self.locked(|c| {
                    c.execute(
                        "INSERT OR IGNORE INTO file_stage_protected VALUES(?,?)",
                        params![p.root.to_str().context("UTF-8 root")?, p.name],
                    )?;
                    Ok(())
                })?
            }
            Err(_) => {
                let mut deferred = p.clone();
                deferred.deleting = true;
                self.locked(|c| {
                    c.execute(
                        "UPDATE file_partials SET record=? WHERE device=? AND id=?",
                        params![
                            serde_json::to_string(&deferred)?,
                            p.device.as_str(),
                            p.begin.transfer_id.as_str()
                        ],
                    )?;
                    Ok(())
                })?;
                return Ok(false);
            }
        }
        self.locked(|c| {
            c.execute(
                "DELETE FROM file_partials WHERE device=? AND id=?",
                params![p.device.as_str(), p.begin.transfer_id.as_str()],
            )?;
            Ok(())
        })?;
        Ok(true)
    }
    fn prune_partials(&self) -> Result<()> {
        let now = crate::now_ms();
        let rollback=self.locked(|c|{
            let high=c.query_row("SELECT wall FROM resume_clock WHERE singleton=1",[],|r|r.get::<_,i64>(0)).optional()?.map(|n|n as u64).unwrap_or(now);
            let rollback=now.saturating_add(ROLLBACK)<high;
            c.execute("INSERT INTO resume_clock VALUES(1,?) ON CONFLICT(singleton) DO UPDATE SET wall=excluded.wall",[(if rollback{now}else{now.max(high)}) as i64])?;Ok(rollback)})?;
        let mut rows = self.partials()?;
        rows.sort_by_key(|p| p.progress);
        let mut bytes = rows.iter().fold(0u64, |n, p| n.saturating_add(p.offset));
        let mut jobs = rows.len();
        let mut peers = HashMap::<Id, usize>::new();
        for p in &rows {
            *peers.entry(p.device.clone()).or_default() += 1;
        }
        let mut runtime = self.resume.lock().expect("resume runtime");
        runtime.live.retain(|_, s| s.strong_count() > 0);
        if rollback {
            runtime.expired.extend(
                rows.iter()
                    .map(|p| (p.device.clone(), p.begin.transfer_id.clone())),
            );
        }
        for p in rows {
            let key = (p.device.clone(), p.begin.transfer_id.clone());
            if runtime.live.contains_key(&key) {
                continue;
            }
            let elapsed = runtime
                .progress
                .get(&key)
                .map(|at| at.elapsed().as_millis() as u64)
                .unwrap_or_else(|| now.saturating_sub(p.progress));
            if !self.partial_binding_current(&p)?
                || p.deleting
                || runtime.expired.contains(&key)
                || elapsed >= TTL
                || bytes > BYTE_LIMIT
                || jobs > 64
                || peers[&p.device] > 8
            {
                if !self.delete_partial(&p)? {
                    continue;
                }
                bytes = bytes.saturating_sub(p.offset);
                jobs -= 1;
                *peers.get_mut(&p.device).expect("peer count") -= 1;
                runtime.progress.remove(&key);
                runtime.expired.remove(&key);
            }
        }
        Ok(())
    }
    pub(super) fn reconcile_resume_folder(&self, folder: &Folder) -> Result<()> {
        self.prune_partials()?;
        crate::storage::clean_legacy_staging(folder)?;
        let directory = staging_directory(folder)?;
        let mut retained = self.locked(|c| {
            let mut q = c.prepare("SELECT name FROM file_stage_protected WHERE root=?")?;
            Ok(
                q.query_map([folder.path()?.to_str().context("UTF-8 root")?], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<HashSet<_>>>()?,
            )
        })?;
        for p in self.partials()? {
            if p.root_id != folder.identity()? && p.root != folder.path()? {
                continue;
            }
            retained.insert(p.name.clone());
            let live = self
                .resume
                .lock()
                .expect("resume runtime")
                .live
                .get(&(p.device.clone(), p.begin.transfer_id.clone()))
                .is_some_and(|s| s.strong_count() > 0);
            if !live && p.open().and_then(|stage| p.rehash(&stage, None)).is_err() {
                self.delete_partial(&p)?;
                self.resume
                    .lock()
                    .expect("resume runtime")
                    .forget(&(p.device, p.begin.transfer_id));
            }
        }
        for entry in pfs::entries(&directory.file)? {
            let name = entry?;
            if name == std::ffi::OsStr::new(".") || name == std::ffi::OsStr::new("..") {
                continue;
            }
            if retained
                .iter()
                .any(|s| name.as_os_str() == std::ffi::OsStr::new(s))
            {
                continue;
            }
            let file = pfs::metadata_file(&directory.file, &name)?;
            if file.metadata()?.is_file() {
                crate::platform::private::check(&file)?;
            }
            if pfs::is_directory_at(&directory.file, &name)? {
                let child = pfs::open_directory_at(&directory.file, &name)?;
                crate::platform::private::check(&child)?;
                crate::storage::wipe(&child)?;
                pfs::remove_directory(&directory.file, &name, &child)?;
            } else {
                pfs::remove_file(&directory.file, &name, &file)?;
            }
        }
        directory.file.sync_all()?;
        Ok(())
    }
    pub(crate) async fn discard_legacy_partial(&self, device: Id, begin: FileBegin) -> Result<()> {
        self.blocking(move |store| {
            if let Some(p) = store
                .partials()?
                .into_iter()
                .find(|p| p.device == device && p.begin.transfer_id == begin.transfer_id)
            {
                ensure!(p.begin.content_eq(&begin), ReceiptConflict);
                store.delete_partial(&p)?;
                store
                    .resume
                    .lock()
                    .expect("resume runtime")
                    .forget(&(p.device, p.begin.transfer_id));
            }
            Ok(())
        })
        .await
    }
    #[cfg(test)]
    pub(crate) async fn resume_stage(
        &self,
        device: Id,
        begin: FileBegin,
    ) -> Result<Option<(Stage, u64, Sha256)>> {
        let cancel = self.register_resume_begin(&device, &begin.transfer_id);
        self.resume_stage_cancellable(device, begin, cancel).await
    }
    pub(crate) async fn resume_stage_cancellable(
        &self,
        device: Id,
        begin: FileBegin,
        cancel: Arc<tokio::sync::watch::Sender<bool>>,
    ) -> Result<Option<(Stage, u64, Sha256)>> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            ensure!(!*cancel.borrow(), "resume cancelled");
            store.prune_partials()?;
            let Some(p) = store
                .partials()?
                .into_iter()
                .find(|p| p.device == device && p.begin.transfer_id == begin.transfer_id)
            else {
                return Ok(None);
            };
            store.locked(|c| epoch.check(c))?;
            ensure!(p.begin.content_eq(&begin), ReceiptConflict);
            ensure!(!p.deleting, "resume deletion pending");
            ensure!(!*cancel.borrow(), "resume cancelled");
            let key = (device, begin.transfer_id);
            let opened = {
                let mut runtime = store.resume.lock().expect("resume runtime");
                ensure!(
                    runtime.live.get(&key).is_none_or(|s| s.strong_count() == 0),
                    "resume stage leased"
                );
                p.open().and_then(|stage| {
                    runtime.lease(key.clone(), &stage)?;
                    Ok(stage)
                })
            };
            // Apply the same receipt reserve and destination headroom as a
            // fresh receive, charging only bytes still to be written.
            if let Ok(stage) = &opened {
                #[cfg(test)]
                store.fault("admission")?;
                capacity(&store.folder, store.reserve.load(Ordering::SeqCst))?;
                ensure!(
                    stage.0.folder.space()?
                        >= begin
                            .size
                            .saturating_sub(p.offset)
                            .saturating_add(16 * 1024 * 1024),
                    pfs::no_space()
                );
            }
            match opened.and_then(|stage| Ok((p.rehash(&stage, Some(&cancel))?, stage))) {
                Ok((hash, stage)) => {
                    store
                        .resume
                        .lock()
                        .expect("resume runtime")
                        .live
                        .insert(key, Arc::downgrade(&stage.0));
                    Ok(Some((stage, p.offset, hash)))
                }
                Err(_) => {
                    store.delete_partial(&p)?;
                    store.resume.lock().expect("resume runtime").forget(&key);
                    Ok(None)
                }
            }
        })
        .await
    }
    pub(crate) async fn checkpoint(
        &self,
        device: Id,
        begin: FileBegin,
        stage: Stage,
        offset: u64,
        hash: Sha256,
        force: bool,
    ) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        let key = (device.clone(), begin.transfer_id.clone());
        self.resume
            .lock()
            .expect("resume runtime")
            .lease(key.clone(), &stage)?;
        self.blocking(move |store| {
            ensure!(pfs::identity_at(&stage.0.directory.file,&stage.0.name)?==pfs::identity(&stage.0.file)?,"staging file replaced before checkpoint");
            let now = crate::now_ms();
            let previous = store.partials()?.into_iter().find(|p| {
                p.device == device && p.begin.transfer_id == begin.transfer_id
            });
            if let Some(p) = previous {
                ensure!(p.begin.content_eq(&begin),ReceiptConflict);
                ensure!(p.file_id==pfs::identity(&stage.0.file)? && p.root_id==stage.0.folder.identity()?,"checkpoint lease changed");
                let small = offset.saturating_sub(p.offset) < 4 * 1024 * 1024;
                let recent = store.resume.lock().expect("resume runtime").progress.get(&key)
                    .map(|at|at.elapsed()<Duration::from_secs(5))
                    .unwrap_or_else(||now.saturating_sub(p.progress)<5000);
                if offset == p.offset || (!force && small && recent) { return Ok(()); }
            }
            ensure!(!stage.0.cancelled.load(Ordering::SeqCst), "stage cancelled");
            stage.0.file.sync_all()?;
            stage.0.directory.file.sync_all()?;
            let p = Partial {
                device: device.clone(), generation: epoch.generation, begin: begin.clone(),
                root: stage.0.folder.path()?, root_id: stage.0.folder.identity()?,
                directory_id: stage.0.directory.identity()?, file_id: pfs::identity(&stage.0.file)?,
                name: stage.0.name.clone(), destination: stage.0.destination.clone(),
                offset, checksum: hex::encode(hash.finalize()), progress: now, deleting: false,
            };
            store.peer_write(epoch, |tx| {
                if let Some((meta,_))=receipt_row(tx,device.as_str(),begin.transfer_id.as_str())? {
                    ensure!(matches!(meta,Metadata::File(ref retained) if retained.content_eq(&begin)),ReceiptConflict);
                    anyhow::bail!("transfer completed during checkpoint");
                }
                let previous:Option<String>=tx.query_row("SELECT record FROM file_partials WHERE device=? AND id=?",params![device.as_str(),begin.transfer_id.as_str()],|r|r.get(0)).optional()?;
                if let Some(previous)=previous {
                    let previous:Partial=serde_json::from_str(&previous)?;
                    ensure!(previous.begin.content_eq(&begin),ReceiptConflict);
                    ensure!(previous.file_id==p.file_id && previous.generation==p.generation,"checkpoint lease changed");
                }
                tx.execute("INSERT INTO resume_clock VALUES(1,?) ON CONFLICT(singleton) DO UPDATE SET wall=max(wall,excluded.wall)", [now as i64])?;
                tx.execute("INSERT INTO file_partials VALUES(?,?,?) ON CONFLICT(device,id) DO UPDATE SET record=excluded.record", params![device.as_str(), begin.transfer_id.as_str(), serde_json::to_string(&p)?])?;
                Ok(())
            })?;
            stage.0.retained.store(true, Ordering::SeqCst);
            let mut runtime = store.resume.lock().expect("resume runtime");
            runtime.live.insert(key.clone(), Arc::downgrade(&stage.0));
            runtime.progress.insert(key, Instant::now());
            drop(runtime);
            store.prune_partials()?;
            Ok(())
        }).await
    }

    pub(crate) fn register_resume_begin(
        &self,
        device: &Id,
        id: &Id,
    ) -> Arc<tokio::sync::watch::Sender<bool>> {
        let cancel = Arc::new(tokio::sync::watch::channel(false).0);
        let mut runtime = self.resume.lock().expect("resume runtime");
        runtime.pending.retain(|_, tokens| {
            tokens.retain(|t| t.strong_count() > 0);
            !tokens.is_empty()
        });
        runtime
            .pending
            .entry((device.clone(), id.clone()))
            .or_default()
            .push(Arc::downgrade(&cancel));
        cancel
    }
    pub(crate) fn signal_cancel(&self, device: &Id, id: &Id) {
        let runtime = self.resume.lock().expect("resume runtime");
        let key = (device.clone(), id.clone());
        if let Some(tokens) = runtime.pending.get(&key) {
            for token in tokens.iter().filter_map(Weak::upgrade) {
                token.send_replace(true);
            }
        }
        if let Some(stage) = runtime.live.get(&key).and_then(Weak::upgrade) {
            stage.cancelled.store(true, Ordering::SeqCst);
        }
    }
    pub(super) fn purge_revoked_partials(&self) -> Result<()> {
        for p in self.partials()? {
            if !self.partial_binding_current(&p)? || p.deleting {
                self.signal_cancel(&p.device, &p.begin.transfer_id);
                self.delete_partial(&p)?;
                self.resume
                    .lock()
                    .expect("resume runtime")
                    .forget(&(p.device, p.begin.transfer_id));
            }
        }
        Ok(())
    }
    pub(crate) async fn purge_partial(&self, device: Id, id: Id) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.locked(|c| epoch.check(c))?;
            if let Some(p) = store
                .partials()?
                .into_iter()
                .find(|p| p.device == device && p.begin.transfer_id == id)
            {
                store.delete_partial(&p)?;
            }
            store
                .resume
                .lock()
                .expect("resume runtime")
                .forget(&(device, id));
            Ok(())
        })
        .await
    }
    pub(crate) async fn forget_partial(&self, device: Id, id: Id) -> Result<()> {
        let epoch = self.peer_epoch(&device);
        self.blocking(move |store| {
            store.locked(|c| {
                epoch.check(c)?;
                c.execute(
                    "DELETE FROM file_partials WHERE device=? AND id=?",
                    params![device.as_str(), id.as_str()],
                )?;
                Ok(())
            })?;
            store
                .resume
                .lock()
                .expect("resume runtime")
                .forget(&(device, id));
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    async fn fixture() -> Result<(tempfile::TempDir, Store, Id, FileBegin)> {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        let device = Id::from_bytes([1; 16]);
        store
            .pair(device.clone(), Hash::new("ab".repeat(32))?)
            .await?;
        let mut begin = super::super::tests::begin(b"abc");
        begin.resume = Some(true);
        Ok((dir, store, device, begin))
    }
    async fn prefix(
        store: &Store,
        device: &Id,
        begin: &FileBegin,
        root: PathBuf,
    ) -> Result<PathBuf> {
        let stage = store.create_stage(root, "a.txt".into(), begin.size).await?;
        (&stage.0.file).write_all(b"ab")?;
        let mut hash = Sha256::new();
        hash.update(b"ab");
        store
            .checkpoint(device.clone(), begin.clone(), stage.clone(), 2, hash, true)
            .await?;
        let path = stage.path()?;
        drop(stage);
        Ok(path)
    }
    #[tokio::test]
    async fn advisory_r2_global_resume_pruning_uses_current_trust() -> Result<()> {
        let (dir, store, device, begin) = fixture().await?;
        let stale = store.with_current_pairing();
        store.unpair_then(device.clone(), |_| {}).await?;
        store
            .pair(device.clone(), Hash::new("cd".repeat(32))?)
            .await?;
        let path = prefix(&store, &device, &begin, dir.path().join("save")).await?;
        stale.maintain_partials().await?;
        assert!(path.exists());
        assert_eq!(store.partials()?.len(), 1);
        assert_eq!(
            store
                .resume_stage(device.clone(), begin.clone())
                .await?
                .unwrap()
                .1,
            2
        );
        store.purge_partial(device, begin.transfer_id).await?;
        Ok(())
    }
    #[tokio::test]
    async fn advisory_r3_unavailable_root_keeps_deferred_resume_deletion() -> Result<()> {
        let (dir, store, device, begin) = fixture().await?;
        let root = dir.path().join("save");
        let path = prefix(&store, &device, &begin, root.clone()).await?;
        let offline = dir.path().join("offline");
        fs::rename(&root, &offline)?;
        store
            .purge_partial(device.clone(), begin.transfer_id.clone())
            .await?;
        assert_eq!(
            store.partials()?.len(),
            1,
            "inaccessible stage remains accounted for"
        );
        super::super::tests::close_for_restart(store).await?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        assert_eq!(
            store.partials()?.len(),
            1,
            "deferred deletion survives restart"
        );
        fs::rename(&offline, &root)?;
        store.maintain_partials().await?;
        assert!(!path.exists());
        assert!(store.partials()?.is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn advisory_r5_cancel_fences_checkpoint_before_blocking_admission() -> Result<()> {
        let (dir, store, device, begin) = fixture().await?;
        let stage = store
            .create_stage(dir.path().join("save"), "a.txt".into(), begin.size)
            .await?;
        let mut permits = Vec::new();
        for _ in 0..crate::work::BLOCKING_LIMIT {
            permits.push(store.reserve().await);
        }
        let checkpoint = store.checkpoint(
            device.clone(),
            begin.clone(),
            stage.clone(),
            0,
            Sha256::new(),
            true,
        );
        tokio::pin!(checkpoint);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                checkpoint.as_mut(),
                cx
            )))
            .await
            .is_pending()
        );
        store.signal_cancel(&device, &begin.transfer_id);
        drop(permits);
        assert!(
            checkpoint.await.is_err(),
            "cancelled checkpoint must not create retained state"
        );
        assert!(store.partials()?.is_empty());
        store.discard_stage(stage).await?;
        Ok(())
    }
    #[tokio::test]
    async fn advisory_r6_resumed_suffix_respects_destination_space() -> Result<()> {
        let (dir, store, device, mut begin) = fixture().await?;
        let path = prefix(&store, &device, &begin, dir.path().join("save")).await?;
        begin.size = u64::MAX;
        let mut p = store.partials()?.remove(0);
        p.begin = begin.clone();
        store.locked(|c| {
            c.execute(
                "UPDATE file_partials SET record=?",
                [serde_json::to_string(&p)?],
            )?;
            Ok(())
        })?;
        let error = store
            .resume_stage(device, begin)
            .await
            .err()
            .context("insufficient suffix space must reject resume")?;
        assert_eq!(save_failure_code(&error), NackCode::DiskFull);
        assert!(
            path.exists(),
            "retain valid prefix after admission rejection"
        );
        Ok(())
    }
    #[tokio::test]
    async fn checkpoint_reconciles_tail_restart_and_rehashes_prefix() -> Result<()> {
        let (dir, store, device, begin) = fixture().await?;
        let root = dir.path().join("save");
        let path = prefix(&store, &device, &begin, root.clone()).await?;
        fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"uncheckpointed")?;
        let db = dir.path().join("state.sqlite3");
        super::super::tests::close_for_restart(store).await?;
        let store = Store::open(&db)?;
        assert!(store.clean_staging(vec![root]).await?.is_empty());
        assert_eq!(fs::read(&path)?, b"ab");
        let (stage, offset, mut hash) = store
            .resume_stage(device.clone(), begin.clone())
            .await?
            .unwrap();
        assert_eq!(offset, 2);
        hash.update(b"c");
        assert_eq!(hex::encode(hash.finalize()), begin.sha256.as_str());
        drop(stage);
        store.purge_partial(device, begin.transfer_id).await?;
        assert!(!path.exists());
        Ok(())
    }
    #[tokio::test]
    async fn checkpoint_corruption_and_short_file_restart_at_zero() -> Result<()> {
        for data in [b"xx".as_slice(), b"a"] {
            let (dir, store, device, begin) = fixture().await?;
            let path = prefix(&store, &device, &begin, dir.path().join("save")).await?;
            fs::write(&path, data)?;
            assert!(
                store
                    .resume_stage(device.clone(), begin.clone())
                    .await?
                    .is_none()
            );
            assert!(!path.exists());
            assert!(store.partials()?.is_empty());
        }
        Ok(())
    }
    #[tokio::test]
    async fn expiry_rollback_and_budget_do_not_evict_live_lease() -> Result<()> {
        for kind in ["ttl", "rollback", "budget"] {
            let (dir, store, device, begin) = fixture().await?;
            let path = prefix(&store, &device, &begin, dir.path().join("save")).await?;
            let (stage, _, _) = store
                .resume_stage(device.clone(), begin.clone())
                .await?
                .unwrap();
            let mut p = store.partials()?.remove(0);
            match kind {
                "ttl" => p.progress = crate::now_ms().saturating_sub(TTL + 1),
                "budget" => p.offset = BYTE_LIMIT + 1,
                _ => {}
            }
            store.locked(|c| {
                c.execute(
                    "UPDATE file_partials SET record=?",
                    [serde_json::to_string(&p)?],
                )?;
                if kind == "rollback" {
                    c.execute(
                        "INSERT OR REPLACE INTO resume_clock VALUES(1,?)",
                        [(crate::now_ms() + ROLLBACK + 10000) as i64],
                    )?;
                }
                Ok(())
            })?;
            store.resume.lock().unwrap().progress.clear();
            store.prune_partials()?;
            assert!(path.exists());
            drop(stage);
            if kind == "rollback" {
                store.locked(|c| {
                    c.execute(
                        "UPDATE resume_clock SET wall=?",
                        [(crate::now_ms() + ROLLBACK + 10000) as i64],
                    )?;
                    Ok(())
                })?;
            }
            store.prune_partials()?;
            assert!(!path.exists(), "{kind}");
        }
        Ok(())
    }
    #[tokio::test]
    async fn metadata_conflicts_cancel_and_revoke_are_binding_scoped() -> Result<()> {
        let (dir, store, device, begin) = fixture().await?;
        let path = prefix(&store, &device, &begin, dir.path().join("save")).await?;
        let mut changed = begin.clone();
        changed.file_name = "other".into();
        assert!(store.resume_stage(device.clone(), changed).await.is_err());
        let mut legacy = begin.clone();
        legacy.resume = None;
        let (stage, offset, _) = store.resume_stage(device.clone(), legacy).await?.unwrap();
        assert_eq!(offset, 2);
        drop(stage);
        store
            .purge_partial(device.clone(), begin.transfer_id.clone())
            .await?;
        store
            .purge_partial(device.clone(), begin.transfer_id.clone())
            .await?;
        assert!(!path.exists());
        assert!(store.resume.lock().unwrap().progress.is_empty());
        let path = prefix(&store, &device, &begin, dir.path().join("save")).await?;
        store.unpair_then(device, |_| {}).await?;
        assert!(!path.exists());
        Ok(())
    }
    #[tokio::test]
    async fn partial_id_reserves_the_operation_namespace() -> Result<()> {
        let (dir, store, device, begin) = fixture().await?;
        prefix(&store, &device, &begin, dir.path().join("save")).await?;
        let text = Clipboard {
            transfer_id: begin.transfer_id,
            text: "conflict".into(),
            created_at: 0,
            origin: ClipboardOrigin::Clipboard,
            hlc: None,
            source_device: None,
            sensitive: false,
        };
        let error = store
            .claim_text(device, text)
            .await
            .err()
            .context("partial ID must conflict")?;
        assert_eq!(save_failure_code(&error), NackCode::IdConflict);
        Ok(())
    }
    #[tokio::test]
    async fn partial_job_budget_evicts_oldest_inactive_peer_job() -> Result<()> {
        let (dir, store, device, begin) = fixture().await?;
        let mut paths = Vec::new();
        for n in 0..9 {
            let mut item = begin.clone();
            item.transfer_id = Id::from_bytes([n + 10; 16]);
            paths.push(prefix(&store, &device, &item, dir.path().join("save")).await?);
        }
        assert_eq!(store.partials()?.len(), 8);
        assert!(!paths[0].exists());
        assert!(paths[1..].iter().all(|p| p.exists()));
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn reconciliation_never_removes_substituted_user_file() -> Result<()> {
        let (dir, store, device, begin) = fixture().await?;
        let root = dir.path().join("save");
        let path = prefix(&store, &device, &begin, root.clone()).await?;
        fs::rename(&path, dir.path().join("original"))?;
        fs::write(&path, b"user data")?;
        assert!(store.resume_stage(device, begin).await?.is_none());
        // Repeat reconciliation after the catalogue candidate has been discarded.
        for _ in 0..2 {
            store
                .blocking({
                    let root = root.clone();
                    move |s| s.reconcile_resume_folder(&Folder::open(&root, false)?)
                })
                .await?;
        }
        assert_eq!(fs::read(path)?, b"user data");
        Ok(())
    }
}
