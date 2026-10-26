use super::*;
use pneu_proto::{PeerSetting, PeerSettings};
async fn pair() -> Result<(tempfile::TempDir, Store, Id, Id)> {
    let dir = crate::test_support::scratch()?;
    let s = Store::open(&dir.path().join("state.sqlite3"))?;
    let local = Id::from_bytes([1; 16]);
    let peer = Id::from_bytes([2; 16]);
    s.pair(peer.clone(), Hash::new("a".repeat(64))?).await?;
    Ok((dir, s, local, peer))
}
fn snapshot(settings: Vec<PeerSetting>) -> PeerSettings {
    PeerSettings {
        transfer_id: Id::from_bytes(rand::random()),
        settings,
    }
}
#[tokio::test]
async fn pair_settings_roundtrip_lww_tie_break_and_exact_pending_replay() -> Result<()> {
    let (_dir, a, local, peer) = pair().await?;
    let dir = crate::test_support::scratch()?;
    let b = Store::open(&dir.path().join("state.sqlite3"))?;
    b.pair(local.clone(), Hash::new("b".repeat(64))?).await?;
    let key = format!("{local}/{peer}/clipboard");
    let fields = a
        .settings_edit(local.clone(), peer.clone(), false, key.clone(), false)
        .await?;
    let pending = a
        .settings_outgoing(local.clone(), peer.clone(), false)
        .await?;
    assert_eq!(pending.settings, fields);
    assert_eq!(
        a.settings_outgoing(local.clone(), peer.clone(), false)
            .await?,
        pending
    );
    let (duplicate, merged) = b
        .settings_merge(peer.clone(), local.clone(), false, pending.clone())
        .await?;
    assert!(!duplicate);
    assert_eq!(merged, fields);
    assert!(
        b.settings_merge(peer.clone(), local.clone(), false, pending.clone())
            .await?
            .0
    );
    a.settings_ack(peer.clone(), pending.transfer_id).await?;
    assert!(
        a.settings_status(peer.clone())
            .await?
            .iter()
            .all(|(_, p)| !*p)
    );
    let mut tie = fields.clone();
    let field = tie.iter_mut().find(|f| f.key == key).unwrap();
    field.enabled = true;
    field.writer_device = peer.clone();
    let (_, merged) = a
        .settings_merge(local.clone(), peer.clone(), false, snapshot(tie))
        .await?;
    assert!(
        merged.iter().find(|f| f.key == key).unwrap().enabled,
        "higher device wins a clock tie"
    );
    let (_, merged) = a
        .settings_merge(local.clone(), peer.clone(), false, snapshot(fields))
        .await?;
    assert!(
        merged.iter().find(|f| f.key == key).unwrap().enabled,
        "older replay cannot replace winner"
    );
    Ok(())
}
#[tokio::test]
async fn pair_settings_invalid_snapshot_rolls_back_every_field_and_clock() -> Result<()> {
    let (_dir, s, local, peer) = pair().await?;
    let original = s
        .settings_fields(local.clone(), peer.clone(), false)
        .await?;
    let mut invalid = original.clone();
    invalid[0].hlc = 10;
    invalid[0].enabled = false;
    invalid[1].enabled = false; // Same tuple with conflicting value.
    assert!(
        s.settings_merge(local.clone(), peer.clone(), false, snapshot(invalid))
            .await
            .unwrap_err()
            .is::<settings::SettingsMalformed>()
    );
    assert_eq!(
        s.settings_fields(local.clone(), peer.clone(), false)
            .await?,
        original
    );
    let seen: String = s
        .call(|c| Ok(c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?))
        .await?;
    assert_eq!(seen, "0");
    let foreign = Id::from_bytes([3; 16]);
    let mut invalid = original.clone();
    invalid[0].key = format!("{local}/{foreign}/clipboard");
    assert!(
        s.settings_merge(local.clone(), peer.clone(), false, snapshot(invalid))
            .await
            .unwrap_err()
            .is::<settings::SettingsUnexpected>()
    );
    assert_eq!(
        s.settings_fields(local.clone(), peer.clone(), false)
            .await?,
        original
    );
    let mut invalid = original.clone();
    invalid[0].hlc = u64::MAX;
    assert!(
        s.settings_merge(local.clone(), peer.clone(), false, snapshot(invalid))
            .await
            .unwrap_err()
            .is::<settings::SettingsMalformed>()
    );
    assert_eq!(s.settings_fields(local, peer, false).await?, original);
    Ok(())
}
#[tokio::test]
async fn pair_settings_offline_edit_survives_restart_and_preserves_unacked_payload() -> Result<()> {
    let (dir, s, local, peer) = pair().await?;
    s.settings_support(peer.clone(), true).await?;
    let first = s
        .settings_outgoing(local.clone(), peer.clone(), true)
        .await?;
    let key = format!("{peer}/{local}/automatic_media");
    s.settings_edit(local.clone(), peer.clone(), true, key.clone(), false)
        .await?;
    assert_eq!(
        s.settings_outgoing(local.clone(), peer.clone(), true)
            .await?,
        first
    );
    drop(s);
    let s = Store::open(&dir.path().join("state.sqlite3"))?;
    assert_eq!(
        s.settings_outgoing(local.clone(), peer.clone(), true)
            .await?,
        first
    );
    assert!(
        s.settings_status(peer.clone())
            .await?
            .iter()
            .find(|(f, _)| f.key == key)
            .unwrap()
            .1
    );
    s.settings_ack(peer.clone(), first.transfer_id.clone())
        .await?;
    let next = s.settings_outgoing(local, peer, true).await?;
    assert!(next.settings.iter().any(|f| f.key == key && !f.enabled));
    assert_ne!(next.transfer_id, first.transfer_id);
    Ok(())
}
#[tokio::test]
async fn pair_settings_receipts_are_bounded_indexed_and_share_content_namespace() -> Result<()> {
    let (_dir, s, local, peer) = pair().await?;
    let state = s
        .settings_fields(local.clone(), peer.clone(), false)
        .await?;
    let first = snapshot(state.clone());
    s.settings_merge(local.clone(), peer.clone(), false, first.clone())
        .await?;
    let text = Clipboard {
        transfer_id: first.transfer_id.clone(),
        text: "collision".into(),
        hlc: None,
        source_device: None,
        created_at: 1,
        origin: ClipboardOrigin::Clipboard,
    };
    assert_eq!(
        save_failure_code(&s.claim_text(peer.clone(), text).await.err().unwrap()),
        NackCode::IdConflict
    );
    let mut conflict = first.clone();
    conflict.settings[0].hlc = 5;
    conflict.settings[0].enabled = false;
    assert_eq!(
        save_failure_code(
            &s.settings_merge(local.clone(), peer.clone(), false, conflict)
                .await
                .unwrap_err()
        ),
        NackCode::IdConflict
    );
    let p = peer.clone();
    let meta = serde_json::to_string(&Metadata::Settings(first.clone()))?;
    s.call(move |c| {
        let tx = c.transaction()?;
        for i in 1..settings::SETTINGS_RECEIPT_WINDOW {
            tx.execute(
                "INSERT INTO settings_receipts VALUES(?,?,?)",
                params![p.as_str(), format!("{i:032x}"), meta],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await?;
    let mut changed = state;
    changed[0].hlc = 5;
    changed[0].enabled = false;
    let changed = snapshot(changed);
    s.call(|c| {
        c.execute_batch("CREATE TRIGGER fail_prune BEFORE DELETE ON settings_receipts BEGIN SELECT RAISE(ABORT,'injected pruning failure'); END;")?;
        Ok(())
    }).await?;
    assert!(
        s.settings_merge(local.clone(), peer.clone(), false, changed.clone())
            .await
            .is_err()
    );
    assert!(
        s.receipt(peer.clone(), changed.transfer_id.clone())
            .await?
            .is_none()
    );
    assert!(
        s.settings_fields(local.clone(), peer.clone(), false)
            .await?[0]
            .enabled
    );
    let count: i64 = s
        .call(|c| {
            assert_eq!(
                c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r
                    .get::<_, String>(0))?,
                "0"
            );
            c.execute_batch("DROP TRIGGER fail_prune;")?;
            Ok(c.query_row("SELECT count(*) FROM settings_receipts", [], |r| r.get(0))?)
        })
        .await?;
    assert_eq!(count, settings::SETTINGS_RECEIPT_WINDOW);
    assert!(
        !s.settings_merge(local.clone(), peer.clone(), false, changed.clone())
            .await?
            .0
    );
    assert!(
        s.settings_merge(local.clone(), peer.clone(), false, changed)
            .await?
            .0
    );
    assert!(
        s.receipt(peer.clone(), first.transfer_id.clone())
            .await?
            .is_none(),
        "oldest receipt was pruned"
    );
    assert!(
        !s.settings_merge(local.clone(), peer.clone(), false, first.clone())
            .await?
            .0,
        "evicted snapshot can safely re-merge"
    );
    assert!(
        s.settings_merge(local.clone(), peer.clone(), false, first)
            .await?
            .0,
        "re-recorded replay deduplicates"
    );
    let (count,plan):(i64,String)=s.call(|c|Ok((c.query_row("SELECT count(*) FROM settings_receipts",[],|r|r.get(0))?,c.query_row("EXPLAIN QUERY PLAN SELECT meta FROM settings_receipts WHERE device='x' AND id='y'",[],|r|r.get(3))?))).await?;
    assert_eq!(count, settings::SETTINGS_RECEIPT_WINDOW);
    assert!(plan.contains("INDEX"));
    assert!(
        !s.settings_fields(local, peer, false).await?[0].enabled,
        "evicted older replay cannot overwrite a winner"
    );
    Ok(())
}
#[tokio::test]
async fn pair_settings_ack_retention_failure_preserves_exact_pending_snapshot() -> Result<()> {
    let (dir, s, local, peer) = pair().await?;
    let outgoing = s
        .settings_outgoing(local.clone(), peer.clone(), false)
        .await?;
    s.call(|c| {
        c.execute_batch("CREATE TRIGGER fail_retention BEFORE INSERT ON settings_replay BEGIN SELECT RAISE(ABORT,'injected retention failure'); END;")?;
        Ok(())
    }).await?;
    assert!(
        s.settings_ack(peer.clone(), outgoing.transfer_id.clone())
            .await
            .is_err()
    );
    assert!(
        s.settings_status(peer.clone())
            .await?
            .iter()
            .all(|(_, pending)| *pending)
    );
    drop(s);
    let s = Store::open(&dir.path().join("state.sqlite3"))?;
    assert_eq!(
        s.settings_outgoing(local.clone(), peer.clone(), false)
            .await?,
        outgoing
    );
    s.call(|c| {
        c.execute_batch("DROP TRIGGER fail_retention;")?;
        Ok(())
    })
    .await?;
    s.settings_ack(peer.clone(), outgoing.transfer_id.clone())
        .await?;
    assert_eq!(s.settings_outgoing(local, peer, false).await?, outgoing);
    Ok(())
}
#[tokio::test]
async fn pair_settings_10000_unchanged_reconnects_replay_acknowledged_id_across_restart()
-> Result<()> {
    let (dir_a, mut a, local, peer) = pair().await?;
    let dir_b = crate::test_support::scratch()?;
    let mut b = Store::open(&dir_b.path().join("state.sqlite3"))?;
    b.pair(local.clone(), Hash::new("b".repeat(64))?).await?;
    let first = a
        .settings_outgoing(local.clone(), peer.clone(), false)
        .await?;
    for reconnect in 0..10_000 {
        if reconnect == 5_000 {
            drop((a, b));
            a = Store::open(&dir_a.path().join("state.sqlite3"))?;
            b = Store::open(&dir_b.path().join("state.sqlite3"))?;
        }
        let outgoing = a
            .settings_outgoing(local.clone(), peer.clone(), false)
            .await?;
        assert_eq!(outgoing, first);
        let (deduplicated, fields) = b
            .settings_merge(peer.clone(), local.clone(), false, outgoing.clone())
            .await?;
        assert_eq!(deduplicated, reconnect != 0);
        assert_eq!(fields, first.settings);
        a.settings_ack(peer.clone(), outgoing.transfer_id).await?;
    }
    let count: i64 = b
        .call(|c| Ok(c.query_row("SELECT count(*) FROM settings_receipts", [], |r| r.get(0))?))
        .await?;
    assert_eq!(count, 1);
    // A no-op UI write also keeps the stamped state and operation ID stable.
    a.settings_edit(
        local.clone(),
        peer.clone(),
        false,
        first.settings[0].key.clone(),
        true,
    )
    .await?;
    assert_eq!(a.settings_outgoing(local, peer, false).await?, first);
    Ok(())
}
#[tokio::test]
async fn pair_settings_changes_and_reconnects_converge_past_receipt_window_and_restart()
-> Result<()> {
    let (dir_a, mut a, local, peer) = pair().await?;
    let dir_b = crate::test_support::scratch()?;
    let mut b = Store::open(&dir_b.path().join("state.sqlite3"))?;
    b.pair(local.clone(), Hash::new("b".repeat(64))?).await?;
    let first = a
        .settings_outgoing(local.clone(), peer.clone(), false)
        .await?;
    b.settings_merge(peer.clone(), local.clone(), false, first.clone())
        .await?;
    a.settings_ack(peer.clone(), first.transfer_id.clone())
        .await?;
    let a_key = format!("{local}/{peer}/clipboard");
    let b_key = format!("{peer}/{local}/clipboard");
    for change in 0..4_200 {
        // Both endpoints edit distinct fields before exchanging full state.
        let enabled = change % 2 != 0;
        a.settings_edit(local.clone(), peer.clone(), false, a_key.clone(), enabled)
            .await?;
        b.settings_edit(peer.clone(), local.clone(), false, b_key.clone(), enabled)
            .await?;
        let outgoing = a
            .settings_outgoing(local.clone(), peer.clone(), false)
            .await?;
        if change == 2_100 {
            // Restart with an operation still pending and both edits durable.
            drop((a, b));
            a = Store::open(&dir_a.path().join("state.sqlite3"))?;
            b = Store::open(&dir_b.path().join("state.sqlite3"))?;
            assert_eq!(
                a.settings_outgoing(local.clone(), peer.clone(), false)
                    .await?,
                outgoing
            );
        }
        b.settings_merge(peer.clone(), local.clone(), false, outgoing.clone())
            .await?;
        // Simulate a lost ACK and reconnect before acknowledgment.
        assert_eq!(
            a.settings_outgoing(local.clone(), peer.clone(), false)
                .await?,
            outgoing
        );
        assert!(
            b.settings_merge(peer.clone(), local.clone(), false, outgoing.clone())
                .await?
                .0
        );
        a.settings_ack(peer.clone(), outgoing.transfer_id).await?;
        let response = b
            .settings_outgoing(peer.clone(), local.clone(), false)
            .await?;
        let (_, merged) = a
            .settings_merge(local.clone(), peer.clone(), false, response.clone())
            .await?;
        assert_eq!(merged, response.settings);
        assert!(merged.iter().all(|f| f.enabled == enabled));
        b.settings_ack(local.clone(), response.transfer_id).await?;
        // Send the merged state, then reconnect again after the ACK.
        let converged = a
            .settings_outgoing(local.clone(), peer.clone(), false)
            .await?;
        assert_eq!(converged.settings, merged);
        assert_eq!(
            b.settings_merge(peer.clone(), local.clone(), false, converged.clone())
                .await?
                .1,
            merged
        );
        a.settings_ack(peer.clone(), converged.transfer_id.clone())
            .await?;
        assert_eq!(
            a.settings_outgoing(local.clone(), peer.clone(), false)
                .await?,
            converged
        );
        assert!(
            b.settings_merge(peer.clone(), local.clone(), false, converged.clone())
                .await?
                .0
        );
        a.settings_ack(peer.clone(), converged.transfer_id).await?;
    }
    let expected = b
        .settings_fields(peer.clone(), local.clone(), false)
        .await?;
    assert!(
        !b.settings_merge(peer.clone(), local.clone(), false, first.clone())
            .await?
            .0
    );
    assert!(
        b.settings_merge(peer.clone(), local.clone(), false, first)
            .await?
            .0
    );
    assert_eq!(b.settings_fields(peer, local, false).await?, expected);
    let count: i64 = b
        .call(|c| Ok(c.query_row("SELECT count(*) FROM settings_receipts", [], |r| r.get(0))?))
        .await?;
    assert_eq!(count, settings::SETTINGS_RECEIPT_WINDOW);
    Ok(())
}
#[tokio::test]
async fn pair_settings_own_id_and_self_direction_never_mutate_state() -> Result<()> {
    let (_dir, s, local, peer) = pair().await?;
    // Deliberately create a corrupt pre-existing self binding at the Store layer
    // (which has no local identity) to exercise defense beyond HELLO admission.
    s.pair(local.clone(), Hash::new("c".repeat(64))?).await?;
    assert!(
        s.settings_fields(local.clone(), local.clone(), false)
            .await
            .is_err()
    );
    assert!(
        s.settings_outgoing(local.clone(), local.clone(), false)
            .await
            .is_err()
    );
    let key = format!("{local}/{local}/clipboard");
    assert!(
        s.settings_edit(local.clone(), local.clone(), false, key.clone(), false)
            .await
            .is_err()
    );
    let own = snapshot(vec![PeerSetting {
        key,
        enabled: false,
        hlc: 1,
        writer_device: local.clone(),
    }]);
    assert!(
        s.settings_merge(local.clone(), local.clone(), false, own.clone())
            .await
            .unwrap_err()
            .is::<settings::SettingsUnexpected>()
    );
    let count: i64 = s.call(|c| Ok(c.query_row("SELECT (SELECT count(*) FROM peer_settings)+(SELECT count(*) FROM settings_support)+(SELECT count(*) FROM settings_outgoing)+(SELECT count(*) FROM settings_receipts)", [], |r| r.get(0))?)).await?;
    assert_eq!(count, 0);
    let mut invalid = s
        .settings_fields(local.clone(), peer.clone(), false)
        .await?;
    let original = invalid.clone();
    invalid[0] = own.settings[0].clone();
    assert!(
        s.settings_merge(local.clone(), peer.clone(), false, snapshot(invalid))
            .await
            .is_err()
    );
    assert_eq!(s.settings_fields(local, peer, false).await?, original);
    Ok(())
}
#[tokio::test]
async fn android_meshstate_exact_full_snapshot_decodes_and_merges() -> Result<()> {
    let (_dir, s, local, phone) = pair().await?;
    // MeshState.PairSettings.snapshot sorts keys and emits hlc as a JSON u64,
    // writer_device and enabled for all three phone/computer fields.
    let payload = format!(
        r#"{{"transfer_id":"33333333333333333333333333333333","settings":[{{"key":"{local}/{phone}/clipboard","enabled":true,"hlc":0,"writer_device":"{local}"}},{{"key":"{phone}/{local}/automatic_media","enabled":false,"hlc":1,"writer_device":"{phone}"}},{{"key":"{phone}/{local}/clipboard","enabled":true,"hlc":0,"writer_device":"{local}"}}]}}"#
    );
    let mut frame = vec![0x21];
    frame.extend_from_slice(payload.as_bytes());
    let mut wire = (frame.len() as u32).to_be_bytes().to_vec();
    wire.extend(frame);
    let message = pneu_proto::read_frame(&mut wire.as_slice()).await?.unwrap();
    let pneu_proto::Message::PeerSettings(snapshot) = message else {
        panic!("wrong frame")
    };
    let (_, fields) = s.settings_merge(local, phone, true, snapshot).await?;
    assert_eq!(fields.len(), 3);
    assert!(
        !fields
            .iter()
            .find(|f| f.key.ends_with("automatic_media"))
            .unwrap()
            .enabled
    );
    Ok(())
}

#[tokio::test]
async fn canceled_settings_caller_still_publishes_policy_and_connection_wakeup() -> Result<()> {
    let (_dir, s, local, peer) = pair().await?;
    s.settings_fields(local.clone(), peer.clone(), false)
        .await?;
    let mut changes = s.settings_changed.subscribe();
    changes.borrow_and_update();
    let (entered, wait) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let held = s.clone();
    let holder = tokio::spawn(async move {
        held.call(move |_| {
            let _ = entered.send(());
            blocked.recv()?;
            Ok(())
        })
        .await
    });
    wait.await?;
    let key = format!("{local}/{peer}/clipboard");
    let mut edit = Box::pin(s.settings_edit(local, peer.clone(), false, key.clone(), false));
    assert!(futures_util::poll!(edit.as_mut()).is_pending());
    drop(edit);
    release.send(())?;
    holder.await??;
    s.drain().await;
    assert!(
        changes.has_changed()?,
        "committed edit relied on its canceled caller to wake the connection"
    );
    assert!(
        !s.pair_settings
            .lock()
            .unwrap()
            .get(&peer)
            .unwrap()
            .iter()
            .find(|f| f.key == key)
            .unwrap()
            .enabled
    );
    assert!(
        s.settings_status(peer)
            .await?
            .iter()
            .any(|(f, pending)| f.key == key && *pending && !f.enabled)
    );
    Ok(())
}

#[tokio::test]
async fn settings_snapshot_is_encoded_before_durable_publication() -> Result<()> {
    let (_dir, s, local, peer) = pair().await?;
    s.settings_fields(local.clone(), peer.clone(), true).await?;
    let device = peer.clone();
    s.call(move |c| {
        c.execute(
            "INSERT INTO peer_settings VALUES(?, 'invalid/key', 1, '0', ?)",
            params![device.as_str(), device.as_str()],
        )?;
        Ok(())
    })
    .await?;
    assert!(
        s.settings_outgoing(local, peer, true).await.is_err(),
        "invalid snapshot was committed"
    );
    let count: i64 = s
        .call(|c| Ok(c.query_row("SELECT count(*) FROM settings_outgoing", [], |r| r.get(0))?))
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn startup_retires_legacy_text_regardless_of_local_or_remote_policy() -> Result<()> {
    for remote in [false, true] {
        let (dir, s, local, peer) = pair().await?;
        let other = Id::from_bytes([3; 16]);
        s.pair(other.clone(), Hash::new("b".repeat(64))?).await?;
        s.enqueue_clipboard(other.clone(), "other peer".into())
            .await?;
        s.enqueue_clipboard(peer.clone(), "must never reappear".into())
            .await?;
        let key = format!("{local}/{peer}/clipboard");
        if remote {
            let mut fields = s
                .settings_fields(local.clone(), peer.clone(), false)
                .await?;
            let field = fields.iter_mut().find(|f| f.key == key).unwrap();
            field.enabled = false;
            field.hlc = 1;
            field.writer_device = peer.clone();
            s.settings_merge(local.clone(), peer.clone(), false, snapshot(fields))
                .await?;
        } else {
            s.settings_edit(local.clone(), peer.clone(), false, key.clone(), false)
                .await?;
        }
        // Settings do not consume retired rows. Startup retires every peer's
        // migration fixture regardless of policy.
        assert!(s.first_outgoing(peer.clone()).await?.is_some());
        s.settings_edit(local.clone(), peer.clone(), false, key, true)
            .await?;
        s.drain().await;
        drop(s);
        let mut restarted = Store::open(&dir.path().join("state.sqlite3"))?;
        restarted.mesh = true;
        restarted.local_device = Some(local);
        assert!(restarted.first_outgoing(peer).await?.is_none());
        assert!(restarted.first_outgoing(other).await?.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn enabling_policy_does_not_consume_retired_outbox() -> Result<()> {
    let (_dir, s, local, peer) = pair().await?;
    let key = format!("{local}/{peer}/clipboard");
    s.settings_edit(local.clone(), peer.clone(), false, key.clone(), false)
        .await?;
    // Model a database written by the preceding implementation, which left
    // disabled text for a connected consumer to clean up.
    s.enqueue_clipboard(peer.clone(), "old disabled row".into())
        .await?;
    s.settings_edit(local, peer.clone(), false, key, true)
        .await?;
    assert_eq!(
        s.first_outgoing(peer).await?.unwrap().1.text,
        "old disabled row"
    );
    Ok(())
}

#[tokio::test]
async fn lifecycle_and_capability_transitions_cannot_resurrect_dropped_text() -> Result<()> {
    for transition in ["unpair", "repair", "capability"] {
        let (dir, s, local, peer) = pair().await?;
        s.enqueue_clipboard(peer.clone(), "obsolete".into()).await?;
        let fp = Hash::new("a".repeat(64))?;
        match transition {
            "unpair" => {
                s.unpair_then(peer.clone(), |_| {}).await?;
                s.pair(peer.clone(), fp).await?;
            }
            "repair" => s.pair(peer.clone(), fp).await?,
            _ => {
                let key = format!("{local}/{peer}/clipboard");
                s.settings_edit(local.clone(), peer.clone(), false, key.clone(), false)
                    .await?;
                let mut hello = pneu_proto::Hello {
                    version: 1,
                    device_id: peer.clone(),
                    name: "peer".into(),
                    role: Some(pneu_proto::DeviceRole::Computer),
                    addresses: None,
                    paused: None,
                    capabilities: vec!["mesh".into()],
                };
                let current = Arc::new(AtomicU64::new(1));
                s.remember_peer_hello(hello.clone(), current.clone(), 1, false)
                    .await?;
                assert!(s.first_outgoing(peer.clone()).await?.is_some());
                hello.capabilities = vec!["clipboard_text".into()];
                s.remember_peer_hello(hello.clone(), current.clone(), 1, false)
                    .await?;
                s.settings_edit(local, peer.clone(), false, key, true)
                    .await?;
                hello.capabilities.push("mesh".into());
                s.remember_peer_hello(hello, current, 1, false).await?;
            }
        }
        s.drain().await;
        drop(s);
        let restarted = Store::open(&dir.path().join("state.sqlite3"))?;
        assert!(
            restarted.first_outgoing(peer).await?.is_none(),
            "{transition}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn retired_outbox_failure_does_not_block_local_or_remote_settings() -> Result<()> {
    for remote in [false, true] {
        let (_dir, s, local, peer) = pair().await?;
        let before = s
            .settings_fields(local.clone(), peer.clone(), false)
            .await?;
        let original = s
            .enqueue_clipboard(peer.clone(), "retained on rollback".into())
            .await?;
        s.call(|c| {
            c.execute_batch("CREATE TRIGGER fail_outbox_delete BEFORE DELETE ON outgoing BEGIN SELECT RAISE(ABORT,'injected delete failure'); END;")?;
            Ok(())
        }).await?;
        let key = format!("{local}/{peer}/clipboard");
        if remote {
            let mut fields = before.clone();
            let field = fields.iter_mut().find(|f| f.key == key).unwrap();
            field.enabled = false;
            field.hlc = 1;
            field.writer_device = peer.clone();
            s.settings_merge(local.clone(), peer.clone(), false, snapshot(fields))
                .await?;
        } else {
            s.settings_edit(local.clone(), peer.clone(), false, key.clone(), false)
                .await?;
        }
        let after = s.settings_fields(local, peer.clone(), false).await?;
        assert!(!after.iter().find(|field| field.key == key).unwrap().enabled);
        assert_eq!(s.pair_settings.lock().unwrap()[&peer], after);
        assert_eq!(s.settings_generation(&peer), 1);
        assert_eq!(s.first_outgoing(peer).await?.unwrap().1, original);
    }
    Ok(())
}
