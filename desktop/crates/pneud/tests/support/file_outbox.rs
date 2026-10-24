use super::*;
use anyhow::Context;
use pneud::outbox::Targets;
fn all() -> Targets {
    Targets::All("all".into())
}
fn target(peer: &Phone) -> Targets {
    Targets::Devices(vec![peer.identity.device_id.clone()])
}
async fn until(mut condition: impl FnMut() -> bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
async fn receiver(h: &Harness, phone: &Phone) -> Result<PhoneTls> {
    h.pair(phone).await?.close().await?;
    let tls = negotiated_phone(h, phone, false, true).await?;
    until(|| {
        h.core
            .ui_snapshot()
            .send_targets
            .iter()
            .any(|t| t.device == phone.identity.device_id && t.eligible)
    })
    .await?;
    Ok(tls)
}
async fn receive_file(tls: &mut PhoneTls) -> Result<(FileBegin, Vec<u8>)> {
    let Message::FileBegin(begin) = wire_reply(tls).await? else {
        anyhow::bail!("expected file BEGIN")
    };
    let mut bytes = Vec::new();
    loop {
        match wire_reply(tls).await? {
            Message::FileChunk(chunk) => {
                assert_eq!(chunk.transfer_id, begin.transfer_id);
                assert_eq!(chunk.offset, bytes.len() as u64);
                bytes.extend(chunk.data);
            }
            Message::FileEnd(end) => {
                assert_eq!(end.transfer_id, begin.transfer_id);
                break;
            }
            Message::Ping(p) => write_frame(tls, &Message::Pong(p)).await?,
            other => anyhow::bail!("unexpected file frame {other:?}"),
        }
    }
    assert_eq!(bytes.len() as u64, begin.size);
    assert_eq!(hex::encode(Sha256::digest(&bytes)), begin.sha256.as_str());
    Ok((begin, bytes))
}
async fn ack(tls: &mut PhoneTls, id: Id) -> Result<()> {
    write_frame(
        tls,
        &Message::Ack(Ack {
            transfer_id: id,
            deduplicated: false,
        }),
    )
    .await?;
    Ok(())
}
fn source(h: &Harness) -> Result<PathBuf> {
    let p = h.dir.path().join("to-send.txt");
    std::fs::write(&p, b"PC file bytes")?;
    Ok(p)
}
fn operation(reply: &serde_json::Value) -> Result<Id> {
    Ok(Id::new(reply["operation"].as_str().unwrap().to_owned())?)
}

async fn admitted(h: &Harness, reply: serde_json::Value) -> Result<serde_json::Value> {
    let id = operation(&reply)?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = h.core.operation_status(id.clone()).await?;
            if status["state"] != "staging" {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}
#[tokio::test]
async fn outbox_phone_fanout_has_independent_ack_failure_retry_and_cancel() -> Result<()> {
    let h = Harness::new().await?;
    let p = h.phone("phone-a")?;
    let q = h.phone("phone-b")?;
    let mut a = receiver(&h, &p).await?;
    let mut b = receiver(&h, &q).await?;
    write_frame(&mut b, &Message::Status(Status { paused: true })).await?;
    until(|| h.core.phone_paused(&q.identity.device_id) == Some(true)).await?;
    let reply = h.core.send_files(vec![source(&h)?], all()).await?;
    let reply = admitted(&h, reply).await?;
    assert_eq!(reply["accepted"].as_array().unwrap().len(), 1);
    let (a_file, a_bytes) = receive_file(&mut a).await?;
    let (b_file, b_bytes) = receive_file(&mut b).await?;
    assert_eq!(a_file, b_file);
    assert_eq!(a_bytes, b_bytes);
    ack(&mut a, a_file.transfer_id).await?;
    write_frame(
        &mut b,
        &Message::Nack(Nack {
            transfer_id: b_file.transfer_id.clone(),
            code: NackCode::UnsupportedKind,
            retryable: false,
            message: None,
        }),
    )
    .await?;
    // Different peers commit receipts independently; neither receipt orders
    // the other's persistence, even when the wire writes happened in order.
    until(|| {
        let rows = h.core.ui_snapshot().transfers;
        rows.iter()
            .any(|t| t.device == q.identity.device_id && t.state == "failed")
            && rows
                .iter()
                .any(|t| t.device == p.identity.device_id && t.state == "sent")
    })
    .await?;
    assert!(
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.device == p.identity.device_id && t.state == "sent")
    );
    h.core
        .retry_transfer(operation(&reply)?, q.identity.device_id.clone())
        .await?;
    let (replay, _) = receive_file(&mut b).await?;
    assert_eq!(replay, b_file);
    h.core
        .cancel_transfer(operation(&reply)?, Some(q.identity.device_id.clone()))
        .await?;
    until(|| {
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.device == q.identity.device_id && t.state == "cancelled")
    })
    .await?;
    assert!(
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.device == p.identity.device_id && t.state == "sent")
    );
    drop(a);
    drop(b);
    h.stop().await
}
#[tokio::test]
async fn outbox_offline_policy_and_capabilityless_legacy_phone() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("legacy")?;
    let mut old = h.pair(&phone).await?;
    until(|| {
        h.core.ui_snapshot().send_targets.iter().any(|t| {
            t.device == phone.identity.device_id
                && !t.eligible
                && t.reason.as_ref().is_some_and(|r| r.contains("Update"))
        })
    })
    .await?;
    let rejected = admitted(
        &h,
        h.core.send_files(vec![source(&h)?], target(&phone)).await?,
    )
    .await?;
    assert_eq!(rejected["state"], "failed");
    assert!(!rejected["rejected"].as_array().unwrap().is_empty());
    old.close().await?;
    let current = negotiated_phone(&h, &phone, false, true).await?;
    until(|| h.core.ui_snapshot().send_targets.iter().any(|t| t.eligible)).await?;
    drop(current);
    until(|| h.core.connected_devices().is_empty()).await?;
    assert_eq!(
        admitted(&h, h.core.send_files(vec![source(&h)?], all()).await?).await?["state"],
        "failed"
    );
    let explicit = admitted(
        &h,
        h.core.send_files(vec![source(&h)?], target(&phone)).await?,
    )
    .await?;
    assert_eq!(
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .find(|t| t.operation == operation(&explicit).unwrap())
            .unwrap()
            .state,
        "waiting_for_device"
    );
    h.core.cancel_transfer(operation(&explicit)?, None).await?;
    h.core
        .set_setting("queue_offline".into(), true.into())
        .await?;
    let queued = admitted(&h, h.core.send_files(vec![source(&h)?], all()).await?).await?;
    assert_eq!(
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .find(|t| t.operation == operation(&queued).unwrap())
            .unwrap()
            .state,
        "waiting_for_device"
    );
    let mut link = negotiated_phone(&h, &phone, false, true).await?;
    let (file, _) = receive_file(&mut link).await?;
    ack(&mut link, file.transfer_id).await?;
    until(|| {
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.operation == operation(&queued).unwrap() && t.state == "sent")
    })
    .await?;
    drop(link);
    h.stop().await
}
#[tokio::test]
async fn outbox_pc_to_pc_lost_ack_replays_to_receiver_dedupe() -> Result<()> {
    let a = Harness::new().await?;
    let b = Harness::new().await?;
    let uri = a
        .core
        .start_pairing_with_addresses(vec![a.address.clone()])
        .await?;
    let preview = b.core.join(&uri.build()?).await?;
    b.core
        .confirm_join(preview["attempt"].as_str().unwrap())
        .await?;
    until(|| {
        a.core
            .connected_devices()
            .contains(&b.core.identity.device_id)
            && b.core
                .connected_devices()
                .contains(&a.core.identity.device_id)
    })
    .await?;
    assert_eq!(
        b.core.join_status(preview["attempt"].as_str().unwrap())?["device"],
        serde_json::json!(a.core.identity.device_id)
    );
    let key = format!(
        "{}/{}/clipboard",
        a.core.identity.device_id, b.core.identity.device_id
    );
    until(|| {
        a.core
            .ui_snapshot()
            .devices
            .iter()
            .any(|d| d.device == b.core.identity.device_id && d.role == "computer")
    })
    .await?;
    a.core
        .set_peer_setting(b.core.identity.device_id.clone(), key, false)
        .await?;
    // Manual FILE traffic remains permitted with clipboard direction disabled.
    // Disallow durable ACK consumption once to emulate an ACK lost at a crash
    // boundary. The sender must close quietly, then replay exactly the file.
    let db = rusqlite::Connection::open(a.core.paths.data.join("state.sqlite3"))?;
    db.execute_batch("CREATE TRIGGER lose_ack BEFORE UPDATE OF state ON outbox_deliveries WHEN NEW.state='sent' BEGIN SELECT RAISE(ABORT,'lost ACK'); END;")?;
    let mut events = b.core.subscribe();
    let reply = a
        .core
        .send_files(
            vec![source(&a)?],
            Targets::Devices(vec![b.core.identity.device_id.clone()]),
        )
        .await?;
    event_matching(&mut events, |e| {
        matches!(
            e,
            pneud::Event::TransferFinished {
                deduplicated: false,
                ..
            }
        )
    })
    .await?;
    until(|| {
        a.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.error.as_ref().is_some_and(|e| e.contains("interrupted")))
    })
    .await?;
    db.execute_batch("DROP TRIGGER lose_ack;")?;
    event_matching(&mut events, |e| {
        matches!(
            e,
            pneud::Event::TransferFinished {
                deduplicated: true,
                ..
            }
        )
    })
    .await?;
    until(|| {
        a.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.operation == operation(&reply).unwrap() && t.state == "sent")
    })
    .await?;
    let received = b.core.ui_snapshot().transfers;
    let incoming: Vec<_> = received
        .iter()
        .filter(|t| t.direction == "incoming")
        .collect();
    assert_eq!(
        incoming.len(),
        1,
        "a deduplicated replay preserves one Recent item"
    );
    assert_eq!(incoming[0].device, a.core.identity.device_id);
    assert_eq!(incoming[0].state, "sent");
    assert_eq!(incoming[0].sent, incoming[0].size);
    assert!(incoming[0].finished_at.is_some());
    assert_eq!(incoming[0].path.as_ref(), b.saved()?.first());
    assert!(
        serde_json::to_value(incoming[0])?
            .get("file_identity")
            .is_none(),
        "Recent exports a navigation path without a receive identity"
    );
    assert!(
        a.core
            .ui_snapshot()
            .devices
            .iter()
            .all(|d| d.state != "connected" || d.route.as_deref() == Some("Wi-Fi"))
    );
    assert_eq!(b.saved()?.len(), 1);
    assert_eq!(std::fs::read(&b.saved()?[0])?, b"PC file bytes");
    a.stop().await?;
    b.stop().await
}
#[tokio::test]
async fn outbox_cli_and_ipc_validate_paths_and_persist_settings() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut link = receiver(&h, &phone).await?;
    let (listener, _owner) = pneud::ipc::bind(&h.core.paths.socket)?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let ipc = tokio::spawn(pneud::ipc::serve(h.core.clone(), listener, rx));
    let path = source(&h)?;
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_pneud"))
        .args([
            "send",
            "--to",
            phone.identity.device_id.as_str(),
            "to-send.txt",
        ])
        .current_dir(h.dir.path())
        .env("HOME", h.dir.path())
        .env("XDG_RUNTIME_DIR", h.dir.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        reply["operation"].is_string(),
        "explicit --to admits directly"
    );
    assert_eq!(reply["rejected"], serde_json::json!([]));
    let status = pneud::ipc::request(&h.core.paths.socket, pneud::ipc::Command::UiStatus).await?;
    assert!(status["window_request"].as_u64().unwrap() > 0);
    assert!(
        status["pending_send"].is_null(),
        "--to bypasses recipient selection"
    );
    let result = admitted(&h, reply).await?;
    let queried = pneud::ipc::request(
        &h.core.paths.socket,
        pneud::ipc::Command::OperationStatus {
            operation: operation(&result)?,
        },
    )
    .await?;
    assert_eq!(queried, result);
    assert_eq!(result["accepted"][0]["path"], path.to_str().unwrap());
    let (file, _) = receive_file(&mut link).await?;
    ack(&mut link, file.transfer_id).await?;
    let bad = h.dir.path().join("symlink");
    std::os::unix::fs::symlink(&path, &bad)?;
    assert!(pneud::ipc::send_command(&[bad.to_str().unwrap().into()]).is_err());
    for (key, value) in [
        ("clipboard_mode", serde_json::json!("manual")),
        ("choose_devices_each_time", true.into()),
        ("queue_offline", true.into()),
    ] {
        pneud::ipc::request(
            &h.core.paths.socket,
            pneud::ipc::Command::SetSetting {
                key: key.into(),
                value,
            },
        )
        .await?;
    }
    // Untargeted IPC must hand off to a picker with the preference on, or
    // reject before queuing any UI work on the runtime without a ready UI child.
    #[cfg(target_os = "macos")]
    let ui_before_picker =
        pneud::ipc::request(&h.core.paths.socket, pneud::ipc::Command::UiStatus).await?;
    for command in [
        #[cfg(target_os = "macos")]
        pneud::ipc::Command::PresentPicker {
            paths: None,
            targets: Vec::new(),
            clipboard: false,
            targeted: false,
        },
        pneud::ipc::Command::SendFiles {
            paths: vec![path.clone()],
            targets: all(),
            request_id: None,
        },
        pneud::ipc::Command::SendClipboard { targets: all() },
        #[cfg(target_os = "macos")]
        pneud::ipc::desktop_send_command(&[path.to_str().unwrap().into()]),
        #[cfg(target_os = "macos")]
        pneud::ipc::Command::ShowWindow {
            choose_folder: false,
        },
        #[cfg(target_os = "macos")]
        pneud::ipc::Command::ShowWindow {
            choose_folder: true,
        },
    ] {
        let reply = pneud::ipc::request(&h.core.paths.socket, command).await;
        #[cfg(target_os = "macos")]
        {
            assert!(
                reply
                    .unwrap_err()
                    .to_string()
                    .contains("menu bar is unavailable")
            );
            let status =
                pneud::ipc::request(&h.core.paths.socket, pneud::ipc::Command::UiStatus).await?;
            assert!(status["pending_send"].is_null());
            assert_eq!(status["window_request"], ui_before_picker["window_request"]);
        }
        #[cfg(target_os = "linux")]
        {
            let reply = reply?;
            assert!(reply["operation"].is_null() && reply["transfer_id"].is_null());
            let status =
                pneud::ipc::request(&h.core.paths.socket, pneud::ipc::Command::UiStatus).await?;
            let request = &status["pending_send"];
            assert_eq!(request["id"], reply["request"]);
            assert_eq!(request["targeted"], false);
            assert_eq!(
                request["targets"][0]["device"],
                phone.identity.device_id.as_str()
            );
            pneud::ipc::request(
                &h.core.paths.socket,
                pneud::ipc::Command::AcknowledgeSend {
                    request: request["id"].as_str().unwrap().into(),
                },
            )
            .await?;
        }
    }
    pneud::ipc::request(
        &h.core.paths.socket,
        pneud::ipc::Command::Rename {
            name: "Test workstation".into(),
        },
    )
    .await?;
    let add_device =
        pneud::ipc::request(&h.core.paths.socket, pneud::ipc::Command::AddDevice).await;
    let ui = pneud::ipc::request(&h.core.paths.socket, pneud::ipc::Command::UiStatus).await?;
    #[cfg(target_os = "linux")]
    {
        add_device?;
        assert_eq!(ui["add_device"], true);
    }
    #[cfg(target_os = "macos")]
    {
        assert!(
            add_device
                .unwrap_err()
                .to_string()
                .contains("menu bar is unavailable")
        );
        assert_eq!(ui["add_device"], false);
        assert_eq!(ui["window_request"], ui_before_picker["window_request"]);
    }
    h.core.request_window(false);
    assert!(!h.core.window_add_device(h.core.window_request()));
    assert_eq!(ui["config"]["clipboard_mode"], "manual");
    assert!(ui["snapshot"]["send_targets"][0]["would_queue"].is_boolean());
    assert!(ui["snapshot"]["transfers"].is_array());
    let (left, right) = tokio::join!(
        h.core
            .set_setting("notification_preview".into(), false.into()),
        h.core.set_setting("open_in_viewer".into(), true.into())
    );
    left?;
    right?;
    let config = pneud::Config::load(&h.core.paths.config)?;
    assert!(!config.notification_preview && config.open_in_viewer);
    assert_eq!(config.name, "Test workstation");
    assert!(config.queue_offline && config.choose_devices_each_time);
    stop.send(true)?;
    ipc.await??;
    drop(link);
    h.stop().await
}

