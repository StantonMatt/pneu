// Application-state vectors complement the shared strict wire vectors.
fn folder_vector(empty:bool)->(FileBegin,Vec<u8>) {
    let manifest=if empty {br#"{"entries":[]}"#.as_slice()} else {br#"{"entries":[{"path":"empty","type":"directory"},{"path":"a.txt","type":"file","size":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}]}"#.as_slice()};
    let mut bytes=manifest.to_vec();if !empty {bytes.extend_from_slice(b"abc");}
    let mut begin=test_begin(&bytes);begin.file_name=if empty {"Empty"} else {"Trip"}.into();begin.mime="application/vnd.pneu.folder".into();begin.resume=Some(true);begin.folder=Some(Box::new(pneu_proto::Folder {manifest_len:manifest.len() as u64}));(begin,bytes)
}
#[tokio::test]
async fn folder_vectors_offsets_empty_root_and_completion_replay()->Result<()> {
    for offset in [1,164,165,167,168] {
        let (_dir,core,device)=fixture(Arc::new(NoActions)).await?;let mut store=core.store.clone();store.file_resume=true;
        let (begin,bytes)=folder_vector(false);assert_eq!(begin.size,168);assert_eq!(begin.sha256.as_str(),"06b77a27b341ed4486baab8d2126e98a9ba3f818aa61c717fdfb2b23de8a44e0");
        let (tx,mut rx)=mpsc::channel(8);let mut incoming=None;
        core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::FileReady(FileReady{offset:0,..}))));
        core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data:bytes[..offset].to_vec()}),&mut incoming,&tx).await?;
        let state=incoming.take().unwrap();assert_eq!(state.receipt,None);
        let stage=state.stage.as_ref().unwrap().clone();store.checkpoint(device.clone(),begin.clone(),stage.clone(),offset as u64,state.hash.clone(),true).await?;
        drop(state);drop(stage);
        assert!(!core.config().save_path.join("Trip").exists(),"no partial tree published");
        core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&tx).await?;
        assert_eq!(rx.recv().await,Some(Message::FileReady(FileReady{transfer_id:begin.transfer_id.clone(),offset:offset as u64})));
        if offset<bytes.len() {core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:offset as u64,data:bytes[offset..].to_vec()}),&mut incoming,&tx).await?;}
        core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id.clone()}),&mut incoming,&tx).await?;
        assert!(matches!(rx.recv().await,Some(Message::Ack(Ack{deduplicated:false,..}))));
        let root=core.config().save_path.join("Trip");assert_eq!(std::fs::read(root.join("a.txt"))?,b"abc");assert!(root.join("empty").is_dir());
        std::fs::remove_dir_all(&root)?;
        core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::FileReady(FileReady{offset:168,..}))));
        core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id.clone()}),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::Ack(Ack{deduplicated:true,..}))));assert!(!root.exists());
        let (empty,data)=folder_vector(true);assert_eq!(empty.sha256.as_str(),"d801aa1fb7ddcc330a5e3173372ea6af4a3d08ec58074478e85aa5603e926658");
        core.receive_test(&store,&device,Message::FileBegin(empty.clone()),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::FileReady(_))));
        core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:empty.transfer_id.clone(),offset:0,data}),&mut incoming,&tx).await?;
        core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:empty.transfer_id}),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::Ack(_))));assert!(core.config().save_path.join("Empty").is_dir());core.shutdown_actions().await?;
    } Ok(())
}
#[tokio::test]
async fn folder_invalid_manifest_drains_and_entry_hash_mismatch_purges()->Result<()> {
    for malformed in [true,false] {
        let (_dir,core,device)=fixture(Arc::new(NoActions)).await?;let mut store=core.store.clone();store.file_resume=true;
        let (mut begin,mut bytes)=folder_vector(false);
        if malformed { let text=String::from_utf8(bytes)?;bytes=text.replace("a.txt","../xx").into_bytes(); } else { *bytes.last_mut().unwrap()=b'd'; }
        begin.sha256=Hash::new(hex::encode(Sha256::digest(&bytes)))?;
        let (tx,mut rx)=mpsc::channel(8);let mut incoming=None;
        core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::FileReady(_))));
        core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data:bytes}),&mut incoming,&tx).await?;
        if malformed {
            assert_eq!(hex::encode(incoming.as_ref().unwrap().hash.clone().finalize()), hex::encode(Sha256::digest([])), "failed writes do not advance the stream hash");
        }
        core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id.clone()}),&mut incoming,&tx).await?;
        assert!(matches!(rx.recv().await,Some(Message::Nack(n)) if n.code==if malformed {NackCode::UnsupportedKind} else {NackCode::HashMismatch}));
        assert!(store.receipt(device.clone(),begin.transfer_id.clone()).await?.is_none());assert!(!core.config().save_path.join("Trip").exists());
        core.shutdown_actions().await?;
    }Ok(())
}
#[tokio::test]
async fn folder_root_collision_cancel_and_legacy_refusal()->Result<()> {
    let (_dir,core,device)=fixture(Arc::new(NoActions)).await?;let mut store=core.store.clone();store.file_resume=true;
    let (begin,bytes)=folder_vector(false);let (tx,mut rx)=mpsc::channel(8);let mut incoming=None;
    std::fs::create_dir_all(core.config().save_path.join("Trip"))?;std::fs::write(core.config().save_path.join("Trip/keep"),b"user")?;
    core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&tx).await?;rx.recv().await;
    core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data:bytes}),&mut incoming,&tx).await?;
    core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id.clone()}),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::Ack(_))));
    assert_eq!(std::fs::read(core.config().save_path.join("Trip/keep"))?,b"user");assert_eq!(std::fs::read(core.config().save_path.join("Trip (2)/a.txt"))?,b"abc");
    let (mut other,bytes)=folder_vector(false);other.transfer_id=Id::from_bytes([29;16]);
    core.receive_test(&store,&device,Message::FileBegin(other.clone()),&mut incoming,&tx).await?;rx.recv().await;
    core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:other.transfer_id.clone(),offset:0,data:bytes[..167].to_vec()}),&mut incoming,&tx).await?;
    let stage_path=incoming.as_ref().unwrap().stage.as_ref().unwrap().path()?;
    core.receive_test(&store,&device,Message::Cancel(Cancel{transfer_id:other.transfer_id}),&mut incoming,&tx).await?;assert!(incoming.is_none());assert!(!stage_path.exists());assert!(rx.try_recv().is_err());
    store.file_resume=false;assert!(core.receive_test(&store,&device,Message::FileBegin(begin),&mut incoming,&tx).await.is_err());assert!(rx.try_recv().is_err());core.shutdown_actions().await
}

