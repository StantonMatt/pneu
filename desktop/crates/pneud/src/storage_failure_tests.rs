const STORAGE_PNG: &[u8] = b"\x89PNG\r\n\x1a\nmock image";
// Included in mesh::tests so the fault harness uses the same isolated TLS fixtures.
async fn storage_reply(tls: &mut TlsStream<TcpStream>) -> Result<Message> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match read_frame(tls).await?.context("session closed")? {
                Message::Status(_) => {}
                message => return Ok(message),
            }
        }
    })
    .await?
}
async fn storage_ping(tls: &mut TlsStream<TcpStream>) -> Result<()> {
    let ping = Ping {
        nonce: Nonce::new(crate::random_hex(8))?,
    };
    send(tls, &Message::Ping(ping.clone())).await?;
    assert_eq!(storage_reply(tls).await?, Message::Pong(ping));
    Ok(())
}
fn storage_text(peer: &Id) -> Clipboard {
    Clipboard {
            sensitive: false,
        transfer_id: Id::from_bytes(rand::random()),
        text: "storage fault test".into(),
        created_at: crate::now_ms(),
        origin: ClipboardOrigin::Clipboard,
        hlc: Some(crate::now_ms()),
        source_device: Some(peer.clone()),
    }
}
fn storage_file(peer: &Id) -> FileBegin {
    FileBegin {
            sensitive: false,
        transfer_id: Id::from_bytes(rand::random()),
        file_name: "clipboard.png".into(),
        mime: "image/png".into(),
        size: STORAGE_PNG.len() as u64,
        sha256: Hash::new(hex::encode(Sha256::digest(STORAGE_PNG))).unwrap(),
        kind: FileKind::ClipboardImage,
        created_at: crate::now_ms(),
        source_media_id: None,
        hlc: Some(crate::now_ms()),
        source_device: Some(peer.clone()),
    }
}
async fn storage_send_file(tls: &mut TlsStream<TcpStream>, begin: &FileBegin) -> Result<()> {
    for message in [
        Message::FileBegin(begin.clone()),
        Message::FileChunk(FileChunk {
            transfer_id: begin.transfer_id.clone(),
            offset: 0,
            data: STORAGE_PNG.to_vec(),
        }),
        Message::FileEnd(FileEnd {
            transfer_id: begin.transfer_id.clone(),
        }),
    ] {
        send(tls, &message).await?;
    }
    Ok(())
}
fn storage_nack(message: Message, id: &Id) {
    assert!(
        matches!(&message, Message::Nack(Nack { transfer_id, code: NackCode::StorageFailed, retryable: true, .. }) if transfer_id == id),
        "{message:?}"
    );
}

