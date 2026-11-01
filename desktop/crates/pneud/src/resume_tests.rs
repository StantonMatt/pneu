// Focused application-state tests for the section 8.1 wire vectors.
#[tokio::test]
async fn resume_vectors_middle_full_completion_and_legacy_without_ready() -> Result<()> {
    let (_dir,core,device)=fixture(Arc::new(NoActions)).await?;
    let mut store=core.store.clone();store.file_resume=true;
    let mut begin=test_begin(b"abc");begin.resume=Some(true);
    let (input,rx)=mpsc::channel(4);let (output,mut replies)=mpsc::channel(8);
    let (close,_)=watch::channel(None);
    let worker=tokio::spawn(core.clone().file_loop_test(store.clone(),device.clone(),rx,output.clone(),close));
    input.send(Message::FileBegin(begin.clone())).await?;
    assert!(matches!(replies.recv().await,Some(Message::FileReady(FileReady{offset:0,..}))));
    input.send(Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data:b"ab".to_vec()})).await?;
    drop(input);worker.await??;
    let mut incoming=None;
    core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&output).await?;
    assert_eq!(replies.recv().await,Some(Message::FileReady(FileReady{transfer_id:begin.transfer_id.clone(),offset:2})));
    core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:2,data:b"c".to_vec()}),&mut incoming,&output).await?;
    core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id.clone()}),&mut incoming,&output).await?;
    assert!(matches!(replies.recv().await,Some(Message::Ack(Ack{deduplicated:false,..}))));
    core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&output).await?;
    assert!(matches!(replies.recv().await,Some(Message::FileReady(FileReady{offset:3,..}))));
    core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id.clone()}),&mut incoming,&output).await?;
    assert!(matches!(replies.recv().await,Some(Message::Ack(Ack{deduplicated:true,..}))));
    begin.resume=None;
    core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&output).await?;
    assert!(replies.try_recv().is_err(),"legacy BEGIN emits no READY even on a capable session");
    core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data:b"abc".to_vec()}),&mut incoming,&output).await?;
    core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id}),&mut incoming,&output).await?;
    assert!(matches!(replies.recv().await,Some(Message::Ack(Ack{deduplicated:true,..}))));
    core.shutdown_actions().await
}
#[tokio::test]
async fn resume_integrity_failure_cancel_conflict_and_zero_file() -> Result<()> {
    let (_dir,core,device)=fixture(Arc::new(NoActions)).await?;
    let mut store=core.store.clone();store.file_resume=true;
    let (output,mut replies)=mpsc::channel(8);let mut incoming=None;
    let mut begin=test_begin(b"abc");begin.resume=Some(true);
    for size_mismatch in [false,true] {
        core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&output).await?;
        assert!(matches!(replies.recv().await,Some(Message::FileReady(FileReady{offset:0,..}))));
        core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data:if size_mismatch{b"a".to_vec()}else{b"bad".to_vec()}}),&mut incoming,&output).await?;
        core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:begin.transfer_id.clone()}),&mut incoming,&output).await?;
        let expected=if size_mismatch{NackCode::SizeMismatch}else{NackCode::HashMismatch};
        assert!(matches!(replies.recv().await,Some(Message::Nack(n)) if n.code==expected&&n.retryable));
    }
    core.receive_test(&store,&device,Message::FileBegin(begin.clone()),&mut incoming,&output).await?;
    assert!(matches!(replies.recv().await,Some(Message::FileReady(FileReady{offset:0,..}))));
    core.receive_test(&store,&device,Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data:b"ab".to_vec()}),&mut incoming,&output).await?;
    let path=incoming.as_ref().unwrap().stage.as_ref().unwrap().path()?;
    core.receive_test(&store,&device,Message::Cancel(Cancel{transfer_id:begin.transfer_id.clone()}),&mut incoming,&output).await?;
    assert!(incoming.is_none());assert!(!path.exists());assert!(replies.try_recv().is_err());
    core.receive_test(&store,&device,Message::Cancel(Cancel{transfer_id:begin.transfer_id.clone()}),&mut incoming,&output).await?;
    let mut empty=test_begin(b"");empty.resume=Some(true);
    core.receive_test(&store,&device,Message::FileBegin(empty.clone()),&mut incoming,&output).await?;
    assert!(matches!(replies.recv().await,Some(Message::FileReady(FileReady{offset:0,..}))));
    core.receive_test(&store,&device,Message::FileEnd(FileEnd{transfer_id:empty.transfer_id.clone()}),&mut incoming,&output).await?;
    assert!(matches!(replies.recv().await,Some(Message::Ack(_))));
    empty.file_name="different".into();
    core.receive_test(&store,&device,Message::FileBegin(empty),&mut incoming,&output).await?;
    assert!(matches!(replies.recv().await,Some(Message::Nack(Nack{code:NackCode::IdConflict,retryable:false,..}))));
    assert!(incoming.is_none());core.shutdown_actions().await
}