#[tokio::test]
async fn folder_sender_refuses_legacy_before_any_frame()->Result<()> {
    for caps in [vec!["files"],vec!["files","file_resume_v1"],vec!["files","folder_v1"]] {
        let (dir,core,device)=fixture(Arc::new(NoActions)).await?;let _connection=admit_ui_test_phone(&core,&device,1)?;
        core.sessions.lock().unwrap().sessions.get_mut(&device).unwrap().capabilities=caps.iter().map(|c|(*c).to_owned()).collect();
        let authority=file_test_peer(&core,&device).await?;let (mut begin,bytes)=folder_vector(false);let path=dir.path().join("stream.bin");std::fs::write(&path,&bytes)?;
        core.outbox.admit(Id::from_bytes([80;16]),path,vec![authority.clone()],1000).await?;
        let mut delivery=core.outbox.claim(device,authority.generation,1).await?.unwrap();let reader=core.outbox.reader(delivery.clone()).await?;begin.transfer_id=delivery.begin.transfer_id.clone();delivery.begin=begin;
        let (send,ready)=tokio::sync::oneshot::channel();send.send(Some(0)).unwrap();
        let work=FileWrite{delivery,connection:1,mesh:false,file:Some(reader.into()),ready:Some(ready),receipt_ready:Arc::new(AtomicBool::new(false))};
        let (_p,mut priority)=mpsc::channel(8);let (_n,mut normal)=mpsc::channel(8);let (_t,mut text)=mpsc::channel(8);let (reports,_)=mpsc::channel(8);let mut wire=Vec::new();
        assert!(transmit_file(&core,&mut wire,work,&mut priority,&mut normal,&mut text,&reports).await.is_err());assert!(wire.is_empty(),"no disguised file BEGIN or child bytes to a legacy receiver");core.shutdown_actions().await?;
    }Ok(())
}