#[tokio::test]
async fn storage_fault_readiness_never_blocks_pong_and_retries_without_another_ping() -> Result<()>
{
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    drop(lb);
    bind(&a, &b).await?;
    bind(&b, &a).await?;
    a.store.fail_next("mark_peer_established", usize::MAX);
    let address = la.local_addr()?.to_string();
    let server = Task::spawn(a.clone().serve(la, false));
    let ComputerLink { mut tls, .. } = b
        .connect_computer(
            &[address],
            a.identity.fingerprint.clone(),
            Some(&a.identity.device_id),
        )
        .await?;
    storage_ping(&mut tls).await?;
    until(|| a.store.faults_remaining("mark_peer_established") < usize::MAX).await?;
    assert!(
        !a.store
            .peer_established(b.identity.device_id.clone())
            .await?
    );
    storage_ping(&mut tls).await?;
    a.store.fail_next("mark_peer_established", 0);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !a
            .store
            .peer_established(b.identity.device_id.clone())
            .await?
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    drop(tls);
    a.request_shutdown();
    server.await??;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_incoming_text_sites_nack_and_same_session_retries() -> Result<()> {
    for site in [
        "observe_hlc",
        "receipt",
        "claim_text",
        "persist_clipboard_history",
        "applied_image",
        "finalize_text",
        "release_text_claim",
    ] {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(lb);
        bind(&a, &b).await?;
        bind(&b, &a).await?;
        let address = la.local_addr()?.to_string();
        let server = Task::spawn(a.clone().serve(la, false));
        let ComputerLink { mut tls, .. } = b
            .connect_computer(
                &[address],
                a.identity.fingerprint.clone(),
                Some(&a.identity.device_id),
            )
            .await?;
        storage_ping(&mut tls).await?;
        let text = storage_text(&b.identity.device_id);
        a.store
            .fail_next(site, if site == "release_text_claim" { 2 } else { 1 });
        if site == "release_text_claim" {
            a.store.fail_next("persist_clipboard_history", 1);
        }
        send(&mut tls, &Message::Clipboard(text.clone())).await?;
        storage_nack(storage_reply(&mut tls).await?, &text.transfer_id);
        storage_ping(&mut tls).await?;
        // NACK delivery does not wait for asynchronous claim discard/Drop
        // cleanup. The retained claim tracks that work through Store::drain.
        a.store.drain().await;
        assert_eq!(a.store.faults_remaining(site), 0, "unexercised site {site}");
        send(&mut tls, &Message::Clipboard(text.clone())).await?;
        assert!(
            matches!(storage_reply(&mut tls).await?, Message::Ack(_)),
            "retry failed at {site}"
        );
        drop(tls);
        a.request_shutdown();
        server.await??;
        b.shutdown_actions().await?;
    }
    Ok(())
}

#[tokio::test]
async fn storage_fault_incoming_file_sites_drain_nack_and_retry() -> Result<()> {
    for site in [
        "observe_hlc",
        "receipt",
        "admission",
        "create_stage",
        "commit_file_checked",
        "discard_stage",
    ] {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(lb);
        bind(&a, &b).await?;
        bind(&b, &a).await?;
        let address = la.local_addr()?.to_string();
        let server = Task::spawn(a.clone().serve(la, false));
        let ComputerLink { mut tls, .. } = b
            .connect_computer(
                &[address],
                a.identity.fingerprint.clone(),
                Some(&a.identity.device_id),
            )
            .await?;
        storage_ping(&mut tls).await?;
        let begin = storage_file(&b.identity.device_id);
        a.store.fail_next(site, 1);
        if site == "discard_stage" {
            a.store.fail_next("commit_file_checked", 1);
        }
        storage_send_file(&mut tls, &begin).await?;
        storage_nack(storage_reply(&mut tls).await?, &begin.transfer_id);
        assert_eq!(a.store.faults_remaining(site), 0, "unexercised site {site}");
        storage_ping(&mut tls).await?;
        storage_send_file(&mut tls, &begin).await?;
        assert!(
            matches!(storage_reply(&mut tls).await?, Message::Ack(_)),
            "retry failed at {site}"
        );
        drop(tls);
        a.request_shutdown();
        server.await??;
        b.shutdown_actions().await?;
    }
    Ok(())
}

#[tokio::test]
async fn storage_fault_settings_merge_and_ack_keep_control_alive() -> Result<()> {
    for site in ["settings_merge", "settings_ack"] {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(lb);
        bind(&a, &b).await?;
        bind(&b, &a).await?;
        let address = la.local_addr()?.to_string();
        let server = Task::spawn(a.clone().serve(la, false));
        let ComputerLink { mut tls, .. } = b
            .connect_computer(
                &[address],
                a.identity.fingerprint.clone(),
                Some(&a.identity.device_id),
            )
            .await?;
        a.store.fail_next(site, 1);
        if site == "settings_ack" {
            let snapshot = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(Message::PeerSettings(snapshot)) =
                        super::read_frame(&mut tls).await?
                    {
                        break Ok::<_, anyhow::Error>(snapshot);
                    }
                }
            })
            .await??;
            send(
                &mut tls,
                &Message::Ack(Ack {
                    transfer_id: snapshot.transfer_id,
                    deduplicated: false,
                }),
            )
            .await?;
            until(|| a.store.faults_remaining(site) == 0).await?;
            assert!(
                a.settings_ready
                    .lock()
                    .unwrap()
                    .get(&b.identity.device_id)
                    .is_none()
            );
        } else {
            storage_ping(&mut tls).await?;
            let fields = b
                .store
                .settings_fields(
                    b.identity.device_id.clone(),
                    a.identity.device_id.clone(),
                    false,
                )
                .await?;
            let snapshot = PeerSettings {
                transfer_id: Id::from_bytes(rand::random()),
                settings: fields,
            };
            send(&mut tls, &Message::PeerSettings(snapshot.clone())).await?;
            storage_nack(storage_reply(&mut tls).await?, &snapshot.transfer_id);
            assert!(
                a.store
                    .receipt(b.identity.device_id.clone(), snapshot.transfer_id.clone())
                    .await?
                    .is_none()
            );
            send(&mut tls, &Message::PeerSettings(snapshot)).await?;
            assert!(matches!(storage_reply(&mut tls).await?, Message::Ack(_)));
        }
        storage_ping(&mut tls).await?;
        drop(tls);
        a.request_shutdown();
        server.await??;
        b.shutdown_actions().await?;
    }
    Ok(())
}