#[tokio::test]
async fn desktop_send_parser_errors_reach_the_cli_or_window() -> Result<()> {
    let h = Harness::new().await?;
    let (listener, _owner) = pneud::ipc::bind(&h.core.paths.socket)?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let ipc = tokio::spawn(pneud::ipc::serve(h.core.clone(), listener, rx));
    for (args, expected) in [
        (vec!["send"], "send needs 1 to 128 regular files"),
        (vec!["send", "--to"], "--to needs a device ID"),
        (vec!["send", "--unknown"], "unknown send option"),
    ] {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_pneud"))
            .args(args)
            .current_dir(h.dir.path())
            .env("HOME", h.dir.path())
            .env("XDG_RUNTIME_DIR", h.dir.path())
            .output()
            .await?;
        let status =
            pneud::ipc::request(&h.core.paths.socket, pneud::ipc::Command::UiStatus).await?;
        #[cfg(target_os = "macos")]
        {
            // This harness deliberately starts IPC without a UI child. M2 must
            // preserve the CLI cause while its real supervised UI is unavailable.
            assert!(!output.status.success());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(expected), "{stderr}");
            assert!(!stderr.contains("unavailable"), "{stderr}");
            assert!(status["pending_send"].is_null());
            assert_eq!(status["window_request"], 0);
        }
        #[cfg(target_os = "linux")]
        {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(status["pending_send"]["id"], reply["request"]);
            assert!(
                status["pending_send"]["error"]
                    .as_str()
                    .unwrap()
                    .contains(expected)
            );
            pneud::ipc::request(
                &h.core.paths.socket,
                pneud::ipc::Command::AcknowledgeSend {
                    request: reply["request"].as_str().unwrap().into(),
                },
            )
            .await?;
        }
    }
    stop.send(true)?;
    ipc.await??;
    h.stop().await
}

