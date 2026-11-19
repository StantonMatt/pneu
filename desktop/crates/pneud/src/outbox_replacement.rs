use super::*;
use serde_json::json;

impl Outbox {
    /// Explicit Retry is a fresh send from immutable bytes, never a rebinding
    /// of a queued delivery or reuse of its operation/transfer/receipt identity.
    pub(crate) async fn retry_replaced(
        &self,
        operation: Id,
        artifact: Option<Id>,
        old: Id,
        eligible: Vec<Authority>,
        source_device: Id,
    ) -> Result<Option<Id>> {
        // Ordinary Retry must not wait behind unrelated large source copies.
        let (op, item, peer) = (operation.clone(), artifact.clone(), old.clone());
        let replaced = self.store.call(move |c| {
            Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM outbox_artifacts a JOIN outbox_deliveries d ON d.artifact=a.id WHERE a.operation=? AND (? IS NULL OR a.id=?) AND d.device=? AND d.state='cancelled' AND d.error=? AND a.staged=1)", params![op.as_str(),item.as_ref().map(Id::as_str),item.as_ref().map(Id::as_str),peer.as_str(),replacement::REPLACED], |r| r.get::<_,bool>(0))?)
        }).await?;
        if !replaced {
            return Ok(None);
        }
        let admission = self.admission.clone().lock_owned().await;
        let this = self.clone();
        let jobs = self.store.call(move |c| {
            let rows = {
                let mut q = c.prepare("SELECT a.meta,a.path,d.generation,d.fingerprint FROM outbox_artifacts a JOIN outbox_deliveries d ON d.artifact=a.id WHERE a.operation=? AND (? IS NULL OR a.id=?) AND d.device=? AND d.state='cancelled' AND d.error=? AND a.staged=1 ORDER BY a.rowid")?;
                q.query_map(params![operation.as_str(),artifact.as_ref().map(Id::as_str),artifact.as_ref().map(Id::as_str),old.as_str(),replacement::REPLACED], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut jobs = Vec::new();
            for (raw, path, generation, pin) in rows {
                let old_authority = Authority { device: old.clone(), generation: generation as u64, fingerprint: Hash::new(pin)? };
                let new = {
                    let mut q = c.prepare("SELECT payload FROM local_pairing_intents WHERE old_device=? AND phase='done'")?;
                    let mut target = None;
                    for row in q.query_map([old.as_str()], |r| r.get::<_,String>(0))? {
                        let offer: replacement::Offer = serde_json::from_str(&row?)?;
                        if offer.old == old_authority && this.authorized(c, &offer.new)? { target = Some(offer.new); break; }
                    }
                    target.context("replacement device is no longer paired")?
                };
                ensure!(eligible.contains(&new), "replacement device is unavailable");
                jobs.push((serde_json::from_str::<FileBegin>(&raw)?, path, old.clone(), new));
            }
            Ok(jobs)
        }).await?;
        if jobs.is_empty() {
            return Ok(None);
        }
        let paths: Vec<PathBuf> = jobs
            .iter()
            .map(|(begin, path, _, _)| {
                path.as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(&begin.file_name))
            })
            .collect();
        let mut recipients = Vec::new();
        for (_, _, _, new) in &jobs {
            if !recipients.contains(new) {
                recipients.push(new.clone());
            }
        }
        // Use ordinary new-send registration to freeze fresh image stamps in
        // selection order before any slow copying. Only the source bytes differ.
        let (operation, _) = self
            .start_operation_originated(paths.clone(), recipients, Vec::new(), None, source_device)
            .await?;
        let retry_operation = operation.clone();
        let this = self.clone();
        let result = self.file_io(move || {
            let _admission = admission;
            let stamps: HashMap<PathBuf, ArtifactStamp> = this.store.locked(|c| {
                let raw: Option<String> = c.query_row("SELECT stamp FROM outbox_operations WHERE id=?", [retry_operation.as_str()], |r| r.get(0))?;
                Ok(raw.map(|raw| serde_json::from_str(&raw)).transpose()?.unwrap_or_default())
            })?;
            let result = (|| {
                let mut target = None;
                for (mut begin, path, old, new) in jobs {
                    let source_path = path.as_ref().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&begin.file_name));
                    let previous = begin.transfer_id.clone();
                    let id = Id::new(crate::random_hex(16))?;
                    begin.transfer_id = id.clone();
                    // A folder named *.png is still a folder, never clipboard media.
                    let stamp = stamps.get(&source_path).filter(|_| begin.folder.is_none());
                    begin.created_at = stamp.map_or_else(crate::now_ms, |s| s.created_at);
                    begin.hlc = stamp.map(|s| s.hlc);
                    begin.source_device = stamp.map(|s| s.source_device.clone());
                    let mut source = platform_fs::open_read_at(&this.folder.file, previous.as_str())?;
                    crate::platform::private::check(&source)?;
                    // Folder spools contain only the frozen manifest; their
                    // content stays in place under the captured source catalogue.
                    let staged_size = begin.folder.as_ref().map_or(begin.size, |folder| folder.manifest_len);
                    this.store.locked(|c| Self::capacity(c, staged_size, &[]))?;
                    ensure!(this.folder.space()?.saturating_sub(staged_size) >= this.reserve, "outbox disk reserve");
                    let mut file = platform_fs::create_private_at(&this.folder.file, id.as_str(), platform_fs::Access::ReadWrite)?;
                    crate::platform::private::created(&file)?;
                    let result = (|| {
                        ensure!(std::io::copy(&mut source, &mut file)? == staged_size, "retained file changed");
                        platform_fs::set_read_only(&file)?;
                        file.sync_all()?;
                        this.folder.file.sync_all()?;
                        this.store.locked(|c| {
                            let tx = c.transaction()?;
                            ensure!(this.authorized(&tx, &new)?, "replacement pairing changed");
                            ensure!(this.operation_allows(&tx, &retry_operation, &new)?, "operation cancelled");
                            ensure!(Self::peer_has_capacity(&tx, staged_size, &new)?, "device outbox is full");
                            ensure!(tx.execute("UPDATE outbox_deliveries SET error=NULL WHERE artifact=? AND device=? AND state='cancelled' AND error=?", params![previous.as_str(),old.as_str(),replacement::REPLACED])? == 1, "replacement retry already handled");
                            Self::capacity(&tx, staged_size, std::slice::from_ref(&new))?;
                            tx.execute("INSERT INTO outbox_artifacts(id,operation,meta,path) VALUES(?,?,?,?)", params![id.as_str(),retry_operation.as_str(),serde_json::to_string(&begin)?,path])?;
                            if begin.folder.is_some() {
                                ensure!(tx.execute("INSERT INTO outbox_folders(id,details,sources) SELECT ?,details,sources FROM outbox_folders WHERE id=?", params![id.as_str(),previous.as_str()])? == 1, "retained folder catalogue missing");
                            }
                            tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,?,?)", params![id.as_str(),new.device.as_str(),new.generation as i64,new.fingerprint.as_str()])?;
                            tx.execute("UPDATE outbox_operations SET accepted=json_insert(accepted,'$[#]',json_object('path',?,'transfer_id',?)) WHERE id=?", params![source_path.to_str(),id.as_str(),retry_operation.as_str()])?;
                            tx.commit()?;
                            Ok(())
                        })
                    })();
                    if let Err(error) = result {
                        let _ = crate::storage::remove_file(&this.folder.file, id.as_str(), &file);
                        return Err(error);
                    }
                    // A later item can fail after this one committed. Publish each
                    // accepted resend so it remains visible and sendable then too.
                    this.refresh()?;
                    // Release consumed sources before checking capacity for the next copy.
                    this.collect()?;
                    target = Some(new.device);
                }
                Ok(target)
            })();
            // Also collect if any later source, copy, or publication failed.
            this.collect()?;
            result
        }).await;
        let status = self.operation_status(operation.clone()).await?;
        let accepted: Vec<serde_json::Value> = serde_json::from_value(status["accepted"].clone())?;
        let rejected = if let Err(error) = &result {
            paths
                .iter()
                .filter(|path| !accepted.iter().any(|item| item["path"] == json!(path)))
                .map(|path| json!({"path":path,"reason":error.to_string()}))
                .collect()
        } else {
            Vec::new()
        };
        self.admission_result(operation, accepted, rejected, true)
            .await?;
        result
    }
}