#[tokio::test]
async fn storage_fault_unavailable_policy_refuses_content_and_recovers() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    drop(lb);
    bind(&a, &b).await?;
    bind(&b, &a).await?;
    a.store.fail_next("settings_fields", 1);
    a.store.fail_next("settings_outgoing", 1);
    let address = la.local_addr()?.to_string();
    let server = Task::spawn(a.clone().serve(la, false));
    let ComputerLink { mut tls, .. } = b
        .connect_computer(
            &[address],
            a.identity.fingerprint.clone(),
            Some(&a.identity.device_id),
        )
        .await?;
    storage_ping(&mut tls).await?;
    let text = storage_text(&b.identity.device_id);
    send(&mut tls, &Message::Clipboard(text.clone())).await?;
    storage_nack(storage_reply(&mut tls).await?, &text.transfer_id);
    let begin = storage_file(&b.identity.device_id);
    storage_send_file(&mut tls, &begin).await?;
    storage_nack(storage_reply(&mut tls).await?, &begin.transfer_id);
    assert_eq!(a.store.faults_remaining("settings_fields"), 0);
    // A valid incoming snapshot loads policy and allows exact content retry.
    let fields = b
        .store
        .settings_fields(
            b.identity.device_id.clone(),
            a.identity.device_id.clone(),
            false,
        )
        .await?;
    send(
        &mut tls,
        &Message::PeerSettings(PeerSettings {
            transfer_id: Id::from_bytes(rand::random()),
            settings: fields,
        }),
    )
    .await?;
    assert!(matches!(storage_reply(&mut tls).await?, Message::Ack(_)));
    send(&mut tls, &Message::Clipboard(text)).await?;
    assert!(matches!(storage_reply(&mut tls).await?, Message::Ack(_)));
    storage_ping(&mut tls).await?;
    drop(tls);
    a.request_shutdown();
    server.await??;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_inbound_metadata_retries_and_never_uses_legacy_hints() -> Result<()> {
    for mesh in [true, false] {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(lb);
        bind(&a, &b).await?;
        bind(&b, &a).await?;
        a.store.fail_next("remember_peer_hello", 1);
        a.store.fail_next("persist_phone", 1);
        a.store.fail_next("mark_peer_established", usize::MAX);
        let address = la.local_addr()?.to_string();
        let server = Task::spawn(a.clone().serve(la, false));
        let connector = TlsConnector::from(Arc::new(crate::tls::client_config(
            &b.identity,
            a.identity.fingerprint.clone(),
        )?));
        let mut tls: TlsStream<TcpStream> = connector
            .connect(
                rustls::pki_types::ServerName::try_from("pneu.local")?,
                TcpStream::connect(address).await?,
            )
            .await?
            .into();
        let mut hello = b.computer_hello();
        hello.name = "new peer name".into();
        hello.addresses = Some(vec!["192.0.2.77:47821".into()]);
        if !mesh {
            hello.capabilities.retain(|c| c != "mesh");
        }
        send(&mut tls, &Message::Hello(hello)).await?;
        assert!(matches!(
            read_frame(&mut tls).await?,
            Some(Message::Hello(_))
        ));
        storage_ping(&mut tls).await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if a.store.peer_info().await?[0].name == "new peer name" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        assert_eq!(a.store.faults_remaining("persist_phone"), 0);
        let peer = a.store.peer_info().await?.remove(0);
        assert_eq!(peer.addresses.contains(&"192.0.2.77:47821".into()), mesh);
        if !mesh {
            assert_eq!(
                a.store.faults_remaining("mark_peer_established"),
                usize::MAX,
                "phone persisted computer evidence"
            );
        }
        a.store.fail_next("mark_peer_established", 0);
        drop(tls);
        a.request_shutdown();
        server.await??;
        assert!(a.paths.data.join("phone.json").is_file());
        b.shutdown_actions().await?;
    }
    Ok(())
}