#[tokio::test]
async fn resume_sender_ready_offsets_early_nack_and_legacy_wire() -> Result<()> {
    for selected in [Some(Some(0)),Some(Some(2)),Some(Some(3)),Some(None),None] {
        let (dir,core,device)=fixture(Arc::new(NoActions)).await?;
        let _connection=admit_ui_test_phone(&core,&device,1)?;
        core.sessions.lock().unwrap().sessions.get_mut(&device).unwrap().capabilities.push("files".into());
        let authority=file_test_peer(&core,&device).await?;
        let source=dir.path().join("abc.txt");std::fs::write(&source,b"abc")?;
        core.outbox.admit(Id::from_bytes([80;16]),source,vec![authority.clone()],100).await?;
        let delivery=core.outbox.claim(device,authority.generation,1).await?.unwrap();
        let file=core.outbox.reader(delivery.clone()).await?;
        core.outbox.negotiate_resume(delivery.clone(),selected.is_some()).await?;
        let ready=selected.map(|offset|{let (tx,rx)=tokio::sync::oneshot::channel();tx.send(offset).unwrap();rx});
        let work=FileWrite{delivery:delivery.clone(),connection:1,mesh:false,file:Some(file),ready,receipt_ready:Arc::new(AtomicBool::new(false))};
        let (_p,mut priority)=mpsc::channel(8);let (_n,mut normal)=mpsc::channel(8);let (_t,mut text)=mpsc::channel(8);let (reports,_)=mpsc::channel(8);
        let mut bytes=Vec::new();
        let sent=transmit_file(&core,&mut bytes,work,&mut priority,&mut normal,&mut text,&reports).await?;
        assert_eq!(sent,selected!=Some(None));
        let persisted=core.store.call(|c| Ok(c.query_row("SELECT sent FROM outbox_deliveries",[],|r|r.get::<_,i64>(0))?)).await?;
        assert_eq!(persisted,if sent && selected.is_some(){3}else{0},"END flushes sub-threshold progress; legacy sends do not checkpoint");
        let mut wire=bytes.as_slice();let mut messages=Vec::new();while let Some(m)=read_frame(&mut wire).await?{messages.push(m);}
        assert!(matches!(&messages[0],Message::FileBegin(b) if b.resume==selected.is_some().then_some(true)));
        if selected==Some(None){assert_eq!(messages.len(),1);}else{
            let offset=selected.flatten().unwrap_or(0);
            if offset<3 {assert!(matches!(&messages[1],Message::FileChunk(c) if c.offset==offset&&c.data==b"abc"[offset as usize..]));}
            assert!(matches!(messages.last(),Some(Message::FileEnd(_))));
        }
        if selected.is_none(){
            let mut begin=delivery.begin;begin.hlc=None;begin.source_device=None;
            let expected=[Message::FileBegin(begin.clone()),Message::FileChunk(FileChunk{transfer_id:begin.transfer_id.clone(),offset:0,data:b"abc".to_vec()}),Message::FileEnd(FileEnd{transfer_id:begin.transfer_id})].iter().map(encode_frame).collect::<std::result::Result<Vec<_>,_>>()?.concat();
            assert_eq!(bytes,expected,"legacy file frames are byte-identical");
        }
        core.shutdown_actions().await?;
    }Ok(())
}