#[tokio::test]
async fn incoming_phone_progress_and_recent_are_scoped_to_each_device() -> Result<()> {
    let h = Harness::new().await?;
    let p = h.phone("phone-a")?;
    let q = h.phone("phone-b")?;
    let mut a = receiver(&h, &p).await?;
    let mut b = receiver(&h, &q).await?;
    let bytes = b"phone photo bytes";
    let transfer_id = Id::from_bytes(rand::random());
    let mut photo = metadata(transfer_id.clone(), bytes);
    photo.kind = FileKind::Photo;
    // Equal transfer IDs from independent devices must have separate rows.
    for link in [&mut a, &mut b] {
        write_frame(link, &Message::FileBegin(photo.clone())).await?;
    }
    write_frame(
        &mut b,
        &Message::FileChunk(FileChunk {
            transfer_id: transfer_id.clone(),
            offset: 0,
            data: bytes[..4].to_vec(),
        }),
    )
    .await?;
    until(|| {
        // Independent connections may process their FileBegin frames in either
        // order. Progress on b alone is not a barrier for a's begin frame.
        let snapshot = h.core.ui_snapshot();
        snapshot.transfers.len() == 2
            && snapshot
                .transfers
                .iter()
                .any(|t| t.device == q.identity.device_id && t.sent == 4)
    })
    .await?;
    let snapshot = h.core.ui_snapshot();
    assert_eq!(snapshot.transfers.len(), 2);
    assert_eq!(
        snapshot
            .transfers
            .iter()
            .find(|t| t.device == p.identity.device_id)
            .unwrap()
            .sent,
        0
    );
    assert_ne!(
        snapshot.transfers[0].operation,
        snapshot.transfers[1].operation
    );
    assert!(
        snapshot
            .devices
            .iter()
            .all(|d| d.route.as_deref() == Some("Wi-Fi"))
    );
    write_frame(
        &mut b,
        &Message::FileChunk(FileChunk {
            transfer_id: transfer_id.clone(),
            offset: 4,
            data: bytes[4..].to_vec(),
        }),
    )
    .await?;
    write_frame(&mut b, &Message::FileEnd(FileEnd { transfer_id })).await?;
    assert!(matches!(wire_reply(&mut b).await?, Message::Ack(_)));
    until(|| {
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.device == q.identity.device_id && t.state == "sent")
    })
    .await?;
    let received = h
        .core
        .ui_snapshot()
        .transfers
        .into_iter()
        .find(|t| t.device == q.identity.device_id)
        .unwrap();
    assert_eq!(received.direction, "incoming");
    assert!(received.finished_at.is_some());
    assert_eq!(std::fs::read(received.path.unwrap())?, bytes);
    drop(a);
    drop(b);
    until(|| h.core.connected_devices().is_empty()).await?;
    let recent = h.core.ui_snapshot().transfers;
    assert_eq!(
        recent.len(),
        1,
        "disconnect removes partial progress, preserving completed Recent"
    );
    h.stop().await
}