#[tokio::test]
async fn folder_reserved_roots_survive_staging_reconciliation()->Result<()> {
    for name in [".pneu-staging",".PNEU-STAGING",".pneu-stage-abcdef0123456789abcdef0123456789",".pneu-cache-abcdef0123456789abcdef0123456789-Trip"] {
        let (_dir,core,device)=fixture(Arc::new(NoActions)).await?;let mut store=core.store.clone();store.file_resume=true;
        let (mut begin,data)=folder_vector(false);begin.file_name=name.into();let (tx,mut rx)=mpsc::channel(8);let mut incoming=None;
        core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::FileReady(_))));
        core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data}),&mut incoming,&tx).await?;
        core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id}),&mut incoming,&tx).await?;assert!(matches!(rx.recv().await,Some(Message::Ack(_))));
        let root=core.config().save_path;let published=root.join(format!("_{name}"));assert_eq!(std::fs::read(published.join("a.txt"))?,b"abc");
        store.blocking(move |_|crate::storage::clean_legacy_staging(&crate::storage::Folder::open(&root,false)?)).await?;
        assert_eq!(std::fs::read(published.join("a.txt"))?,b"abc","user folder must not enter the orphan cleanup namespace");core.shutdown_actions().await?;
    }Ok(())
}

/// Bytes held under a save root's private staging directory, split into
/// stream data (stage files and tree files) and the small mapping journal.
fn staging_usage(root: &std::path::Path) -> Result<(u64, u64)> {
    fn walk(path: &std::path::Path, data: &mut u64, map: &mut u64) -> Result<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.is_dir() {
                walk(&entry.path(), data, map)?;
            } else if entry.file_name().to_string_lossy().ends_with("-map") {
                *map += meta.len();
            } else {
                *data += meta.len();
            }
        }
        Ok(())
    }
    let (mut data, mut map) = (0, 0);
    let staging = root.join(crate::storage::STAGING_DIR);
    if staging.exists() {
        walk(&staging, &mut data, &mut map)?;
    }
    Ok((data, map))
}