#[tokio::test]
async fn advisory_r1_resume_rejects_initial_cache_fallback_without_receipt() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    std::fs::write(core.config().save_path, b"not a directory")?;
    let mut store = core.store.clone();
    store.file_resume = true;
    let mut begin = test_begin(b"abc");
    begin.resume = Some(true);
    let (tx, mut rx) = mpsc::channel(8);
    let mut incoming = None;
    core.receive_test(
        &store,
        &device,
        Message::FileBegin(begin.clone()),
        &mut incoming,
        &tx,
    )
    .await?;
    assert!(matches!(
        rx.recv().await,
        Some(Message::Nack(Nack {
            code: NackCode::NotWritable,
            ..
        }))
    ));
    assert!(incoming.is_none());
    assert!(store.receipt(device, begin.transfer_id).await?.is_none());
    core.shutdown_actions().await
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn advisory_r4_resume_midtransfer_fallback_finishes_without_checkpoint() -> Result<()> {
    for unavailable in [false, true] {
        let (dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let mut store = core.store.clone();
        store.file_resume = true;
        let mut begin = test_begin(b"abc");
        begin.resume = Some(true);
        let (tx, mut rx) = mpsc::channel(8);
        let mut incoming = None;
        core.receive_test(
            &store,
            &device,
            Message::FileBegin(begin.clone()),
            &mut incoming,
            &tx,
        )
        .await?;
        assert!(matches!(rx.recv().await, Some(Message::FileReady(_))));
        let original = incoming.as_ref().unwrap().stage.as_ref().unwrap().path()?;
        let offline = dir.path().join("offline");
        if unavailable {
            std::fs::rename(core.config().save_path, &offline)?;
            store.fail_next("discard_stage", 1);
        }
        incoming.as_mut().unwrap().file = Some(
            tokio::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .await?,
        );
        core.receive_test(
            &store,
            &device,
            Message::FileChunk(FileChunk {
                transfer_id: begin.transfer_id.clone(),
                offset: 0,
                data: b"ab".to_vec(),
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        assert_eq!(
            incoming.as_ref().unwrap().receipt,
            None,
            "fallback must not fail checkpoint ownership"
        );
        if !unavailable {
            assert!(
                store
                    .resume_stage(device.clone(), begin.clone())
                    .await?
                    .is_none(),
                "fallback purges resumable ownership"
            );
        }
        core.receive_test(
            &store,
            &device,
            Message::FileChunk(FileChunk {
                transfer_id: begin.transfer_id.clone(),
                offset: 2,
                data: b"c".to_vec(),
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        core.receive_test(
            &store,
            &device,
            Message::FileEnd(FileEnd {
                transfer_id: begin.transfer_id.clone(),
            }),
            &mut incoming,
            &tx,
        )
        .await?;
        assert!(matches!(
            rx.recv().await,
            Some(Message::Nack(Nack {
                code: NackCode::DiskFull,
                ..
            }))
        ));
        assert!(store.receipt(device, begin.transfer_id).await?.is_none());
        let cached: Vec<_> = std::fs::read_dir(&core.paths.cache)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .collect();
        assert_eq!(cached.len(), 1);
        assert_eq!(std::fs::read(cached[0].path())?, b"abc");
        if unavailable {
            assert_eq!(
                store
                    .call(|c| Ok(c
                        .query_row("SELECT count(*) FROM file_partials", [], |r| r
                            .get::<_, i64>(0))?))
                    .await?,
                1,
                "END retains a deferred original-stage deletion"
            );
            std::fs::rename(&offline, core.config().save_path)?;
            store.maintain_partials().await?;
        }
        assert!(!original.exists());
        assert_eq!(
            store
                .call(
                    |c| Ok(c.query_row("SELECT count(*) FROM file_partials", [], |r| r
                        .get::<_, i64>(0))?)
                )
                .await?,
            0
        );
        core.shutdown_actions().await?;
    }
    Ok(())
}
#[tokio::test]
async fn advisory_r6_resume_rechecks_receipt_admission_before_ready() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let mut store = core.store.clone();
    store.file_resume = true;
    let mut begin = test_begin(b"abc");
    begin.resume = Some(true);
    let (tx, mut rx) = mpsc::channel(8);
    let mut incoming = None;
    core.receive_test(
        &store,
        &device,
        Message::FileBegin(begin.clone()),
        &mut incoming,
        &tx,
    )
    .await?;
    rx.recv().await;
    drop(incoming.take());
    store.fail_next("admission", 1);
    core.receive_test(
        &store,
        &device,
        Message::FileBegin(begin),
        &mut incoming,
        &tx,
    )
    .await?;
    assert!(matches!(
        rx.recv().await,
        Some(Message::Nack(Nack {
            code: NackCode::StorageFailed,
            ..
        }))
    ));
    assert!(incoming.is_none());
    core.shutdown_actions().await
}
#[tokio::test]
async fn advisory_r5_resume_cancel_interrupts_begin_admission() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let mut store = core.store.clone();
    store.file_resume = true;
    let mut begin = test_begin(b"abc");
    begin.resume = Some(true);
    let (tx, mut rx) = mpsc::channel(8);
    let mut incoming = None;
    core.receive_test(
        &store,
        &device,
        Message::FileBegin(begin.clone()),
        &mut incoming,
        &tx,
    )
    .await?;
    rx.recv().await;
    drop(incoming.take());
    let mut permits = Vec::new();
    for _ in 0..crate::work::BLOCKING_LIMIT {
        permits.push(store.reserve().await);
    }
    let receive = core.receive_test(
        &store,
        &device,
        Message::FileBegin(begin.clone()),
        &mut incoming,
        &tx,
    );
    tokio::pin!(receive);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            receive.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    store.signal_cancel(&device, &begin.transfer_id);
    let prompt = tokio::time::timeout(Duration::from_millis(200), &mut receive).await;
    drop(permits);
    let cancelled = prompt.is_ok();
    match prompt {
        Ok(result) => result?,
        Err(_) => receive.await?,
    };
    assert!(
        cancelled,
        "CANCEL must release BEGIN without waiting for store admission"
    );
    assert!(
        rx.try_recv().is_err(),
        "cancelled BEGIN emits no READY or NACK"
    );
    store
        .purge_partial(device.clone(), begin.transfer_id)
        .await?;
    core.shutdown_actions().await
}

#[tokio::test]
async fn advisory_r5_resume_cancel_before_worker_dispatch_never_waits_for_admission() -> Result<()>
{
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let mut store = core.store.clone();
    store.file_resume = true;
    let mut begin = test_begin(b"abc");
    begin.resume = Some(true);
    let message = ApplicationMessage {
        resume_cancel: Some(store.register_resume_begin(&device, &begin.transfer_id)),
        message: Message::FileBegin(begin.clone()),
        refusal: None,
    };
    let mut permits = Vec::new();
    for _ in 0..crate::work::BLOCKING_LIMIT {
        permits.push(store.reserve().await);
    }
    store.signal_cancel(&device, &begin.transfer_id);
    let (tx, mut replies) = mpsc::channel(4);
    let mut incoming = None;
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        core.receive(&store, None, &device, message, &mut incoming, &tx),
    )
    .await;
    drop(permits);
    result??;
    assert!(incoming.is_none());
    assert!(replies.try_recv().is_err());
    core.shutdown_actions().await
}