#[tokio::test]
async fn outbox_text_lane_and_receipts_do_not_wait_for_file_ack() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut link = receiver(&h, &phone).await?;
    let reply = h.core.send_files(vec![source(&h)?], target(&phone)).await?;
    let (file, _) = receive_file(&mut link).await?;
    h.core
        .send_clipboard(
            phone.identity.device_id.clone(),
            "text during file receipt".into(),
        )
        .await?;
    let Message::Clipboard(text) = wire_reply(&mut link).await? else {
        anyhow::bail!("expected independent text lane")
    };
    assert_eq!(text.text, "text during file receipt");
    ack(&mut link, text.transfer_id).await?;
    // ACK text before FILE. Both outstanding receipts must remain independent.
    assert!(
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.operation == operation(&reply).unwrap() && t.state == "sending")
    );
    ack(&mut link, file.transfer_id).await?;
    until(|| {
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.operation == operation(&reply).unwrap() && t.state == "sent")
    })
    .await?;
    drop(link);
    h.stop().await
}
#[tokio::test]
async fn outbox_transient_disconnect_recovery_retries_and_reconnect_replays() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let mut link = receiver(&h, &phone).await?;
    let reply = h.core.send_files(vec![source(&h)?], target(&phone)).await?;
    let (first, _) = receive_file(&mut link).await?;
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    db.execute_batch("CREATE TRIGGER refuse_recovery BEFORE UPDATE OF state ON outbox_deliveries WHEN OLD.state='sending' AND NEW.state='queued' BEGIN SELECT RAISE(ABORT,'injected recovery failure'); END;")?;
    let mut events = h.core.subscribe();
    drop(link);
    event_matching(&mut events,|e|matches!(e,pneud::Event::Error {message,..} if message.contains("outbox recovery deferred"))).await?;
    until(|| h.core.connected_devices().is_empty()).await?;
    db.execute_batch("DROP TRIGGER refuse_recovery;")?;
    let mut link = negotiated_phone(&h, &phone, false, true).await?;
    let (replay, _) = receive_file(&mut link).await?;
    assert_eq!(replay, first);
    ack(&mut link, replay.transfer_id).await?;
    until(|| {
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.operation == operation(&reply).unwrap() && t.state == "sent")
    })
    .await?;
    drop(link);
    h.stop().await
}
#[tokio::test]
async fn outbox_captured_authority_rejects_repairing_and_admission_is_idempotent() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("phone")?;
    let link = receiver(&h, &phone).await.context("initial receiver")?;
    let captured = h
        .core
        .ui_snapshot()
        .send_targets
        .into_iter()
        .find(|t| t.device == phone.identity.device_id)
        .unwrap();
    drop(link);
    until(|| h.core.connected_devices().is_empty())
        .await
        .context("initial disconnect")?;
    h.core
        .unpair_device(phone.identity.device_id.clone())
        .await
        .context("unpair original authority")?;
    let mut link = receiver(&h, &phone).await.context("replacement receiver")?;
    let stale = Targets::Captured(vec![pneud::outbox::Authority {
        device: captured.device,
        generation: captured.generation,
        fingerprint: captured.fingerprint.unwrap(),
    }]);
    let path = source(&h)?;
    let reply = h
        .core
        .send_files_request(vec![path.clone()], stale, Some("captured-retry".into()))
        .await?;
    let rejected = admitted(&h, reply.clone())
        .await
        .context("stale admission settles")?;
    assert_eq!(rejected["state"], "failed");
    assert!(
        rejected["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["device"] == serde_json::json!(phone.identity.device_id))
    );
    // Retrying with refreshed inputs still returns the original failed operation.
    let retry = h
        .core
        .send_files_request(vec![path.clone()], all(), Some("captured-retry".into()))
        .await?;
    assert_eq!(retry["operation"], reply["operation"]);
    // Repeating a request recovers the original operation and its now-settled
    // rejections; it must never re-resolve the refreshed pairing inputs.
    assert_eq!(retry["rejected"], rejected["rejected"]);
    let sent = h
        .core
        .send_files_request(vec![path.clone()], all(), Some("send-once".into()))
        .await?;
    // Reuse the picked file. Calling source() again would truncate it while
    // asynchronous staging is reading, correctly rejecting the operation.
    let duplicate = h
        .core
        .send_files_request(vec![path], all(), Some("send-once".into()))
        .await?;
    assert_eq!(sent, duplicate);
    // The socket deadline covers delivery after asynchronous staging finishes.
    let settled = admitted(&h, sent.clone())
        .await
        .context("fresh admission settles")?;
    assert_eq!(settled["state"], "admitted", "{settled}");
    let (file, _) = receive_file(&mut link).await.with_context(|| {
        format!(
            "fresh binding receives file; rows={:?}",
            h.core.ui_snapshot().transfers
        )
    })?;
    ack(&mut link, file.transfer_id).await?;
    until(|| {
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.operation == operation(&sent).unwrap() && t.state == "sent")
    })
    .await?;
    let db = rusqlite::Connection::open(h.core.paths.data.join("state.sqlite3"))?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM outbox_artifacts", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    h.core.dismiss_transfer(operation(&sent)?).await?;
    assert!(
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .all(|t| t.operation != operation(&sent).unwrap())
    );
    assert_eq!(
        db.query_row("SELECT state FROM outbox_deliveries", [], |r| r
            .get::<_, String>(0))?,
        "sent"
    );
    drop(link);
    h.stop().await.context("stop captured-authority fixture")
}
#[tokio::test]
async fn outbox_per_recipient_retry_and_cancel_work_with_sibling_awaiting_receipt() -> Result<()> {
    let h = Harness::new().await?;
    let p = h.phone("a")?;
    let q = h.phone("b")?;
    let mut a = receiver(&h, &p).await?;
    let mut b = receiver(&h, &q).await?;
    let reply = h.core.send_files(vec![source(&h)?], all()).await?;
    let (first, _) = receive_file(&mut a).await?;
    let (second, _) = receive_file(&mut b).await?;
    write_frame(
        &mut a,
        &Message::Nack(Nack {
            transfer_id: first.transfer_id.clone(),
            code: NackCode::UnsupportedKind,
            retryable: false,
            message: None,
        }),
    )
    .await?;
    until(|| {
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.device == p.identity.device_id && t.state == "failed")
    })
    .await?;
    h.core
        .retry_transfer(operation(&reply)?, p.identity.device_id.clone())
        .await?;
    let (retry, _) = receive_file(&mut a).await?;
    assert_eq!(retry, first);
    h.core
        .cancel_transfer(operation(&reply)?, Some(p.identity.device_id.clone()))
        .await?;
    assert!(
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.device == q.identity.device_id && t.state == "sending")
    );
    ack(&mut b, second.transfer_id).await?;
    until(|| {
        h.core
            .ui_snapshot()
            .transfers
            .iter()
            .any(|t| t.device == q.identity.device_id && t.state == "sent")
    })
    .await?;
    drop(a);
    drop(b);
    h.stop().await
}
