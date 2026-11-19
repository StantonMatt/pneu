//! Folder-specific admission; transport, receipt and cancellation use the file lane.
use super::*;
impl Outbox {
    pub(super) fn admit_folder(
        &self,
        operation: &Id,
        path: &Path,
        mut recipients: Vec<Authority>,
        limit: u64,
    ) -> Result<(Id, Vec<RecipientRejection>)> {
        let mut rejected = Vec::new();
        // Files are sent in place, so queue capacity charges only the manifest.
        // The separate per-folder limit still bounds the full stream.
        self.store.locked(|c| {
            recipients.retain(|peer| {
                let eligible = self
                    .authorized(c, peer)
                    .and_then(|a| Ok(a && self.operation_allows(c, operation, peer)?));
                match eligible {
                    Ok(true) => true,
                    _ => {
                        rejected.push(RecipientRejection {
                            device: peer.device.clone(),
                            reason: "Pairing changed".into(),
                        });
                        false
                    }
                }
            });
            Ok(())
        })?;
        ensure!(!recipients.is_empty(), "No eligible devices");
        let unsupported = self.store.locked(|c| {
            let mut unsupported = HashMap::new();
            let mut phones = 0;
            for peer in &recipients {
                let (role, name): (String, String) = c.query_row(
                    "SELECT role,name FROM peers WHERE id=?",
                    [peer.device.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if role == "phone"
                    && self.supports_folders(&peer.device, peer.generation) != Some(true)
                {
                    phones += 1;
                    unsupported.insert(
                        peer.device.clone(),
                        (
                            "skipped".to_owned(),
                            "Can't receive folders yet".to_owned(),
                            name,
                        ),
                    );
                } else if self.supports_folders(&peer.device, peer.generation) == Some(false) {
                    unsupported.insert(
                        peer.device.clone(),
                        ("failed".to_owned(), "Needs a Pneu update".to_owned(), name),
                    );
                }
            }
            if phones == recipients.len() {
                let name = &unsupported[&recipients[0].device].2;
                anyhow::bail!("{name} can't receive folders yet");
            }
            Ok(unsupported)
        })?;
        let progress_at = Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(1));
        let update = |items, bytes, total| {
            let mut at = progress_at.lock().expect("folder progress");
            if at.elapsed() < std::time::Duration::from_millis(250) {
                return;
            }
            *at = std::time::Instant::now();
            let row = Transfer {
                operation: operation.clone(),
                transfer_id: operation.clone(),
                device: Id::from_bytes([0; 16]),
                file_name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                size: total,
                sent: bytes,
                state: "staging".into(),
                error: None,
                direction: outgoing_direction(),
                finished_at: None,
                path: Some(path.to_owned()),
                generation: 0,
                folder: Some(crate::folders::Details {
                    items,
                    ..Default::default()
                }),
            };
            self.preparing
                .lock()
                .expect("folder preparation")
                .insert(operation.clone(), row);
        };
        let result = (|| {
            #[cfg(test)]
            {
                let hook = self.source_hook.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook(path);
                }
            }
            let capture = crate::folders::capture(
                path,
                limit,
                &|| {
                    ensure!(!self.admission_cancelled(operation), "operation cancelled");
                    ensure!(
                        !self.stopping.load(std::sync::atomic::Ordering::SeqCst),
                        "daemon is stopping"
                    );
                    Ok(())
                },
                &update,
            )?;
            let size = capture.size;
            let id = Id::new(crate::random_hex(16))?;
            let _admission = self.admission.blocking_lock();
            // Files are sent in place; the frozen manifest is the only spool.
            let mut staged = platform_fs::create_private_at(
                &self.folder.file,
                id.as_str(),
                platform_fs::Access::ReadWrite,
            )?;
            crate::platform::private::created(&staged)?;
            let staged_result = (|| {
                ensure!(
                    self.folder.space()?
                        >= (capture.manifest.len() as u64).saturating_add(self.reserve),
                    platform_fs::no_space()
                );
                ensure!(!self.admission_cancelled(operation), "operation cancelled");
                staged.write_all(&capture.manifest)?;
                update(capture.details.items, size, size);
                let begin = FileBegin {
                    transfer_id: id.clone(),
                    file_name: path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .context("folder name must be UTF-8")?
                        .into(),
                    mime: "application/vnd.pneu.folder".into(),
                    size,
                    sha256: capture.sha256.clone(),
                    kind: FileKind::File,
                    created_at: crate::now_ms(),
                    source_media_id: None,
                    hlc: None,
                    source_device: None,
                    sensitive: false,
                    resume: Some(true),
                    folder: Some(Box::new(pneu_proto::Folder {
                        manifest_len: capture.manifest.len() as u64,
                    })),
                };
                pneu_proto::FolderManifest::parse(&capture.manifest, &begin)?;
                platform_fs::set_read_only(&staged)?;
                staged.sync_all()?;
                self.folder.file.sync_all()?;
                self.store.locked(|c| {
                    let tx=c.transaction()?;
                    ensure!(!self.admission_cancelled(operation),"operation cancelled");
                    let manifest_len = capture.manifest.len() as u64;
                    self.select_recipients(&tx, operation, manifest_len, &mut recipients, &mut rejected)?;
                    ensure!(!recipients.is_empty(), "No eligible devices");
                    Self::capacity(&tx, manifest_len, &recipients)?;
                    tx.execute("INSERT INTO outbox_artifacts(id,operation,meta,path) VALUES(?,?,?,?)",params![id.as_str(),operation.as_str(),serde_json::to_string(&begin)?,path.to_str().context("UTF-8 path")?])?;
                    tx.execute("INSERT INTO outbox_folders VALUES(?,?,?)",params![id.as_str(),serde_json::to_string(&capture.details)?,serde_json::to_string(&capture.sources)?])?;
                    for peer in &recipients {let (state,error)=unsupported.get(&peer.device).map(|(state,error,_)|(state.as_str(),Some(error.as_str()))).unwrap_or(("queued",None));
                        tx.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint,state,error) VALUES(?,?,?,?,?,?)",params![id.as_str(),peer.device.as_str(),peer.generation as i64,peer.fingerprint.as_str(),state,error])?;}
                    tx.execute("UPDATE outbox_operations SET accepted=json_insert(accepted,'$[#]',json_object('path',?,'transfer_id',?)) WHERE id=?",params![path.to_str(),id.as_str(),operation.as_str()])?;
                    for rejection in &rejected {
                        tx.execute("UPDATE outbox_operations SET rejected=json_insert(rejected,'$[#]',json_object('path',?,'device',?,'reason',?)) WHERE id=?",params![path.to_str(),rejection.device.as_str(),rejection.reason,operation.as_str()])?;
                    }
                    tx.commit()?;
                    for peer in &recipients {self.view.lock().expect("outbox view").push(Transfer {folder:Some(capture.details.clone()),operation:operation.clone(),transfer_id:id.clone(),device:peer.device.clone(),file_name:begin.file_name.clone(),size,sent:0,state:unsupported.get(&peer.device).map(|v|v.0.clone()).unwrap_or_else(||"queued".into()),error:unsupported.get(&peer.device).map(|v|v.1.clone()),generation:peer.generation,direction:outgoing_direction(),finished_at:None,path:Some(path.to_owned())});} Ok(())
                })
            })();
            if staged_result.is_err() {
                let _ = crate::storage::remove_file(&self.folder.file, id.as_str(), &staged);
            }
            if let Err(error) = staged_result {
                return Err(AdmissionFailure {
                    recipient_only: recipients.is_empty(),
                    reason: error.to_string(),
                    rejected,
                }
                .into());
            }
            Ok((id, rejected))
        })();
        self.preparing
            .lock()
            .expect("folder preparation")
            .remove(operation);
        result
    }
    /// Open a folder delivery's stream in place from its frozen manifest and
    /// captured source versions. A changed source fails with `SourceChanged`.
    pub(crate) async fn folder_reader(
        &self,
        delivery: Delivery,
    ) -> Result<crate::folders::FolderSource> {
        let this = self.clone();
        self.file_io(move || this.folder_source(&delivery)).await
    }
    fn folder_source(&self, delivery: &Delivery) -> Result<crate::folders::FolderSource> {
        ensure!(self.current(delivery), "file delivery cancelled");
        let (path, sources): (String, String) = self.store.locked(|c| {
            Ok(c.query_row(
                "SELECT a.path,f.sources FROM outbox_artifacts a JOIN outbox_folders f ON f.id=a.id WHERE a.id=?",
                [delivery.begin.transfer_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })?;
        let sources: crate::folders::Sources = serde_json::from_str(&sources)?;
        let mut file =
            platform_fs::open_read_at(&self.folder.file, delivery.begin.transfer_id.as_str())?;
        crate::platform::private::check(&file)?;
        let manifest_len = delivery
            .begin
            .folder
            .as_ref()
            .context("folder delivery")?
            .manifest_len;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() == manifest_len,
            "staged folder manifest changed"
        );
        let mut manifest = Vec::with_capacity(manifest_len as usize);
        file.read_to_end(&mut manifest)?;
        crate::folders::FolderSource::open(Path::new(&path), manifest, &delivery.begin, &sources)
    }
    /// Fail a folder whose files changed after Preparing, and have the
    /// receiver purge any partial copy (CANCEL now or on its next connection).
    pub(crate) async fn fail_changed(&self, delivery: Delivery) -> Result<()> {
        let this = self.clone();
        self.store
            .blocking(move |_| {
                this.store.locked(|c| {
                    let tx = c.transaction()?;
                    let changed = tx.execute("UPDATE outbox_deliveries SET state='failed',error=? WHERE artifact=? AND device=? AND generation=? AND session=? AND state='sending'",params![crate::folders::SourceChanged.to_string(),delivery.begin.transfer_id.as_str(),delivery.authority.device.as_str(),delivery.authority.generation as i64,delivery.session as i64])?;
                    if changed > 0 {
                        tx.execute("INSERT OR IGNORE INTO file_pending_cancels SELECT device,artifact,generation FROM outbox_deliveries WHERE artifact=? AND device=? AND generation=? AND resume=1",params![delivery.begin.transfer_id.as_str(),delivery.authority.device.as_str(),delivery.authority.generation as i64])?;
                    }
                    tx.commit()?;
                    Ok(())
                })?;
                this.refresh()
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture, id};
    use super::*;

    fn seed_capacity(q: &Outbox, peer: &Authority, bytes: u64, count: usize) -> Result<()> {
        q.store.locked(|c| {
            let tx = c.transaction()?;
            for n in 0..count {
                let artifact = Id::new(format!("{:032x}", 1000 + n))?;
                let begin = FileBegin {
                    transfer_id: artifact.clone(),
                    file_name: "existing.bin".into(),
                    mime: "application/octet-stream".into(),
                    size: if n == 0 { bytes } else { 0 },
                    sha256: Hash::new("ab".repeat(32))?,
                    kind: FileKind::File,
                    created_at: 0,
                    source_media_id: None,
                    hlc: None,
                    source_device: None,
                    sensitive: false,
                    resume: None,
                    folder: None,
                };
                tx.execute(
                    "INSERT INTO outbox_artifacts(id,operation,meta) VALUES(?,?,?)",
                    params![artifact.as_str(), id(80).as_str(), serde_json::to_string(&begin)?],
                )?;
                tx.execute(
                    "INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES(?,?,?,?)",
                    params![artifact.as_str(), peer.device.as_str(), peer.generation as i64, peer.fingerprint.as_str()],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    #[tokio::test]
    async fn folder_admission_isolates_full_peer_count_and_bytes() -> Result<()> {
        for by_bytes in [false, true] {
            let (dir, q, peers, _) = fixture().await?;
            for peer in &peers {
                q.note_folders(peer.device.clone(), peer.generation, true);
            }
            seed_capacity(
                &q,
                &peers[0],
                if by_bytes { PEER_BYTES } else { 0 },
                if by_bytes { 1 } else { PEER_DELIVERIES },
            )?;
            let folder = dir.path().join("Trip");
            fs::create_dir(&folder)?;
            fs::write(folder.join("a.txt"), b"abc")?;
            let (operation, _) = q
                .start_operation(vec![folder.clone()], peers.clone(), Vec::new(), None)
                .await?;
            let (artifact, rejected) = q
                .admit_selected(operation.clone(), folder, peers.clone(), 1000, id(99))
                .await?;
            assert_eq!(rejected.len(), 1);
            assert_eq!(rejected[0].device, peers[0].device);
            assert_eq!(rejected[0].reason, "device outbox is full");
            let rows = q.snapshot(&[]);
            let rows: Vec<_> = rows.iter().filter(|r| r.transfer_id == artifact).collect();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].device, peers[1].device);
            assert_eq!(rows[0].state, "waiting_for_device");
            let reply = q.operation_status(operation).await?;
            assert_eq!(reply["rejected"][0]["device"], peers[0].device.as_str());
            assert_eq!(reply["rejected"][0]["reason"], "device outbox is full");
        }
        Ok(())
    }

    #[tokio::test]
    async fn folder_admission_enforces_manifest_byte_capacity() -> Result<()> {
        for global in [false, true] {
            for fits in [false, true] {
                let (dir, q, peers, _) = fixture().await?;
                q.note_folders(peers[0].device.clone(), peers[0].generation, true);
                let folder = dir.path().join("Trip");
                fs::create_dir(&folder)?;
                fs::write(folder.join("a.txt"), b"abc")?;
                let capture = crate::folders::capture(&folder, 1000, &|| Ok(()), &|_, _, _| {})?;
                let manifest_len = capture.manifest.len() as u64;
                let cap = if global { GLOBAL_BYTES } else { PEER_BYTES };
                // At the exact limit the manifest fits, but the content would not.
                let bytes = cap - manifest_len + u64::from(!fits);
                seed_capacity(&q, &peers[usize::from(global)], bytes, 1)?;
                let result = q.admit(id(9), folder, vec![peers[0].clone()], 1000).await;
                if fits {
                    let artifact = result?;
                    let row = q
                        .snapshot(&[])
                        .into_iter()
                        .find(|r| r.transfer_id == artifact)
                        .unwrap();
                    assert_eq!(row.size, capture.size);
                    assert!(row.size > manifest_len);
                } else {
                    let error = result.unwrap_err();
                    assert_eq!(
                        error.to_string(),
                        if global {
                            "outbox is full"
                        } else {
                            "No eligible devices"
                        }
                    );
                    assert_eq!(fs::read_dir(dir.path().join("outbox"))?.count(), 0);
                    let count = q.store.locked(|c| {
                        Ok(
                            c.query_row("SELECT count(*) FROM outbox_artifacts", [], |r| {
                                r.get::<_, i64>(0)
                            })?,
                        )
                    })?;
                    assert_eq!(count, 1, "rejection must not publish a manifest");
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn folder_preparing_releases_admission_and_stops_on_shutdown() -> Result<()> {
        let (dir, q, peers, _) = fixture().await?;
        let computer = peers[0].device.clone();
        q.store
            .call(move |c| {
                c.execute(
                    "UPDATE peers SET role='computer' WHERE id=?",
                    [computer.as_str()],
                )?;
                Ok(())
            })
            .await?;
        let folder = dir.path().join("Trip");
        fs::create_dir(&folder)?;
        fs::write(folder.join("a"), b"abc")?;
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let barrier = Mutex::new(Some((entered, wait)));
        *q.source_hook.lock().unwrap() = Some(Arc::new(move |_| {
            let (entered, wait) = barrier.lock().unwrap().take().unwrap();
            entered.send(()).unwrap();
            wait.recv().unwrap();
        }));
        let prepare = {
            let q = q.clone();
            let peers = peers.clone();
            tokio::spawn(async move { q.admit(id(9), folder, vec![peers[0].clone()], 1000).await })
        };
        ready.await?;
        let file = dir.path().join("normal.txt");
        fs::write(&file, b"normal")?;
        tokio::time::timeout(
            Duration::from_secs(2),
            q.admit(id(10), file, vec![peers[1].clone()], 1000),
        )
        .await??;
        q.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        release.send(())?;
        let error = tokio::time::timeout(Duration::from_secs(2), prepare)
            .await??
            .err()
            .context("shutdown interrupts Preparing")?;
        assert_eq!(error.to_string(), "daemon is stopping");
        Ok(())
    }
    #[tokio::test]
    async fn folder_capacity_charges_manifest_and_early_disk_full_is_terminal() -> Result<()> {
        let (dir, q, peers, _) = fixture().await?;
        let computer = peers[0].device.clone();
        q.store
            .call(move |c| {
                c.execute(
                    "UPDATE peers SET role='computer' WHERE id=?",
                    [computer.as_str()],
                )?;
                Ok(())
            })
            .await?;
        let folder = dir.path().join("Trip");
        fs::create_dir(&folder)?;
        fs::write(folder.join("a"), b"abc")?;
        let artifact = q.admit(id(9), folder, vec![peers[0].clone()], 1000).await?;
        let a = artifact.clone();
        q.store
            .call(move |c| {
                c.execute(
                    "UPDATE outbox_artifacts SET meta=json_set(meta,'$.size',?) WHERE id=?",
                    params![pneu_proto::MAX_FILE_SIZE as i64, a.as_str()],
                )?;
                Ok(())
            })
            .await?;
        let file = dir.path().join("normal.txt");
        fs::write(&file, b"normal")?;
        q.admit(id(10), file, vec![peers[0].clone()], 1000).await?;
        // Restore the actual size before claiming, then model an early NACK.
        let a = artifact.clone();
        q.store.call(move |c| { c.execute("UPDATE outbox_artifacts SET meta=json_set(meta,'$.size',json_extract(meta,'$.folder.manifest_len')+3) WHERE id=?",[a.as_str()])?; Ok(()) }).await?;
        let delivery = q
            .claim(peers[0].device.clone(), peers[0].generation, 1)
            .await?
            .context("folder")?;
        assert_eq!(delivery.begin.transfer_id, artifact);
        q.receipt_phase(delivery, Some(NackCode::DiskFull), true)
            .await?;
        let row = q
            .snapshot(&[])
            .into_iter()
            .find(|t| t.transfer_id == artifact)
            .unwrap();
        assert_eq!(row.state, "failed");
        assert!(row.error.as_deref().unwrap().starts_with("Needs "));
        Ok(())
    }
    #[tokio::test]
    async fn folder_outbox_skips_phone_keeps_computer_and_releases_shared_spool() -> Result<()> {
        let (dir, q, peers, _) = fixture().await?;
        let computer = peers[1].device.clone();
        let phone = peers[0].device.clone();
        q.store
            .call(move |c| {
                c.execute(
                    "UPDATE peers SET role='computer' WHERE id=?",
                    [computer.as_str()],
                )?;
                Ok(())
            })
            .await?;
        let folder = dir.path().join("Trip");
        fs::create_dir_all(folder.join("empty"))?;
        fs::write(folder.join("a.txt"), b"abc")?;
        let transfer = q
            .admit_selected(
                id(9),
                folder,
                peers.clone(),
                pneu_proto::MAX_FILE_SIZE,
                id(99),
            )
            .await?
            .0;
        let rows = q.snapshot(&[]);
        assert_eq!(
            rows.iter().find(|r| r.device == phone).unwrap().state,
            "skipped"
        );
        assert!(q.claim(phone, peers[0].generation, 1).await?.is_none());
        let delivery = q
            .claim(peers[1].device.clone(), peers[1].generation, 1)
            .await?
            .context("computer folder delivery")?;
        assert!(delivery.begin.folder.is_some());
        let spool = q.folder.path()?.join(transfer.as_str());
        assert!(spool.exists());
        q.receipt(delivery, None).await?;
        assert!(
            !spool.exists(),
            "skipped phones do not retain a shared source forever"
        );
        Ok(())
    }
    #[tokio::test]
    async fn folder_outbox_phone_only_refuses_and_legacy_computer_is_retryable() -> Result<()> {
        let (dir, q, peers, _) = fixture().await?;
        let folder = dir.path().join("Trip");
        fs::create_dir(&folder)?;
        fs::write(folder.join("x"), b"x")?;
        let error = q
            .admit_selected(
                id(9),
                folder.clone(),
                vec![peers[0].clone()],
                pneu_proto::MAX_FILE_SIZE,
                id(99),
            )
            .await
            .err()
            .context("phone refusal")?;
        assert!(error.to_string().ends_with("can't receive folders yet"));
        let computer = peers[1].device.clone();
        q.store
            .call(move |c| {
                c.execute(
                    "UPDATE peers SET role='computer' WHERE id=?",
                    [computer.as_str()],
                )?;
                Ok(())
            })
            .await?;
        q.note_folders(peers[1].device.clone(), peers[1].generation, false);
        q.admit_selected(
            id(9),
            folder,
            vec![peers[1].clone()],
            pneu_proto::MAX_FILE_SIZE,
            id(99),
        )
        .await?;
        let row = q
            .snapshot(&[])
            .into_iter()
            .find(|r| r.device == peers[1].device)
            .context("failed folder")?;
        assert_eq!(row.state, "failed");
        assert_eq!(row.error.as_deref(), Some("Needs a Pneu update"));
        q.note_folders(peers[1].device.clone(), peers[1].generation, true);
        q.change(
            row.operation,
            Some(row.transfer_id),
            Some(peers[1].device.clone()),
            true,
        )
        .await?;
        assert!(
            q.claim(peers[1].device.clone(), peers[1].generation, 1)
                .await?
                .is_some()
        );
        Ok(())
    }
}