#[tokio::test]
async fn storage_fault_dial_evidence_and_route_retry_without_dropping_connection() -> Result<()> {
    let (_da, a, la) = core().await?;
    drop(la);
    let (_db, b, lb) = core().await?;
    let lb = MockListener::new(lb);
    let address = lb.local_addr()?.to_string();
    computer_route(&a, &b, address.clone()).await?;
    // Keep a stale last-good route ahead of the reachable cached hint.
    a.store
        .remember_route(b.identity.device_id.clone(), "127.0.0.1:1".into())
        .await?;
    a.store.fail_next("peer_established", 1);
    a.store.fail_next("mark_peer_established", 1);
    a.store.fail_next("remember_route", 1);
    let peer = a.store.peer_info().await?.remove(0);
    let (_cancel, rx) = watch::channel(false);
    let worker = Task::spawn(a.clone().dial_worker(peer, rx));
    let mut tls = accept_ready(&b, &lb, None).await?;
    until(|| a.connected_devices().contains(&b.identity.device_id)).await?;
    storage_ping(&mut tls).await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if a.store
                .peer_established(b.identity.device_id.clone())
                .await?
                && a.store.peer_info().await?[0].addresses[0] == address
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    for site in [
        "peer_established",
        "mark_peer_established",
        "remember_route",
    ] {
        assert_eq!(a.store.faults_remaining(site), 0);
    }
    storage_ping(&mut tls).await?;
    worker.abort();
    let _ = worker.await;
    a.shutdown_actions().await?;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_join_install_retries_and_existing_legacy_binding_saves_working_route()
-> Result<()> {
    for existing in [true, false] {
        let (_da, a, la) = core().await?;
        drop(la);
        let (_db, b, lb) = core().await?;
        let lb = MockListener::new(lb);
        let address = lb.local_addr()?.to_string();
        if existing {
            computer_route(&a, &b, "192.0.2.9:47821".into()).await?;
        }
        let generation = a.store.pairing_generation(&b.identity.device_id);
        let uri = b
            .start_pairing_with_addresses(vec![address.clone()])
            .await?;
        let preview = a.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        a.confirm_join(attempt).await?;
        if !existing {
            a.store.fail_next("pair_then_hello", 1);
        } else {
            a.store.fail_next("remember_route", 1);
        }
        for round in 0..if existing { 1 } else { 2 } {
            let pending = a.mesh.join.lock().unwrap().as_ref().unwrap().clone();
            let worker = Task::spawn(a.clone().join_worker(pending));
            let mut tls = accept_client_hello(&b, &lb).await?;
            let mut hello = b.computer_hello();
            hello.capabilities.retain(|c| c != "mesh");
            hello.addresses = Some(vec!["192.0.2.99:47821".into()]);
            send(&mut tls, &Message::Hello(hello)).await?;
            assert!(matches!(
                read_frame(&mut tls).await?,
                Some(Message::PairRequest(_))
            ));
            accepted(&mut tls).await?;
            if !existing && round == 0 {
                assert!(worker.await?.is_err());
                assert!(a.paired_devices().is_empty());
                assert!(a.joining(attempt));
                continue;
            }
            until(|| a.connected_devices().contains(&b.identity.device_id)).await?;
            storage_ping(&mut tls).await?;
            tokio::time::timeout(Duration::from_secs(3), async {
                while a.store.peer_info().await?[0].addresses[0] != address {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            let peer = a.store.peer_info().await?.remove(0);
            assert!(!peer.addresses.contains(&"192.0.2.99:47821".into()));
            if existing {
                assert_eq!(Some(peer.generation), generation);
            }
            worker.abort();
            let _ = worker.await;
        }
        a.shutdown_actions().await?;
        b.shutdown_actions().await?;
    }
    Ok(())
}

async fn storage_legacy_link(a: &Core, b: &Core, address: String) -> Result<TlsStream<TcpStream>> {
    let connector = TlsConnector::from(Arc::new(crate::tls::client_config(
        &b.identity,
        a.identity.fingerprint.clone(),
    )?));
    let mut tls: TlsStream<TcpStream> = connector
        .connect(
            rustls::pki_types::ServerName::try_from("pneu.local")?,
            TcpStream::connect(address).await?,
        )
        .await?
        .into();
    let mut hello = b.computer_hello();
    hello.capabilities.retain(|c| c != "mesh");
    hello.paused = Some(false);
    send(&mut tls, &Message::Hello(hello)).await?;
    assert!(matches!(
        read_frame(&mut tls).await?,
        Some(Message::Hello(_))
    ));
    Ok(tls)
}

#[tokio::test]
async fn storage_fault_local_clock_allocation_and_policy_reads_leave_session_alive() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    drop(lb);
    bind(&a, &b).await?;
    bind(&b, &a).await?;
    let address = la.local_addr()?.to_string();
    let server = Task::spawn(a.clone().serve(la, false));
    let mut tls = storage_legacy_link(&a, &b, address).await?;
    storage_ping(&mut tls).await?;
    a.store.fail_next("local_observation_checked", 1);
    assert!(
        a.send_clipboard(b.identity.device_id.clone(), "must not send".into())
            .await
            .is_err()
    );
    a.store.fail_next("local_observation_checked", 1);
    // Watcher observations retain their stamp and recover autonomously;
    // explicit shares above still return their terminal storage error.
    tokio::time::timeout(Duration::from_secs(2), a.observe_local_clipboard_offer()).await??;
    assert_eq!(a.store.faults_remaining("local_observation_checked"), 0);
    assert!(
        a.store
            .first_outgoing(b.identity.device_id.clone())
            .await?
            .is_none()
    );
    a.store.fail_next("peer_info", 1);
    until(|| a.store.faults_remaining("peer_info") == 0).await?;
    assert!(a.connected_devices().contains(&b.identity.device_id));
    storage_ping(&mut tls).await?;
    drop(tls);
    a.request_shutdown();
    server.await??;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_settings_outgoing_retries_exact_snapshot_after_local_error() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    drop(lb);
    bind(&a, &b).await?;
    bind(&b, &a).await?;
    let retained = a
        .store
        .settings_outgoing(
            a.identity.device_id.clone(),
            b.identity.device_id.clone(),
            false,
        )
        .await?;
    a.store.fail_next("settings_outgoing", 1);
    let mut events = a.subscribe();
    let address = la.local_addr()?.to_string();
    let server = Task::spawn(a.clone().serve(la, false));
    let ComputerLink { mut tls, .. } = b
        .connect_computer(
            &[address],
            a.identity.fingerprint.clone(),
            Some(&a.identity.device_id),
        )
        .await?;
    storage_ping(&mut tls).await?;
    // The blocking store job consumes the fault before READY receives its
    // result and arms the retry. Observe READY's error before moving its clock.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(events.recv().await?, Event::Error { device: Some(device), message }
                if device == b.identity.device_id && message.starts_with("settings persistence failed:"))
            {
                break Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    assert_eq!(a.store.faults_remaining("settings_outgoing"), 0);
    // The failure has no receipt timer. Retry without disconnecting or losing policy.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();
    let replay = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(Message::PeerSettings(snapshot)) = super::read_frame(&mut tls).await? {
                break Ok::<_, anyhow::Error>(snapshot);
            }
        }
    })
    .await??;
    assert_eq!(replay, retained);
    send(
        &mut tls,
        &Message::Ack(Ack {
            transfer_id: replay.transfer_id,
            deduplicated: false,
        }),
    )
    .await?;
    storage_ping(&mut tls).await?;
    until(|| {
        a.store
            .pair_settings
            .lock()
            .unwrap()
            .contains_key(&b.identity.device_id)
    })
    .await?;
    drop(tls);
    a.request_shutdown();
    server.await??;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_cache_commit_and_stage_cleanup_preserve_control() -> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    drop(lb);
    bind(&a, &b).await?;
    bind(&b, &a).await?;
    // A missing selected volume uses the existing recoverable cache path.
    if a.config().save_path.is_dir() {
        std::fs::remove_dir(a.config().save_path.clone())?;
    }
    std::fs::create_dir_all(a.config().save_path.parent().unwrap())?;
    std::fs::write(a.config().save_path.clone(), b"volume unavailable")?;
    let address = la.local_addr()?.to_string();
    let server = Task::spawn(a.clone().serve(la, false));
    let mut tls = storage_legacy_link(&a, &b, address).await?;
    storage_ping(&mut tls).await?;
    let mut begin = storage_file(&b.identity.device_id);
    begin.hlc = None;
    begin.source_device = None;
    begin.kind = FileKind::File;
    a.store.fail_next("commit_cache", 1);
    a.store.fail_next("discard_stage", 1);
    storage_send_file(&mut tls, &begin).await?;
    storage_nack(storage_reply(&mut tls).await?, &begin.transfer_id);
    for site in ["commit_cache", "discard_stage"] {
        assert_eq!(a.store.faults_remaining(site), 0);
    }
    storage_ping(&mut tls).await?;
    std::fs::remove_file(a.config().save_path.clone())?;
    std::fs::create_dir(a.config().save_path.clone())?;
    storage_send_file(&mut tls, &begin).await?;
    assert!(matches!(storage_reply(&mut tls).await?, Message::Ack(_)));
    drop(tls);
    a.request_shutdown();
    server.await??;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_inbound_pairing_reports_storage_denial_and_preserves_invitation()
-> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    drop(lb);
    let address = la.local_addr()?.to_string();
    let uri = a
        .start_pairing_with_addresses(vec![address.clone()])
        .await?;
    a.store.fail_next("pair_then_hello", 1);
    let server = Task::spawn(a.clone().serve(la, false));
    for round in 0..2 {
        if round == 1 {
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        let ComputerLink { mut tls, .. } = b
            .connect_computer(std::slice::from_ref(&address), a.identity.fingerprint.clone(), None)
            .await?;
        send(
            &mut tls,
            &Message::PairRequest(PairRequest {
                token: uri.token.clone(),
            }),
        )
        .await?;
        let Message::PairResult(result) = storage_reply(&mut tls).await? else {
            bail!("pair result expected");
        };
        if round == 0 {
            assert!(!result.accepted);
            assert_eq!(result.code, Some(PairCode::StorageFailed));
            assert!(a.paired_devices().is_empty());
        } else {
            assert!(result.accepted);
            storage_ping(&mut tls).await?;
        }
    }
    a.request_shutdown();
    server.await??;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_readiness_read_cannot_authorize_revocation() -> Result<()> {
    let (_da, a, la) = core().await?;
    drop(la);
    let (_db, b, lb) = core().await?;
    let lb = MockListener::new(lb);
    computer_route(&a, &b, lb.local_addr()?.to_string()).await?;
    a.store
        .mark_peer_established(b.identity.device_id.clone())
        .await?;
    a.store.fail_next("peer_established", 1);
    let peer = a.store.peer_info().await?.remove(0);
    let (cancel, rx) = watch::channel(false);
    let worker = Task::spawn(a.clone().dial_worker(peer, rx));
    let mut denied = accept_client_hello(&b, &lb).await?;
    send(
        &mut denied,
        &Message::Error(ErrorMessage {
            code: ErrorCode::Unauthorized,
            message: None,
            supported_version: None,
        }),
    )
    .await?;
    let _ = tokio::time::timeout(Duration::from_secs(3), super::read_frame(&mut denied)).await?;
    until(|| a.link_problem(&b.identity.device_id) == Some(LinkProblem::Unauthorized)).await?;
    assert_eq!(a.paired_devices(), std::slice::from_ref(&b.identity.device_id));
    assert_eq!(a.store.faults_remaining("peer_established"), 0);
    cancel.send_replace(true);
    worker.await??;
    a.shutdown_actions().await?;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_remote_unpair_preserves_binding_and_closes_without_protocol_blame()
-> Result<()> {
    let (_da, a, la) = core().await?;
    let (_db, b, lb) = core().await?;
    drop(lb);
    bind(&a, &b).await?;
    bind(&b, &a).await?;
    let address = la.local_addr()?.to_string();
    let server = Task::spawn(a.clone().serve(la, false));
    let mut tls = storage_legacy_link(&a, &b, address.clone()).await?;
    storage_ping(&mut tls).await?;
    a.store.fail_next("unpair_reserved", 1);
    send(&mut tls, &Message::Unpair(Unpair {})).await?;
    assert!(matches!(
        storage_reply(&mut tls).await?,
        Message::Error(ErrorMessage {
            code: ErrorCode::Closing,
            ..
        })
    ));
    assert_eq!(a.paired_devices(), std::slice::from_ref(&b.identity.device_id));
    assert_eq!(a.store.faults_remaining("unpair_reserved"), 0);
    drop(tls);
    let mut replacement = storage_legacy_link(&a, &b, address).await?;
    storage_ping(&mut replacement).await?;
    drop(replacement);
    a.request_shutdown();
    server.await??;
    b.shutdown_actions().await
}

#[tokio::test]
async fn storage_fault_post_effect_register_keeps_ordering_conservative() -> Result<()> {
    for image in [false, true] {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(lb);
        bind(&a, &b).await?;
        bind(&b, &a).await?;
        let address = la.local_addr()?.to_string();
        let server = Task::spawn(a.clone().serve(la, false));
        let ComputerLink { mut tls, .. } = b
            .connect_computer(
                &[address],
                a.identity.fingerprint.clone(),
                Some(&a.identity.device_id),
            )
            .await?;
        storage_ping(&mut tls).await?;
        let stamp = crate::now_ms() + 500;
        a.store.fail_next("applied_image", 1);
        if image {
            let mut begin = storage_file(&b.identity.device_id);
            begin.hlc = Some(stamp);
            storage_send_file(&mut tls, &begin).await?;
            assert!(matches!(storage_reply(&mut tls).await?, Message::Ack(_)));
            until(|| a.store.faults_remaining("applied_image") == 0).await?;
            a.clipboard_test_idle().await;
        } else {
            let mut text = storage_text(&b.identity.device_id);
            text.hlc = Some(stamp);
            send(&mut tls, &Message::Clipboard(text.clone())).await?;
            storage_nack(storage_reply(&mut tls).await?, &text.transfer_id);
        }
        assert!(a.store.clipboard_snapshot().await?.0.is_none());
        storage_ping(&mut tls).await?;
        // A stale receipt repairs the successful OS effect's register before
        // ACKing, without executing the older text or manufacturing an origin.
        let mut older = storage_text(&b.identity.device_id);
        older.hlc = Some(stamp - 1);
        older.text = "older must not apply".into();
        send(&mut tls, &Message::Clipboard(older)).await?;
        assert!(matches!(
            storage_reply(&mut tls).await?,
            Message::Ack(Ack {
                deduplicated: true,
                ..
            })
        ));
        assert_eq!(a.store.clipboard_snapshot().await?.0.unwrap().hlc, stamp);
        storage_ping(&mut tls).await?;
        a.send_clipboard(b.identity.device_id.clone(), "new local".into())
            .await?;
        assert!(a.store.clipboard_snapshot().await?.0.unwrap().hlc > stamp);
        drop(tls);
        a.request_shutdown();
        server.await??;
        b.shutdown_actions().await?;
    }
    Ok(())
}
#[tokio::test]
async fn storage_fault_image_effect_boundary_and_post_effect_record_keep_session_alive()
-> Result<()> {
    for site in ["persist_clipboard_history", "applied_image"] {
        let (_da, a, la) = core().await?;
        let (_db, b, lb) = core().await?;
        drop(lb);
        bind(&a, &b).await?;
        bind(&b, &a).await?;
        let address = la.local_addr()?.to_string();
        let server = Task::spawn(a.clone().serve(la, false));
        let ComputerLink { mut tls, .. } = b
            .connect_computer(
                &[address],
                a.identity.fingerprint.clone(),
                Some(&a.identity.device_id),
            )
            .await?;
        storage_ping(&mut tls).await?;
        a.store.fail_next(site, 1);
        storage_send_file(&mut tls, &storage_file(&b.identity.device_id)).await?;
        assert!(matches!(storage_reply(&mut tls).await?, Message::Ack(_)));
        until(|| a.store.faults_remaining(site) == 0).await?;
        a.clipboard_test_idle().await;
        storage_ping(&mut tls).await?;
        drop(tls);
        a.request_shutdown();
        server.await??;
        b.shutdown_actions().await?;
    }
    Ok(())
}