#[tokio::test]
async fn folder_receiver_temp_space_stays_about_one_times_the_stream() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let mut store = core.store.clone();
    store.file_resume = true;
    let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let file = |path: &str, data: &[u8]| -> Result<pneu_proto::FolderEntry> {
        Ok(pneu_proto::FolderEntry {
            path: path.into(),
            entry_type: pneu_proto::FolderEntryType::File,
            size: Some(data.len() as u64),
            sha256: Some(Hash::new(hex::encode(Sha256::digest(data)))?),
        })
    };
    let manifest = serde_json::to_vec(&pneu_proto::FolderManifest {
        entries: vec![file("big.bin", &big)?, file("small.txt", b"abc")?],
    })?;
    let mut bytes = manifest.clone();
    bytes.extend_from_slice(&big);
    bytes.extend_from_slice(b"abc");
    let mut begin = test_begin(&bytes);
    begin.file_name = "Trip".into();
    begin.mime = "application/vnd.pneu.folder".into();
    begin.resume = Some(true);
    begin.folder = Some(Box::new(pneu_proto::Folder {
        manifest_len: manifest.len() as u64,
    }));
    let (tx, mut rx) = mpsc::channel(8);
    let mut incoming = None;
    core.receive_test(&store, &device, Message::FileBegin(begin.clone()), &mut incoming, &tx)
        .await?;
    assert!(matches!(rx.recv().await, Some(Message::FileReady(FileReady { offset: 0, .. }))));
    let save = core.config().save_path;
    let half = manifest.len() + 2 * 1024 * 1024 + 7;
    let mut offset = 0;
    for chunk in bytes[..half].chunks(pneu_proto::CHUNK_SIZE) {
        core.receive_test(
            &store,
            &device,
            Message::FileChunk(FileChunk {
                transfer_id: begin.transfer_id.clone(),
                offset: offset as u64,
                data: chunk.to_vec(),
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        offset += chunk.len();
        let (data, map) = staging_usage(&save)?;
        // Every received byte is stored exactly once: manifest bytes in the
        // stage file, content in the private tree. No whole-stream spool.
        assert_eq!(data, offset as u64, "temporary data at offset {offset}");
        assert_eq!(map, 0, "the catalogue is the only mapping journal");
    }
    assert_eq!(incoming.as_ref().unwrap().receipt, None);
    core.receive_test(
        &store,
        &device,
        Message::FileChunk(FileChunk {
            transfer_id: begin.transfer_id.clone(),
            offset: half as u64,
            data: bytes[half..].to_vec(),
        }),
        &mut incoming,
        &tx,
    )
    .await?;
    assert_eq!(staging_usage(&save)?.0, bytes.len() as u64);
    core.receive_test(&store, &device, Message::FileEnd(FileEnd { transfer_id: begin.transfer_id.clone() }), &mut incoming, &tx)
        .await?;
    assert!(matches!(rx.recv().await, Some(Message::Ack(Ack { deduplicated: false, .. }))));
    assert_eq!(std::fs::read(save.join("Trip/big.bin"))?, big);
    assert_eq!(std::fs::read(save.join("Trip/small.txt"))?, b"abc");
    assert_eq!(staging_usage(&save)?, (0, 0), "publication leaves no staging residue");
    core.shutdown_actions().await
}

#[tokio::test]
async fn folder_sender_file_changed_after_preparing_fails_cleanly() -> Result<()> {
    for before_begin in [true, false] {
        let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let _connection = admit_ui_test_phone(&core, &device, 1)?;
        core.sessions.lock().unwrap().sessions.get_mut(&device).unwrap().capabilities =
            ["files", "file_resume_v1", "folder_v1"].iter().map(|c| (*c).to_owned()).collect();
        let peer = device.clone();
        core.store
            .call(move |c| {
                c.execute("UPDATE peers SET role='computer' WHERE id=?", [peer.as_str()])?;
                Ok(())
            })
            .await?;
        let authority = file_test_peer(&core, &device).await?;
        let source = dir.path().join("Trip");
        std::fs::create_dir_all(&source)?;
        std::fs::write(source.join("a.bin"), vec![1u8; 600_000])?;
        std::fs::write(source.join("b.bin"), vec![2u8; 200_000])?;
        core.outbox
            .admit_selected(
                Id::from_bytes([81; 16]),
                source.clone(),
                vec![authority.clone()],
                pneu_proto::MAX_FILE_SIZE,
                core.identity.device_id.clone(),
            )
            .await?;
        let delivery = core.outbox.claim(device.clone(), authority.generation, 1).await?.unwrap();
        core.outbox.negotiate_resume(delivery.clone(), true).await?;
        let id = delivery.begin.transfer_id.clone();
        if before_begin {
            // Same size, new content: caught before any frame is sent.
            std::fs::write(source.join("b.bin"), vec![3u8; 200_000])?;
            let error = core.outbox.folder_reader(delivery.clone()).await.err().context("changed folder")?;
            assert!(error.downcast_ref::<crate::folders::SourceChanged>().is_some(), "{error:#}");
            core.outbox.fail_changed(delivery.clone()).await?;
        } else {
            // Changed after BEGIN while resuming inside the first file: the
            // resumed suffix is checked, then the changed file fails the send.
            let reader = core.outbox.folder_reader(delivery.clone()).await?;
            std::fs::write(source.join("b.bin"), vec![3u8; 200_000])?;
            let resume = delivery.begin.folder.as_ref().unwrap().manifest_len + 1000;
            let (send, ready) = tokio::sync::oneshot::channel();
            send.send(Some(resume)).unwrap();
            let work = FileWrite {
                delivery: delivery.clone(),
                connection: 1,
                mesh: false,
                file: Some(reader.into()),
                ready: Some(ready),
                receipt_ready: Arc::new(AtomicBool::new(false)),
            };
            let (_p, mut priority) = mpsc::channel(8);
            let (_n, mut normal) = mpsc::channel(8);
            let (_t, mut text) = mpsc::channel(8);
            let (reports, _) = mpsc::channel(8);
            let mut bytes = Vec::new();
            assert!(!transmit_file(&core, &mut bytes, work, &mut priority, &mut normal, &mut text, &reports).await?);
            let mut wire = bytes.as_slice();
            let mut messages = Vec::new();
            while let Some(m) = read_frame(&mut wire).await? {
                messages.push(m);
            }
            assert!(matches!(&messages[0], Message::FileBegin(b) if b.transfer_id == id));
            assert!(matches!(&messages[1], Message::FileChunk(c) if c.offset == resume));
            assert!(matches!(messages.last(), Some(Message::Cancel(c)) if c.transfer_id == id));
            assert!(!messages.iter().any(|m| matches!(m, Message::FileEnd(_))), "no END after a change");
            let sent: usize = messages
                .iter()
                .filter_map(|m| if let Message::FileChunk(c) = m { Some(c.data.len()) } else { None })
                .sum();
            assert!(sent > 0 && sent <= 599_000, "only the verified first file's suffix is sent: {sent}");
        }
        let row = core.ui_snapshot().transfers.into_iter().find(|t| t.transfer_id == id).context("row")?;
        assert_eq!(row.state, "failed");
        assert_eq!(row.error.as_deref(), Some("Folder changed after Preparing. Send the folder again."));
        assert!(core.outbox.change(row.operation.clone(),Some(id.clone()),Some(row.device.clone()),true).await.is_err(), "a canceled changed-folder ID cannot be requeued");
        let pending = core
            .store
            .call(move |c| Ok(c.query_row("SELECT count(*) FROM file_pending_cancels WHERE id=?", [id.as_str()], |r| r.get::<_, i64>(0))?))
            .await?;
        assert_eq!(pending, i64::from(before_begin), "only a CANCEL not yet flushed remains pending");
        core.shutdown_actions().await?;
    }
    Ok(())
}

#[tokio::test]
async fn folder_ten_thousand_entries_and_large_end_meet_receive_deadlines() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let mut store = core.store.clone(); store.file_resume = true;
    let empty_hash = Hash::new(hex::encode(Sha256::digest([])))?;
    let big = vec![0x57; 32 * 1024 * 1024];
    let mut entries: Vec<_> = (0..9998).map(|i| pneu_proto::FolderEntry {
        path: format!("empty-{i:05}"), entry_type: pneu_proto::FolderEntryType::File,
        size: Some(0), sha256: Some(empty_hash.clone()),
    }).collect();
    entries.push(pneu_proto::FolderEntry { path: "dir".into(), entry_type: pneu_proto::FolderEntryType::Directory, size: None, sha256: None });
    entries.push(pneu_proto::FolderEntry { path: "dir/big.bin".into(), entry_type: pneu_proto::FolderEntryType::File,
        size: Some(big.len() as u64), sha256: Some(Hash::new(hex::encode(Sha256::digest(&big)))?) });
    let manifest = serde_json::to_vec(&pneu_proto::FolderManifest { entries })?;
    let mut stream = manifest.clone(); stream.extend_from_slice(&big);
    let mut begin = test_begin(&stream); begin.file_name = "Many".into(); begin.mime = "application/vnd.pneu.folder".into();
    begin.resume = Some(true); begin.folder = Some(Box::new(pneu_proto::Folder { manifest_len: manifest.len() as u64 }));
    let (tx, mut rx) = mpsc::channel(8); let mut incoming = None;
    core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&tx).await?;
    assert!(matches!(rx.recv().await, Some(Message::FileReady(_))));
    let started = std::time::Instant::now();
    for (i, chunk) in stream.chunks(pneu_proto::CHUNK_SIZE).enumerate() {
        tokio::time::timeout(Duration::from_secs(60), core.receive_test(&store,&device,
            Message::FileChunk(FileChunk { transfer_id: begin.transfer_id.clone(), offset: (i*pneu_proto::CHUNK_SIZE) as u64, data: chunk.to_vec() }),&mut incoming,&tx)).await??;
    }
    let receive_elapsed = started.elapsed();
    assert_eq!(incoming.as_ref().unwrap().receipt, None);
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(120), core.receive_test(&store,&device,
        Message::FileEnd(FileEnd { transfer_id: begin.transfer_id }),&mut incoming,&tx)).await??;
    let end_elapsed = started.elapsed();
    assert!(matches!(rx.recv().await, Some(Message::Ack(_))));
    eprintln!("10,000 entries / 32 MiB: receive {receive_elapsed:?}, END {end_elapsed:?}");
    let root = core.config().save_path.join("Many");
    assert_eq!(std::fs::read_dir(&root)?.count(),9999);
    assert_eq!(std::fs::read(root.join("dir/big.bin"))?,big);
    assert_eq!(staging_usage(&core.config().save_path)?, (0,0));
    core.shutdown_actions().await
}
