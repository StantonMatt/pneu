use super::*;
use anyhow::Result;
use members::*;
use pneu_proto::*;
fn id(n: u8) -> Id {
    Id::from_bytes([n; 16])
}
fn member(n: u8) -> Member {
    Member {
        device_id: id(n),
        fingerprint: Hash::new(format!("{n:064x}")).unwrap(),
        role: DeviceRole::Computer,
        name: format!("Device {n}"),
        status: MemberStatus::Member,
        hlc: n as u64,
        writer_device: id(1),
        reach: None,
        readmits: None,
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    s: Store,
    f: SetupFence,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let dir = crate::test_support::scratch()?;
        let s = Store::open(&dir.path().join("state.sqlite3"))?;
        let a = member(1);
        s.pair(a.device_id.clone(), a.fingerprint.clone()).await?;
        s.call(|c| {
            c.execute("UPDATE peers SET role='computer',name='Introducer'", [])?;
            Ok(())
        })
        .await?;
        s.setup_link(id(1), SetupScope::Enabled).await?;
        let mut f = SetupFence {
            binding_generation: 1,
            generation: 0,
            device: id(1),
            fingerprint: a.fingerprint.clone(),
            connection: 1,
            current: Arc::new(AtomicU64::new(1)),
        };
        assert!(
            !s.setup_activate(
                f.clone(),
                member(2),
                a,
                SetupHello {
                    scope: SetupScope::Enabled,
                    setup_id: Some(id(0))
                },
                |_| {}
            )
            .await?
        );
        f.generation = s.setup_state().generation;
        Ok(Self { _dir: dir, s, f })
    }
    fn snapshot(&self, other: Vec<Member>) -> Members {
        let mut roster = self.s.setup_state().roster;
        for m in other {
            roster.retain(|v| {
                !(v.device_id == m.device_id && v.fingerprint == m.fingerprint)
                    && (m.status != MemberStatus::Member
                        || v.status != MemberStatus::Member
                        || v.device_id != m.device_id)
            });
            roster.push(m);
        }
        Members {
            transfer_id: Id::from_bytes(rand::random()),
            setup_id: id(0),
            members: roster,
        }
    }
    async fn merge(&self, m: Members) -> Result<bool> {
        self.s
            .members_merge(self.f.clone(), member(2), m, Vec::new(), |_| {})
            .await
    }
    // Legacy persisted barriers have no origin bits and conservatively ask.
    // The v2 vector runner separately exercises real authored local removals.
    async fn legacy_authored_removal(&self, m: Member) -> Result<()> {
        self.merge(self.snapshot(vec![m.clone()])).await?;
        self.s.call(move |c| {c.execute("UPDATE setup_entries SET payload=json_remove(payload,'$.authored','$.revoked_binding') WHERE device=? AND fingerprint=?",params![m.device_id.as_str(),m.fingerprint.as_str()])?;Ok(())}).await?;
        ReadmitVectors::reload(&self.s).await
    }
    async fn pin(&self, n: u8) -> Result<Option<Hash>> {
        Ok(self
            .s
            .peers()
            .await?
            .into_iter()
            .find(|(_, v)| *v == id(n))
            .map(|(p, _)| p))
    }
}
#[tokio::test]
async fn setup_remote_scope_tracks_hello_without_changing_local_scope() -> Result<()> {
    let f = Fixture::new().await?;
    assert_eq!(
        f.s.setup_remote_scopes().get(&id(1)),
        Some(&SetupScope::Enabled)
    );
    let mut changed = f.s.setup_changed.subscribe();
    for scope in [SetupScope::Ask, SetupScope::Disabled, SetupScope::Enabled] {
        changed.borrow_and_update();
        f.s.setup_activate(
            f.f.clone(),
            member(2),
            member(1),
            SetupHello {
                scope,
                setup_id: Some(id(0)),
            },
            |_| {},
        )
        .await?;
        assert_eq!(f.s.setup_remote_scopes().get(&id(1)), Some(&scope));
        assert!(changed.has_changed()?);
        let link = f.s.setup_state().links.remove(0);
        assert_eq!(link.scope, SetupScope::Enabled);
        assert!(!link.prompt);
        assert_eq!(link.participating, scope == SetupScope::Enabled);
    }
    // Failed or obsolete HELLO work must not publish UI metadata either.
    let before = f.s.setup_remote_scopes();
    f.s.fail_next("members_before_commit", 1);
    assert!(
        f.s.setup_activate(
            f.f.clone(),
            member(2),
            member(1),
            SetupHello {
                scope: SetupScope::Disabled,
                setup_id: None
            },
            |_| {},
        )
        .await
        .is_err()
    );
    assert_eq!(f.s.setup_remote_scopes(), before);
    f.f.current.store(2, Ordering::SeqCst);
    assert!(
        f.s.setup_activate(
            f.f.clone(),
            member(2),
            member(1),
            SetupHello {
                scope: SetupScope::Disabled,
                setup_id: None
            },
            |_| {},
        )
        .await
        .is_err()
    );
    assert_eq!(f.s.setup_remote_scopes(), before);
    Ok(())
}

#[tokio::test]
async fn setup_remote_scope_is_unknown_after_restart_or_binding_end() -> Result<()> {
    let f = Fixture::new().await?;
    let path = f._dir.path().join("state.sqlite3");
    // Changing an unrelated binding must preserve the last HELLO here.
    f.s.pair(id(3), member(3).fingerprint).await?;
    assert_eq!(
        f.s.setup_remote_scopes().get(&id(1)),
        Some(&SetupScope::Enabled)
    );
    assert!(!f.s.setup_remote_scopes().contains_key(&id(3)));
    f.s.unpair_then(id(3), |_| {}).await?;
    assert_eq!(
        f.s.setup_remote_scopes().get(&id(1)),
        Some(&SetupScope::Enabled)
    );
    drop(f.s);
    let s = Store::open(&path)?;
    assert!(s.setup_remote_scopes().is_empty());
    s.setup_activate(
        f.f.clone(),
        member(2),
        member(1),
        SetupHello {
            scope: SetupScope::Ask,
            setup_id: Some(id(0)),
        },
        |_| {},
    )
    .await?;
    assert_eq!(s.setup_remote_scopes().get(&id(1)), Some(&SetupScope::Ask));
    s.pair(id(1), member(1).fingerprint).await?;
    assert!(s.setup_remote_scopes().is_empty());
    let mut fence = f.f;
    fence.binding_generation = s.pairing_generation(&id(1)).unwrap();
    s.setup_activate(
        fence,
        member(2),
        member(1),
        SetupHello {
            scope: SetupScope::Disabled,
            setup_id: None,
        },
        |_| {},
    )
    .await?;
    assert_eq!(
        s.setup_remote_scopes().get(&id(1)),
        Some(&SetupScope::Disabled)
    );
    s.unpair_then(id(1), |_| {}).await?;
    assert!(s.setup_remote_scopes().is_empty());
    Ok(())
}

#[tokio::test]
async fn setup_remote_scope_is_cleared_when_setup_is_not_negotiated() -> Result<()> {
    let f = Fixture::new().await?;
    f.s.setup_legacy_ready(f.f.clone()).await?;
    assert!(f.s.setup_remote_scopes().is_empty());
    assert!(!f.s.setup_state().links[0].negotiated);
    assert_eq!(f.s.setup_state().links[0].scope, SetupScope::Enabled);
    Ok(())
}

#[tokio::test]
async fn setup_join_installs_once_with_provenance_zero_readiness_and_pair_defaults() -> Result<()> {
    let f = Fixture::new().await?;
    let mut c = member(3);
    c.reach = Some(MemberReach {
        addresses: vec!["127.0.0.1:40003".into()],
        relay: vec![],
        hlc: 4,
        writer_device: id(3),
    });
    let m = f.snapshot(vec![c]);
    assert!(!f.merge(m.clone()).await?);
    assert_eq!(f.pin(3).await?, Some(member(3).fingerprint));
    assert!(!f.s.peer_established(id(3)).await?);
    let link =
        f.s.setup_state()
            .links
            .into_iter()
            .find(|l| l.device == id(3))
            .unwrap();
    assert_eq!(link.scope, SetupScope::Enabled);
    assert_eq!(link.introduced_by, Some(id(1)));
    assert_eq!(link.introducer_name.as_deref(), Some("Introducer"));
    assert!(
        f.s.settings_fields(id(2), id(3), false)
            .await?
            .iter()
            .all(|v| v.enabled && v.hlc == 0)
    );
    assert!(f.merge(m).await?);
    assert_eq!(f.s.pairing_generation(&id(3)), Some(1));
    Ok(())
}
#[tokio::test]
async fn setup_unchanged_and_reach_replay_preserves_all_peer_owned_state() -> Result<()> {
    let f = Fixture::new().await?;
    let m = f.snapshot(vec![member(3)]);
    f.merge(m.clone()).await?;
    f.s.settings_edit(
        id(2),
        id(3),
        false,
        format!("{}/{}/clipboard", id(2), id(3)),
        false,
    )
    .await?;
    let pending = f.s.settings_outgoing(id(2), id(3), false).await?;
    f.s.mark_peer_established(id(3)).await?;
    f.s.remember_route(id(3), "127.0.0.1:49999".into()).await?;
    // Real schema rows cover durable file/text receipts and a waiting delivery.
    f.s.call(|c| {
        let meta=serde_json::to_string(&Metadata::Text(TextReceipt {hlc:Some(1),source_device:Some(id(3)),transfer_id:id(70),sha256:member(9).fingerprint,created_at:1,origin:ClipboardOrigin::Clipboard}))?;
        c.execute("INSERT INTO clipboard_recent VALUES(?,?,?)",params![id(3).as_str(),id(70).as_str(),meta])?;
        c.execute("INSERT INTO receipts VALUES(?,?,?,'saved.bin')",params![id(3).as_str(),id(71).as_str(),serde_json::to_string(&Metadata::Settings(PeerSettings {transfer_id:id(71),settings:vec![]}))?])?;
        c.execute("INSERT INTO outbox_artifacts(id,operation,meta) VALUES('artifact','operation','{}')",[])?;
        c.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES('artifact',?,1,?)",params![id(3).as_str(),member(3).fingerprint.as_str()])?;Ok(())
    }).await?;
    let before = f.s.setup_state();
    let generation = f.s.pairing_generation(&id(3));
    assert!(f.merge(m.clone()).await?);
    let mut reordered = m;
    reordered.transfer_id = id(8);
    reordered.members.reverse();
    assert!(!f.merge(reordered).await?);
    let mut c = member(3);
    c.reach = Some(MemberReach {
        addresses: vec!["127.0.0.1:40003".into()],
        relay: vec![],
        hlc: 30,
        writer_device: id(3),
    });
    f.merge(f.snapshot(vec![c])).await?;
    assert_eq!(generation, f.s.pairing_generation(&id(3)));
    assert_eq!(before.links, f.s.setup_state().links);
    assert!(f.s.peer_established(id(3)).await?);
    assert_eq!(f.s.settings_outgoing(id(2), id(3), false).await?, pending);
    f.s.call(|c| {
        for table in ["clipboard_recent", "receipts", "outbox_deliveries"] {
            assert_eq!(
                c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
        }
        let routes: String = c.query_row(
            "SELECT addresses FROM peers WHERE id=?",
            [id(3).as_str()],
            |r| r.get(0),
        )?;
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&routes)?[0],
            "127.0.0.1:49999"
        );
        Ok(())
    })
    .await?;
    Ok(())
}
#[tokio::test]
async fn setup_terminal_removal_beats_stale_and_newer_add_across_restart() -> Result<()> {
    for remove_first in [false, true] {
        let f = Fixture::new().await?;
        let add = f.snapshot(vec![member(3)]);
        let mut removed = member(3);
        removed.status = MemberStatus::Removed;
        removed.hlc = 10;
        let removal = f.snapshot(vec![removed]);
        if !remove_first {
            f.merge(add.clone()).await?;
        }
        f.merge(removal).await?;
        let generation = f.s.pairing_generation(&id(3));
        let mut newer = member(3);
        newer.hlc = 20;
        f.merge(f.snapshot(vec![newer])).await?;
        f.merge(add).await?;
        assert_eq!(f.pin(3).await?, None);
        assert_eq!(generation, f.s.pairing_generation(&id(3)));
        assert!(
            f.s.setup_state()
                .roster
                .iter()
                .any(|m| m.device_id == id(3) && m.status == MemberStatus::Removed)
        );
        let path = f._dir.path().join("state.sqlite3");
        drop(f.s);
        let s = Store::open(&path)?;
        assert!(s.peers().await?.values().all(|v| *v != id(3)));
    }
    Ok(())
}
#[tokio::test]
async fn setup_reused_id_pin_conflicts_and_historical_removal_do_not_replace() -> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    let generation = f.s.pairing_generation(&id(3));
    let mut changed = member(3);
    changed.fingerprint = member(9).fingerprint;
    changed.hlc = 30;
    f.merge(f.snapshot(vec![changed.clone()])).await?;
    assert_eq!(f.pin(3).await?, Some(member(3).fingerprint));
    assert_eq!(f.s.pairing_generation(&id(3)), generation);
    changed.status = MemberStatus::Removed;
    changed.hlc = 40;
    f.merge(f.snapshot(vec![changed])).await?;
    assert_eq!(f.pin(3).await?, Some(member(3).fingerprint));
    let mut unique = member(4);
    unique.fingerprint = member(3).fingerprint;
    let mut m = f.snapshot(vec![unique]);
    m.members.retain(|m| m.device_id != id(3));
    f.merge(m).await?;
    assert_eq!(f.pin(4).await?, None);
    Ok(())
}
#[tokio::test]
async fn setup_spoofed_sender_receiver_and_self_pin_reject_atomically() -> Result<()> {
    for which in 0..4 {
        let f = Fixture::new().await?;
        let before = f.s.setup_state();
        let mut m = f.snapshot(vec![member(3)]);
        match which {
            0 => {
                m.members
                    .iter_mut()
                    .find(|m| m.device_id == id(1))
                    .unwrap()
                    .fingerprint = member(9).fingerprint
            }
            1 => {
                m.members
                    .iter_mut()
                    .find(|m| m.device_id == id(2))
                    .unwrap()
                    .fingerprint = member(9).fingerprint
            }
            2 => {
                m.members.retain(|m| m.device_id != id(2));
                m.members
                    .iter_mut()
                    .find(|m| m.device_id == id(3))
                    .unwrap()
                    .fingerprint = member(2).fingerprint;
            }
            _ => m.members.retain(|m| m.device_id != id(1)),
        }
        assert!(f.merge(m).await.unwrap_err().is::<MembersError>());
        assert_eq!(f.s.setup_state(), before);
        assert_eq!(f.pin(3).await?, None);
    }
    Ok(())
}
#[tokio::test]
async fn setup_live_attempt_blocks_introduction_by_id_or_pin() -> Result<()> {
    for (target, pin, blocked) in [
        (Some(id(3)), member(9).fingerprint, true),
        (None, member(3).fingerprint, true),
        (Some(id(4)), member(4).fingerprint, false),
    ] {
        let f = Fixture::new().await?;
        let m = f.snapshot(vec![member(3)]);
        f.s.members_merge(f.f.clone(), member(2), m, vec![(target, pin)], |_| {})
            .await?;
        assert_eq!(f.pin(3).await?.is_none(), blocked);
        if blocked {
            f.s.setup_reconcile_candidates(id(2), Vec::new(), |_| {})
                .await?;
            assert!(f.pin(3).await?.is_some());
        }
    }
    Ok(())
}
#[tokio::test]
async fn setup_ask_disabled_and_id_backstop_do_not_observe_or_receipt() -> Result<()> {
    for kind in 0..4 {
        let f = Fixture::new().await?;
        let mut m = f.snapshot(vec![member(3)]);
        match kind {
            0 => f.s.setup_link(id(1), SetupScope::Ask).await?,
            1 => f.s.setup_link(id(1), SetupScope::Disabled).await?,
            2 => m.setup_id = id(255),
            _ => {
                f.s.call(|c| {
                    c.execute("UPDATE setup_state SET setup_id=NULL", [])?;
                    Ok(())
                })
                .await?;
            }
        }
        assert!(f.merge(m).await.unwrap_err().is::<MembersError>());
        assert_eq!(f.pin(3).await?, None);
        assert_eq!(
            f.s.call(|c| Ok(
                c.query_row("SELECT count(*) FROM members_receipts", [], |r| r
                    .get::<_, i64>(0))?
            ))
            .await?,
            0
        );
    }
    Ok(())
}
#[tokio::test]
async fn setup_equal_tuple_conflicts_future_skew_and_storage_failure_are_atomic() -> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    let before = f.s.setup_state();
    for kind in 0..3 {
        let mut c = member(3);
        match kind {
            0 => c.name = "Conflicting tie".into(),
            1 => c.hlc = crate::now_ms() + 120000,
            _ => f.s.fail_next("members_before_commit", 1),
        }
        let m = f.snapshot(vec![c, member(4)]);
        assert!(f.merge(m).await.is_err());
        assert_eq!(before, f.s.setup_state());
        assert_eq!(f.pin(4).await?, None);
    }
    Ok(())
}
#[tokio::test]
async fn setup_shared_receipt_conflict_and_semantic_order_equality() -> Result<()> {
    let f = Fixture::new().await?;
    let m = f.snapshot(vec![member(3)]);
    f.merge(m.clone()).await?;
    let mut reordered = m.clone();
    reordered.members.reverse();
    assert!(f.merge(reordered).await?);
    let mut conflict = m.clone();
    conflict
        .members
        .iter_mut()
        .find(|m| m.device_id == id(3))
        .unwrap()
        .name = "Different".into();
    assert!(f.merge(conflict).await.unwrap_err().is::<ReceiptConflict>());
    let ps = PeerSettings {
        transfer_id: m.transfer_id,
        settings: f.s.settings_fields(id(2), id(1), false).await?,
    };
    assert!(
        f.s.settings_merge(id(2), id(1), false, ps)
            .await
            .unwrap_err()
            .is::<ReceiptConflict>()
    );
    Ok(())
}
#[tokio::test]
async fn setup_last_member_dissolves_retains_tombstones_and_fences_old_work() -> Result<()> {
    let f = Fixture::new().await?;
    let old = f.f.clone();
    let m = f.snapshot(vec![member(3)]);
    f.merge(m).await?;
    f.s.setup_remove(id(2), id(3), |_| {}).await?;
    assert_eq!(f.s.setup_state().setup_id, Some(id(0)));
    f.s.setup_remove(id(2), id(1), |_| {}).await?;
    let s = f.s.setup_state();
    assert_eq!(s.setup_id, None);
    assert!(s.roster.is_empty());
    assert!(s.generation > old.generation);
    assert_eq!(
        f.s.call(
            |c| Ok(c.query_row("SELECT count(*) FROM setup_entries", [], |r| r
                .get::<_, i64>(0))?)
        )
        .await?,
        2
    );
    assert!(f.s.members_outgoing(old).await.is_err());
    Ok(())
}
#[tokio::test]
async fn setup_scoped_unpair_and_own_tombstone_only_withdraw_locally() -> Result<()> {
    for own in [false, true] {
        let f = Fixture::new().await?;
        f.merge(f.snapshot(vec![member(3)])).await?;
        f.s.pair(id(4), member(4).fingerprint).await?;
        f.s.pair(id(5), member(5).fingerprint).await?;
        // This side opted in, but the other never did: it is not exported.
        f.s.setup_link(id(5), SetupScope::Enabled).await?;
        assert!(!f.s.setup_scoped(&id(5)));
        if own {
            let mut m =
                f.s.setup_state()
                    .roster
                    .into_iter()
                    .find(|m| m.device_id == id(2))
                    .unwrap();
            m.status = MemberStatus::Removed;
            m.reach = None;
            m.hlc = crate::now_ms() + 1;
            f.merge(f.snapshot(vec![m])).await?;
        } else {
            f.s.setup_withdraw(f.f.clone(), |_| {}).await?;
        }
        let s = f.s.setup_state();
        assert!(s.withdrawn);
        assert_eq!(s.setup_id, None);
        assert!(s.roster.is_empty());
        assert!(f.s.setup_quiet_reconnect(member(1).fingerprint).await?);
        assert!(f.s.setup_quiet_reconnect(member(3).fingerprint).await?);
        assert!(!f.s.setup_quiet_reconnect(member(4).fingerprint).await?);
        assert!(!f.s.setup_quiet_reconnect(member(9).fingerprint).await?);
        assert_eq!(f.s.peers().await?.len(), 2);
        assert!(f.pin(4).await?.is_some());
        assert!(f.pin(5).await?.is_some());
        let retained =
            f.s.call(|c| {
                Ok(c.query_row("SELECT count(*) FROM setup_entries", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .await?;
        assert_eq!(retained, if own { 1 } else { 0 });
        assert!(f.s.setup_link(id(4), SetupScope::Enabled).await.is_err());
        let retained_generation = f.s.pairing_generation(&id(4));
        f.s.setup_fresh_local_recovery(id(4), member(2), |_| {})
            .await?;
        assert!(!f.s.setup_state().withdrawn);
        assert_eq!(f.s.pairing_generation(&id(4)), retained_generation);
        f.s.setup_link(id(4), SetupScope::Enabled).await?;
    }
    Ok(())
}
#[tokio::test]
async fn setup_s5_retained_ask_member_prevents_bridge_to_another_setup() -> Result<()> {
    let f = Fixture::new().await?;
    f.s.pair(id(3), member(3).fingerprint).await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    assert_eq!(
        f.s.setup_state()
            .links
            .iter()
            .find(|l| l.device == id(3))
            .unwrap()
            .scope,
        SetupScope::Ask
    );
    f.s.setup_remove(id(2), id(1), |_| {}).await?;
    assert_eq!(f.s.setup_state().setup_id, Some(id(0)));
    f.s.pair(id(4), member(4).fingerprint).await?;
    f.s.setup_link(id(4), SetupScope::Enabled).await?;
    let fence = SetupFence {
        device: id(4),
        fingerprint: member(4).fingerprint,
        ..f.f.clone()
    };
    assert!(
        !f.s.setup_activate(
            fence,
            member(2),
            member(4),
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: Some(id(255))
            },
            |_| {}
        )
        .await?
    );
    assert_eq!(f.s.setup_state().setup_id, Some(id(0)));
    assert_eq!(
        f.s.setup_state()
            .links
            .iter()
            .find(|l| l.device == id(4))
            .unwrap()
            .scope,
        SetupScope::Disabled
    );
    f.s.setup_remove(id(2), id(3), |_| {}).await?;
    assert_eq!(f.s.setup_state().setup_id, None);
    Ok(())
}
#[tokio::test]
async fn setup_pending_ack_replay_receipt_eviction_and_restart() -> Result<()> {
    let f = Fixture::new().await?;
    let pending = f.s.members_outgoing(f.f.clone()).await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    assert_eq!(f.s.members_outgoing(f.f.clone()).await?, pending);
    f.s.members_ack(f.f.clone(), pending.transfer_id.clone())
        .await?;
    let changed = f.s.members_outgoing(f.f.clone()).await?;
    assert_ne!(pending.transfer_id, changed.transfer_id);
    f.s.members_ack(f.f.clone(), changed.transfer_id.clone())
        .await?;
    assert_eq!(f.s.members_outgoing(f.f.clone()).await?, changed);
    let path = f._dir.path().join("state.sqlite3");
    drop(f.s);
    let s = Store::open(&path)?;
    assert_eq!(s.members_outgoing(f.f.clone()).await?, changed);
    let snapshot = Members {
        transfer_id: id(90),
        setup_id: id(0),
        members: s.setup_state().roster,
    };
    assert!(
        !s.members_merge(f.f.clone(), member(2), snapshot.clone(), vec![], |_| {})
            .await?
    );
    let device = id(1);
    s.call(move |c| {
        c.execute(
            "DELETE FROM members_receipts WHERE device=?",
            [device.as_str()],
        )?;
        Ok(())
    })
    .await?;
    assert!(
        !s.members_merge(f.f.clone(), member(2), snapshot.clone(), vec![], |_| {})
            .await?
    );
    assert!(
        s.members_merge(f.f, member(2), snapshot, vec![], |_| {})
            .await?
    );
    Ok(())
}
#[tokio::test]
async fn setup_fresh_local_pair_high_water_and_historical_self_pins() -> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    let mut removed = member(3);
    removed.status = MemberStatus::Removed;
    removed.hlc = 10;
    let removal = f.snapshot(vec![removed.clone()]);
    f.merge(removal.clone()).await?;
    let hello = Hello {
        version: 1,
        device_id: id(3),
        name: member(3).name,
        capabilities: vec!["mesh".into(), "setup".into()],
        paused: None,
        role: Some(DeviceRole::Computer),
        addresses: None,
        setup: Some(SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(0)),
        }),
    };
    f.s.pair_then_hello_scoped(
        id(3),
        member(3).fingerprint,
        Some(hello),
        Some(member(2)),
        SetupScope::Enabled,
        false,
        || {},
    )
    .await?;
    let generation = f.s.pairing_generation(&id(3));
    f.merge(removal).await?;
    assert!(f.pin(3).await?.is_some());
    assert_eq!(generation, f.s.pairing_generation(&id(3)));
    let fresh =
        f.s.setup_state()
            .roster
            .iter()
            .find(|m| m.device_id == id(3))
            .unwrap()
            .hlc;
    removed.hlc = fresh + 1;
    f.merge(f.snapshot(vec![removed])).await?;
    assert!(f.pin(3).await?.is_none());
    let mut historical = member(2);
    historical.fingerprint = member(8).fingerprint;
    historical.role = DeviceRole::Phone;
    historical.status = MemberStatus::Removed;
    let mut old_sender = member(1);
    old_sender.fingerprint = member(9).fingerprint;
    old_sender.status = MemberStatus::Removed;
    f.merge(f.snapshot(vec![historical, old_sender])).await?;
    assert_eq!(f.s.setup_state().setup_id, Some(id(0)));
    assert!(f.pin(1).await?.is_some());
    Ok(())
}
#[tokio::test]
async fn setup_prompt_once_deny_allow_refresh_and_generation_fences() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.s.setup_link(id(1), SetupScope::Ask).await?;
    let remote = SetupHello {
        scope: SetupScope::Enabled,
        setup_id: Some(id(0)),
    };
    f.s.setup_prompt(f.f.clone(), remote.clone()).await?;
    assert!(f.s.setup_state().links[0].prompt);
    let path = f._dir.path().join("state.sqlite3");
    let mut before = f.s.setup_state();
    // HELLO evidence belongs to this daemon run; durable consent survives.
    for link in &mut before.links {
        link.hello_seen = false;
    }
    drop(f.s);
    f.s = Store::open(&path)?;
    assert_eq!(f.s.setup_state(), before);
    let generation = f.s.pairing_generation(&id(1)).unwrap();
    f.s.setup_decide(id(1), f.f.generation, generation, true)
        .await?;
    assert_eq!(f.s.setup_state().links[0].scope, SetupScope::Ask);
    f.f.connection = 2;
    f.f.current.store(2, Ordering::SeqCst);
    f.s.setup_activate(f.f.clone(), member(2), member(1), remote.clone(), |_| {})
        .await?;
    assert_eq!(f.s.setup_state().links[0].scope, SetupScope::Enabled);
    f.s.setup_link(id(1), SetupScope::Ask).await?;
    f.s.setup_prompt(f.f.clone(), remote).await?;
    assert!(
        !f.s.setup_state().links[0].prompt,
        "no duplicate prompt in one binding/setup generation"
    );
    // A new binding has a new migration decision, while old prompt actions fail.
    f.s.pair(id(1), member(1).fingerprint).await?;
    let new = f.s.pairing_generation(&id(1)).unwrap();
    assert_ne!(new, generation);
    f.s.setup_prompt(
        SetupFence {
            binding_generation: new,
            ..f.f.clone()
        },
        SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(0)),
        },
    )
    .await?;
    f.s.setup_decide(id(1), f.f.generation, new, false).await?;
    assert_eq!(f.s.setup_state().links[0].scope, SetupScope::Disabled);
    assert!(
        f.s.setup_decide(id(1), f.f.generation, generation, true)
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn setup_local_union_capacity_and_pin_history_never_evict() -> Result<()> {
    let f = Fixture::new().await?;
    // Current setup has two records; 62 unknown tombstones fill its total cap.
    let removals = (3..65)
        .map(|n| {
            let mut m = member(n);
            m.status = MemberStatus::Removed;
            m
        })
        .collect::<Vec<_>>();
    f.merge(f.snapshot(removals)).await?;
    let before = f.s.setup_state();
    assert_eq!(before.roster.len(), 64);
    let snapshot = Members {
        transfer_id: id(99),
        setup_id: id(0),
        members: vec![
            before
                .roster
                .iter()
                .find(|m| m.device_id == id(1))
                .unwrap()
                .clone(),
            member(90),
        ],
    };
    assert!(f.merge(snapshot).await.unwrap_err().is::<MembersError>());
    assert_eq!(before, f.s.setup_state());
    assert!(f.pin(90).await?.is_none());
    let g = Fixture::new().await?;
    for n in 3..18 {
        g.s.pair(id(n), member(n).fingerprint).await?;
    }
    assert!(
        g.merge(g.snapshot(vec![member(18)]))
            .await
            .unwrap_err()
            .is::<MembersError>()
    );
    assert!(g.pin(18).await?.is_none());
    Ok(())
}
#[test]
fn setup_hlc_skew_boundaries_are_checked_without_overflow() {
    let now = 100;
    assert!(validate_stamp(now + 60000, now).is_ok());
    assert!(validate_stamp(now + 60001, now).is_err());
    assert!(validate_stamp(u64::MAX, u64::MAX).is_ok());
}
#[tokio::test]
async fn setup_crash_fixture() -> Result<()> {
    let Some(path) = std::env::var_os("PNEU_SETUP_CRASH_DB") else {
        return Ok(());
    };
    let s = Store::open(Path::new(&path))?;
    let fence = SetupFence {
        binding_generation: 1,
        generation: s.setup_state().generation,
        device: id(1),
        fingerprint: member(1).fingerprint,
        connection: 1,
        current: Arc::new(AtomicU64::new(1)),
    };
    let mut roster = s.setup_state().roster;
    let mut new = member(3);
    new.hlc = roster.iter().map(|m| m.hlc).max().unwrap() + 1;
    roster.push(new);
    s.members_merge(
        fence,
        member(2),
        Members {
            transfer_id: id(70),
            setup_id: id(0),
            members: roster,
        },
        vec![],
        |_| std::process::exit(0),
    )
    .await?;
    unreachable!()
}
#[tokio::test]
async fn setup_crash_before_and_after_commit_reopens_atomically_without_reinstall() -> Result<()> {
    for before in [true, false] {
        let f = Fixture::new().await?;
        let path = f._dir.path().join("state.sqlite3");
        let mut old = f.s.setup_state();
        for link in &mut old.links {
            link.hello_seen = false;
        }
        drop(f.s);
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "store::members_tests::setup_crash_fixture",
                "--test-threads=1",
            ])
            .env("PNEU_SETUP_CRASH_DB", &path);
        if before {
            command.env("PNEU_SETUP_CRASH_BEFORE_COMMIT", "1");
        }
        assert!(command.output()?.status.success());
        let s = Store::open(&path)?;
        if before {
            assert_eq!(s.setup_state(), old);
            assert!(!s.peers().await?.values().any(|d| *d == id(3)));
        } else {
            assert_eq!(s.pairing_generation(&id(3)), Some(1));
            let mut roster = old.roster;
            let mut new = member(3);
            new.hlc = roster.iter().map(|m| m.hlc).max().unwrap() + 1;
            roster.push(new);
            assert!(
                s.members_merge(
                    f.f.clone(),
                    member(2),
                    Members {
                        transfer_id: id(70),
                        setup_id: id(0),
                        members: roster
                    },
                    vec![],
                    |_| {}
                )
                .await?
            );
            assert_eq!(s.pairing_generation(&id(3)), Some(1));
            assert_eq!(
                s.call(
                    |c| Ok(c.query_row("SELECT count(*) FROM setup_notices", [], |r| r
                        .get::<_, i64>(0))?)
                )
                .await?,
                1
            );
        }
    }
    Ok(())
}
#[tokio::test]
async fn setup_token_founding_lower_id_waits_and_first_durable_assignment_wins() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let s = Store::open(&dir.path().join("state.sqlite3"))?;
    s.pair(id(1), member(1).fingerprint).await?;
    s.setup_link(id(1), SetupScope::Enabled).await?;
    let mut fence = SetupFence {
        binding_generation: 1,
        generation: 0,
        device: id(1),
        fingerprint: member(1).fingerprint,
        connection: 1,
        current: Arc::new(AtomicU64::new(1)),
    };
    assert!(
        !s.setup_activate(
            fence.clone(),
            member(2),
            member(1),
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: None
            },
            |_| {}
        )
        .await?
    );
    assert!(s.setup_state().setup_id.is_none());
    assert!(
        !s.setup_activate(
            fence.clone(),
            member(2),
            member(1),
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: Some(id(5))
            },
            |_| {}
        )
        .await?
    );
    assert_eq!(s.setup_state().setup_id, Some(id(5)));
    assert!(
        s.setup_activate(
            fence.clone(),
            member(2),
            member(1),
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: Some(id(6))
            },
            |_| {}
        )
        .await
        .is_err()
    );
    fence.generation = s.setup_state().generation;
    assert!(
        !s.setup_activate(
            fence,
            member(2),
            member(1),
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: Some(id(6))
            },
            |_| {}
        )
        .await?
    );
    assert_eq!(s.setup_state().setup_id, Some(id(5)));
    let dir2 = crate::test_support::scratch()?;
    let path = dir2.path().join("state.sqlite3");
    let s = Store::open(&path)?;
    s.pair(id(3), member(3).fingerprint).await?;
    s.setup_link(id(3), SetupScope::Enabled).await?;
    s.call(|c| {
        c.execute("UPDATE setup_links SET token_founding=1", [])?;
        Ok(())
    })
    .await?;
    let fence = SetupFence {
        binding_generation: 1,
        generation: 0,
        device: id(3),
        fingerprint: member(3).fingerprint,
        connection: 1,
        current: Arc::new(AtomicU64::new(1)),
    };
    assert!(
        !s.setup_activate(
            fence,
            member(2),
            member(3),
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: None
            },
            |_| {}
        )
        .await?
    );
    let founded = s.setup_state().setup_id.unwrap();
    assert_ne!(founded, id(2));
    assert_ne!(founded, id(3));
    drop(s);
    let s = Store::open(&path)?;
    assert_eq!(s.setup_state().setup_id, Some(founded));
    Ok(())
}
#[tokio::test]
async fn setup_v3_migration_preserves_bindings_settings_queues_and_receipts() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let path = dir.path().join("state.sqlite3");
    let s = Store::open(&path)?;
    s.pair(id(1), member(1).fingerprint).await?;
    s.settings_edit(
        id(2),
        id(1),
        false,
        format!("{}/{}/clipboard", id(2), id(1)),
        false,
    )
    .await?;
    let pending = s.settings_outgoing(id(2), id(1), false).await?;
    let generation = s.pairing_generation(&id(1));
    drop(s);
    {
        let c = Connection::open(&path)?;
        c.execute_batch("DROP TABLE setup_links;DROP TABLE setup_entries;DROP TABLE setup_state;DROP TABLE members_receipts;DROP TABLE members_outgoing;DROP TABLE members_replay;DROP TABLE setup_candidates;DROP TABLE setup_conflicts;DROP TABLE setup_notices;DROP TABLE setup_retired;DROP TABLE setup_withdrawn_pins;PRAGMA user_version=3;")?;
    }
    let s = Store::open(&path)?;
    assert_eq!(s.setup_state().links[0].scope, SetupScope::Ask);
    assert_eq!(s.pairing_generation(&id(1)), generation);
    assert_eq!(s.settings_outgoing(id(2), id(1), false).await?, pending);
    assert_eq!(s.peers().await?.len(), 1);
    Ok(())
}
#[tokio::test]
async fn setup_reach_ties_removed_sender_and_phone_metadata_are_isolated() -> Result<()> {
    let f = Fixture::new().await?;
    let mut c = member(3);
    c.reach = Some(MemberReach {
        addresses: vec!["127.0.0.1:40003".into()],
        relay: vec![],
        hlc: 5,
        writer_device: id(3),
    });
    f.merge(f.snapshot(vec![c.clone()])).await?;
    let before = f.s.setup_state();
    c.reach.as_mut().unwrap().addresses = vec!["127.0.0.1:40004".into()];
    assert!(
        f.merge(f.snapshot(vec![c]))
            .await
            .unwrap_err()
            .is::<MembersError>()
    );
    assert_eq!(before, f.s.setup_state());
    let mut phone = member(4);
    phone.role = DeviceRole::Phone;
    let mut second = member(5);
    second.role = DeviceRole::Phone;
    f.merge(f.snapshot(vec![phone, second])).await?;
    assert_eq!(
        f.s.peer_info()
            .await?
            .iter()
            .filter(|p| p.role == "phone")
            .count(),
        2
    );
    let captured = f.snapshot(vec![{
        let mut v = member(3);
        v.status = MemberStatus::Removed;
        v.hlc = 30;
        v
    }]);
    f.s.setup_remove(id(2), id(1), |_| {}).await?;
    let before = f.s.setup_state();
    assert!(f.merge(captured).await.is_err());
    assert_eq!(before, f.s.setup_state());
    assert!(f.pin(3).await?.is_some());
    Ok(())
}
#[tokio::test]
async fn setup_receipt_window_prunes_only_receipts_and_stale_replay_merges_safely() -> Result<()> {
    let f = Fixture::new().await?;
    let add = f.snapshot(vec![member(3)]);
    f.merge(add.clone()).await?;
    f.s.setup_remove(id(2), id(3), |_| {}).await?;
    let metadata = serde_json::to_string(&members::members_metadata(&add))?;
    let generation = f.f.generation;
    f.s.call(move |c| {
        let tx = c.transaction()?;
        for n in 0..4096 {
            tx.execute(
                "INSERT INTO members_receipts VALUES(?,?,?,?)",
                params![
                    id(1).as_str(),
                    format!("{n:032x}"),
                    generation as i64,
                    metadata
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await?;
    let mut newer = add.clone();
    newer.transfer_id = id(99);
    assert!(!f.merge(newer).await?);
    assert_eq!(
        f.s.call(|c| Ok(
            c.query_row("SELECT count(*) FROM members_receipts", [], |r| r
                .get::<_, i64>(0))?
        ))
        .await?,
        4096
    );
    assert!(!f.merge(add).await?);
    assert!(f.pin(3).await?.is_none());
    assert_eq!(
        f.s.setup_state()
            .roster
            .iter()
            .find(|m| m.device_id == id(3))
            .unwrap()
            .status,
        MemberStatus::Removed
    );
    Ok(())
}
#[tokio::test]
async fn setup_offline_catchup_preserves_writers_and_uses_current_introducer() -> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(3), member(4)])).await?;
    f.s.setup_remove(id(2), id(1), |_| {}).await?;
    let fence = SetupFence {
        device: id(3),
        fingerprint: member(3).fingerprint,
        binding_generation: f.s.pairing_generation(&id(3)).unwrap(),
        ..f.f.clone()
    };
    let mut forwarded = member(5);
    forwarded.writer_device = id(1);
    forwarded.hlc = 40;
    let snapshot = Members {
        transfer_id: id(98),
        setup_id: id(0),
        members: vec![member(3), forwarded],
    };
    assert!(
        !f.s.members_merge(fence, member(2), snapshot, vec![], |_| {})
            .await?
    );
    assert!(
        f.pin(4).await?.is_some(),
        "omission never removes retained members"
    );
    assert!(f.pin(1).await?.is_none());
    assert!(f.pin(5).await?.is_some());
    let state = f.s.setup_state();
    assert_eq!(
        state
            .roster
            .iter()
            .find(|m| m.device_id == id(5))
            .unwrap()
            .writer_device,
        id(1)
    );
    assert_eq!(
        state
            .links
            .iter()
            .find(|l| l.device == id(5))
            .unwrap()
            .introduced_by,
        Some(id(3))
    );
    Ok(())
}
#[tokio::test]
async fn setup_forwarded_reach_never_becomes_last_good_across_restart() -> Result<()> {
    let mut f = Fixture::new().await?;
    let mut c = member(3);
    c.reach = Some(MemberReach {
        addresses: vec!["127.0.0.1:40003".into()],
        relay: vec![],
        hlc: 10,
        writer_device: id(3),
    });
    f.merge(f.snapshot(vec![c.clone()])).await?;
    assert!(
        !f.s.peer_info()
            .await?
            .into_iter()
            .find(|p| p.id == id(3))
            .unwrap()
            .route_proven
    );
    let path = f._dir.path().join("state.sqlite3");
    drop(f.s);
    f.s = Store::open(&path)?;
    assert!(
        !f.s.peer_info()
            .await?
            .into_iter()
            .find(|p| p.id == id(3))
            .unwrap()
            .route_proven
    );
    c.reach.as_mut().unwrap().hlc = 11;
    c.reach.as_mut().unwrap().addresses = vec!["127.0.0.1:40004".into()];
    f.merge(f.snapshot(vec![c.clone()])).await?;
    let peer =
        f.s.peer_info()
            .await?
            .into_iter()
            .find(|p| p.id == id(3))
            .unwrap();
    assert_eq!(peer.addresses[0], "127.0.0.1:40004");
    assert!(!peer.route_proven);
    f.s.remember_route(id(3), "127.0.0.1:40005".into()).await?;
    c.reach.as_mut().unwrap().hlc = 12;
    c.reach.as_mut().unwrap().addresses = vec!["127.0.0.1:40006".into()];
    f.merge(f.snapshot(vec![c])).await?;
    let peer =
        f.s.peer_info()
            .await?
            .into_iter()
            .find(|p| p.id == id(3))
            .unwrap();
    assert!(peer.route_proven);
    assert_eq!(peer.addresses[0], "127.0.0.1:40005");
    Ok(())
}
#[tokio::test]
async fn setup_assigned_member_waits_for_unassigned_peer_without_reconnect_loop() -> Result<()> {
    let f = Fixture::new().await?;
    let before = f.s.setup_state();
    let reconnect = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let captured = reconnect.clone();
    assert!(
        !f.s.setup_activate(
            f.f.clone(),
            member(2),
            member(1),
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: None
            },
            move |delta| {
                captured.store(delta.reconnect, Ordering::SeqCst);
            }
        )
        .await?
    );
    assert!(
        !reconnect.load(Ordering::SeqCst),
        "the assigned side must let the unassigned dialer complete READY and adopt its HELLO ID"
    );
    assert_eq!(f.s.setup_state(), before);
    Ok(())
}
#[tokio::test]
async fn setup_token_replacement_retires_old_pin_and_dissolves_separate_last_member() -> Result<()>
{
    let f = Fixture::new().await?;
    let previous = f.s.setup_state().generation;
    let mut hello = Hello {
        version: 1,
        device_id: id(1),
        name: "Replacement".into(),
        capabilities: vec!["mesh".into(), "setup".into()],
        paused: None,
        role: Some(DeviceRole::Computer),
        addresses: None,
        setup: Some(SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(255)),
        }),
    };
    f.s.pair_then_hello_scoped(
        id(1),
        member(9).fingerprint.clone(),
        Some(hello.clone()),
        Some(member(2)),
        SetupScope::Enabled,
        false,
        || {},
    )
    .await?;
    let state = f.s.setup_state();
    assert!(state.setup_id.is_none());
    assert!(state.generation > previous);
    assert_eq!(state.links[0].scope, SetupScope::Disabled);
    assert_eq!(f.pin(1).await?, Some(member(9).fingerprint.clone()));
    f.s.setup_link(id(1), SetupScope::Enabled).await?;
    let mut replacement = member(1);
    replacement.fingerprint = member(9).fingerprint.clone();
    let fence = SetupFence {
        generation: state.generation,
        binding_generation: f.s.pairing_generation(&id(1)).unwrap(),
        fingerprint: member(9).fingerprint,
        ..f.f.clone()
    };
    f.s.setup_activate(
        fence,
        member(2),
        replacement,
        hello.setup.take().unwrap(),
        |_| {},
    )
    .await?;
    let state = f.s.setup_state();
    assert_eq!(state.setup_id, Some(id(255)));
    assert!(
        state
            .roster
            .iter()
            .all(|m| m.fingerprint != member(1).fingerprint)
    );
    assert!(
        f.s.call(|c| Ok(c.query_row(
            "SELECT count(*) FROM setup_entries WHERE fingerprint=?",
            [member(1).fingerprint.as_str()],
            |r| r.get::<_, i64>(0)
        )?))
        .await?
            > 0
    );
    Ok(())
}

#[tokio::test]
async fn setup_metadata_capacity_is_checked_before_local_config_save() -> Result<()> {
    let f = Fixture::new().await?;
    let endpoint = |n| {
        format!(
            "{n}{}.{}.{}.{}:47821",
            "a".repeat(62),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(60)
        )
    };
    let mut local = member(2);
    local.reach = Some(MemberReach {
        addresses: (0..2).map(endpoint).collect(),
        relay: vec![],
        hlc: 0,
        writer_device: id(2),
    });
    f.s.setup_refresh_self(local).await?;
    let mut snapshot = f.snapshot(
        (3..65)
            .map(|n| {
                let mut m = member(n);
                m.status = MemberStatus::Removed;
                m.name = "\u{1}".repeat(100);
                m
            })
            .collect(),
    );
    snapshot
        .members
        .iter_mut()
        .find(|m| m.device_id == id(1))
        .unwrap()
        .reach = Some(MemberReach {
        addresses: (0..8).map(endpoint).collect(),
        relay: vec![],
        hlc: 1,
        writer_device: id(1),
    });
    let mut size = serde_json::to_vec(&snapshot)?.len();
    for m in snapshot
        .members
        .iter_mut()
        .filter(|m| m.status == MemberStatus::Removed)
    {
        while m.name.len() < 128 && size + 6 <= 65530 {
            m.name.push('\u{1}');
            size += 6;
        }
        while m.name.len() < 128 && size < 65530 {
            m.name.push('x');
            size += 1;
        }
    }
    assert_eq!(size, 65530);
    assert_eq!(serde_json::to_vec(&snapshot)?.len(), size);
    f.merge(snapshot).await?;
    let before = f.s.setup_state();
    let saved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marker = saved.clone();
    let mut local = member(2);
    local.name = "\u{1}".repeat(128);
    assert!(
        f.s.setup_refresh_self_then(
            local,
            move || {
                marker.store(true, Ordering::SeqCst);
                Ok(())
            },
            || panic!("failed edit must not publish")
        )
        .await
        .unwrap_err()
        .is::<MembersError>()
    );
    assert!(!saved.load(Ordering::SeqCst));
    assert_eq!(f.s.setup_state(), before);
    let marker = saved.clone();
    let mut local = member(2);
    local.name = "New".into();
    f.s.setup_refresh_self_then(
        local,
        move || {
            marker.store(true, Ordering::SeqCst);
            Ok(())
        },
        || {},
    )
    .await?;
    assert!(saved.load(Ordering::SeqCst));
    assert!(
        f.s.setup_state()
            .roster
            .iter()
            .any(|m| m.device_id == id(2) && m.name == "New")
    );
    Ok(())
}

// Deterministic READY scheduling uses persisted peer HELLOs and fresh authenticated connection fences.
struct Migration {
    dirs: Vec<tempfile::TempDir>,
    stores: Vec<Store>,
    connection: u64,
}
impl Migration {
    async fn new(count: u8) -> Result<Self> {
        let mut dirs = Vec::new();
        let mut stores = Vec::new();
        for n in 1..=count {
            let dir = crate::test_support::scratch()?;
            let store = Store::open(&dir.path().join("state.sqlite3"))?;
            for peer in 1..=count {
                if n != peer {
                    store.pair(id(peer), member(peer).fingerprint).await?;
                    store
                        .call(move |c| {
                            c.execute(
                                "UPDATE peers SET role='computer' WHERE id=?",
                                [id(peer).as_str()],
                            )?;
                            Ok(())
                        })
                        .await?;
                }
            }
            dirs.push(dir);
            stores.push(store);
        }
        Ok(Self {
            dirs,
            stores,
            connection: 10,
        })
    }
    fn s(&self, n: u8) -> &Store {
        &self.stores[n as usize - 1]
    }
    async fn link(&self, n: u8, peer: u8) -> Result<()> {
        self.s(n)
            .setup_begin_link(id(peer), None, vec![], |_| {})
            .await
            .map(|_| ())
    }
    async fn expire(&self, n: u8) -> Result<()> {
        self.s(n)
            .setup_discovery_tick(member(n), u64::MAX, |_| {})
            .await
    }
    async fn hello(&mut self, to: u8, from: u8) -> Result<()> {
        let hello = self.s(from).setup_hello(Some(&id(to)), SetupScope::Enabled);
        self.deliver(to, from, hello).await
    }
    async fn deliver(&mut self, to: u8, from: u8, hello: SetupHello) -> Result<()> {
        self.connection += 1;
        let f = SetupFence {
            generation: self.s(to).setup_state().generation,
            binding_generation: self.s(to).pairing_generation(&id(from)).unwrap(),
            device: id(from),
            fingerprint: member(from).fingerprint,
            connection: self.connection,
            current: Arc::new(AtomicU64::new(self.connection)),
        };
        self.s(to)
            .setup_activate(f, member(to), member(from), hello, |_| {})
            .await?;
        Ok(())
    }
    async fn allow(&self, n: u8, peer: u8) -> Result<()> {
        self.s(n)
            .setup_decide(
                id(peer),
                self.s(n).setup_state().generation,
                self.s(n).pairing_generation(&id(peer)).unwrap(),
                true,
            )
            .await
    }
    async fn found(&self, n: u8, peer: u8) -> Result<Id> {
        self.link(n, peer).await?;
        self.expire(n).await?;
        Ok(self.s(n).setup_state().setup_id.unwrap())
    }
}
#[tokio::test]
async fn setup_migration_reordered_hellos_concurrent_allows_and_founder_loss() -> Result<()> {
    // A=1 < C=2 < B=3; vary A delivery order and B/C join order.
    for order in [[2, 3], [3, 2]] {
        for join_order in [[2, 3], [3, 2]] {
            for bc_first in [true, false] {
                let mut m = Migration::new(3).await?;
                let setup = m.found(1, 3).await?;
                assert_eq!(m.s(1).setup_state().roster.len(), 1);
                for peer in order {
                    m.hello(peer, 1).await?;
                }
                tokio::try_join!(m.allow(2, 1), m.allow(3, 1))?;
                assert!(
                    m.s(2)
                        .setup_state()
                        .links
                        .iter()
                        .all(|l| l.scope == SetupScope::Ask)
                );
                for peer in order {
                    m.hello(peer, 1).await?;
                }
                let first = join_order[0];
                let second = join_order[1];
                if bc_first {
                    m.hello(2, 3).await?;
                    m.hello(3, 2).await?;
                    assert!(m.s(2).setup_state().setup_id.is_none());
                    assert!(m.s(3).setup_state().setup_id.is_none());
                    m.hello(first, 1).await?;
                    // Founder vanishes before the second assignment.
                    m.hello(second, first).await?;
                } else {
                    m.hello(first, 1).await?;
                    m.hello(second, 1).await?;
                    m.hello(2, 3).await?;
                    m.hello(3, 2).await?;
                }
                for peer in [2, 3] {
                    let s = m.s(peer).setup_state();
                    assert_eq!(s.setup_id, Some(setup.clone()));
                    assert!(
                        s.links
                            .iter()
                            .all(|l| l.scope == SetupScope::Enabled && !l.prompt)
                    );
                    assert_eq!(s.generation, 1);
                }
            }
        }
    }
    Ok(())
}
#[tokio::test]
async fn setup_link_joins_before_founding_and_late_separate_peer_stays_direct() -> Result<()> {
    let mut m = Migration::new(3).await?;
    let setup = m.found(1, 2).await?;
    m.s(2).setup_link(id(1), SetupScope::Disabled).await?;
    m.link(2, 1).await?;
    m.hello(2, 1).await?;
    m.expire(2).await?;
    assert_eq!(m.s(2).setup_state().setup_id, Some(setup.clone()));
    assert!(!m.s(2).setup_state().pending_link);
    let separate = m.found(3, 2).await?;
    assert_ne!(setup, separate);
    m.hello(2, 3).await?;
    assert_eq!(m.s(2).setup_state().setup_id, Some(setup));
    assert_eq!(
        m.s(2)
            .setup_state()
            .links
            .iter()
            .find(|l| l.device == id(3))
            .unwrap()
            .scope,
        SetupScope::Disabled
    );
    Ok(())
}
#[tokio::test]
async fn setup_link_defers_to_ask_advertisement_and_survives_restart() -> Result<()> {
    let mut m = Migration::new(2).await?;
    let setup = m.found(1, 2).await?;
    m.s(1).setup_link(id(2), SetupScope::Ask).await?;
    m.link(2, 1).await?;
    m.hello(2, 1).await?;
    m.expire(2).await?;
    assert!(m.s(2).setup_state().setup_id.is_none());
    assert_eq!(m.s(2).setup_state().waiting_for, Some(id(1)));
    drop(m.stores.remove(1));
    let reopened = Store::open(&m.dirs[1].path().join("state.sqlite3"))?;
    m.stores.insert(1, reopened);
    assert!(m.s(2).setup_state().pending_link);
    assert!(m.s(2).setup_state().waiting_for.is_none());
    m.hello(2, 1).await?;
    m.expire(2).await?;
    assert!(m.s(2).setup_state().setup_id.is_none());
    m.hello(1, 2).await?;
    m.allow(1, 2).await?;
    m.hello(1, 2).await?;
    m.hello(2, 1).await?;
    assert_eq!(m.s(2).setup_state().setup_id, Some(setup));
    assert!(!m.s(2).setup_state().pending_link);
    Ok(())
}
#[tokio::test]
async fn setup_link_restart_without_advertisement_founds_self_only() -> Result<()> {
    let mut m = Migration::new(2).await?;
    m.link(2, 1).await?;
    drop(m.stores.remove(1));
    let reopened = Store::open(&m.dirs[1].path().join("state.sqlite3"))?;
    assert!(reopened.setup_state().pending_link);
    reopened
        .setup_discovery_tick(member(2), crate::now_ms(), |_| {})
        .await?;
    assert!(reopened.setup_state().setup_id.is_none());
    reopened
        .setup_discovery_tick(member(2), u64::MAX, |_| {})
        .await?;
    assert!(reopened.setup_state().setup_id.is_some());
    assert_eq!(reopened.setup_state().roster.len(), 1);
    assert_eq!(reopened.setup_state().generation, 1);
    Ok(())
}
#[tokio::test]
async fn setup_link_pending_prompt_is_allow_and_disabled_is_not_bulk_enabled() -> Result<()> {
    let mut m = Migration::new(3).await?;
    let setup = m.found(1, 2).await?;
    m.s(2).setup_link(id(3), SetupScope::Disabled).await?;
    m.hello(2, 1).await?;
    m.link(2, 1).await?;
    assert!(!m.s(2).setup_state().pending_link);
    assert!(m.s(2).setup_state().setup_id.is_none());
    m.hello(2, 1).await?;
    m.hello(2, 1).await?;
    assert_eq!(m.s(2).setup_state().setup_id, Some(setup));
    assert_eq!(
        m.s(2)
            .setup_state()
            .links
            .iter()
            .find(|l| l.device == id(3))
            .unwrap()
            .scope,
        SetupScope::Disabled
    );
    Ok(())
}
#[tokio::test]
async fn setup_independent_link_taps_hold_assigned_hellos_until_both_found() -> Result<()> {
    for order in [[1, 2], [2, 1]] {
        let mut m = Migration::new(2).await?;
        m.link(1, 2).await?;
        m.link(2, 1).await?;
        m.hello(1, 2).await?;
        m.hello(2, 1).await?;
        m.expire(1).await?;
        m.expire(2).await?;
        let ids = [m.s(1).setup_state().setup_id, m.s(2).setup_state().setup_id];
        assert_ne!(ids[0], ids[1]);
        for to in order {
            m.hello(to, 3 - to).await?;
        }
        for n in 1..=2 {
            assert_eq!(m.s(n).setup_state().setup_id, ids[n as usize - 1]);
            assert_eq!(m.s(n).setup_state().roster.len(), 1);
        }
    }
    Ok(())
}
#[tokio::test]
async fn setup_link_self_only_is_not_dissolved_by_unrelated_token_pairing() -> Result<()> {
    let m = Migration::new(2).await?;
    let setup = m.found(1, 2).await?;
    let hello = Hello {
        version: 1,
        device_id: id(2),
        name: "Separate".into(),
        capabilities: vec!["mesh".into(), "setup".into()],
        paused: None,
        role: Some(DeviceRole::Computer),
        addresses: None,
        setup: Some(SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(90)),
        }),
    };
    m.s(1)
        .pair_then_hello_scoped(
            id(2),
            member(2).fingerprint,
            Some(hello),
            Some(member(1)),
            SetupScope::Enabled,
            false,
            || {},
        )
        .await?;
    assert_eq!(m.s(1).setup_state().setup_id, Some(setup));
    assert_eq!(m.s(1).setup_state().roster.len(), 1);
    Ok(())
}

#[tokio::test]
async fn setup_token_founding_races_migration_join_first_assignment_wins() -> Result<()> {
    for receiver in [2, 3] {
        for migration_first in [true, false] {
            let token_peer = 5 - receiver;
            let mut m = Migration::new(3).await?;
            let setup = m.found(1, receiver).await?;
            m.s(receiver).setup_link(id(1), SetupScope::Enabled).await?;
            // A confirmed token pairing enables only its new link on each endpoint.
            for (local, remote) in [(receiver, token_peer), (token_peer, receiver)] {
                m.s(local)
                    .setup_link(id(remote), SetupScope::Enabled)
                    .await?;
                m.s(local)
                    .call(move |c| {
                        c.execute(
                            "UPDATE setup_links SET token_founding=1 WHERE device=?",
                            [id(remote).as_str()],
                        )?;
                        Ok(())
                    })
                    .await?;
            }
            let stale = m
                .s(receiver)
                .setup_hello(Some(&id(token_peer)), SetupScope::Enabled);
            if migration_first {
                m.hello(receiver, 1).await?;
            }
            if token_peer < receiver {
                m.deliver(token_peer, receiver, stale).await?;
            }
            m.hello(receiver, token_peer).await?;
            let winner = m.s(receiver).setup_state().setup_id.clone().unwrap();
            if !migration_first {
                m.hello(receiver, 1).await?;
            }
            assert_eq!(m.s(receiver).setup_state().setup_id, Some(winner.clone()));
            if migration_first {
                assert_eq!(winner, setup);
            } else {
                assert_ne!(winner, setup);
            }
            let other = if winner == setup { id(99) } else { setup };
            let snapshot = Members {
                transfer_id: id(90),
                setup_id: other,
                members: vec![member(1), member(receiver)],
            };
            let fence = SetupFence {
                generation: m.s(receiver).setup_state().generation,
                binding_generation: 1,
                device: id(1),
                fingerprint: member(1).fingerprint,
                connection: 1,
                current: Arc::new(AtomicU64::new(1)),
            };
            assert!(
                m.s(receiver)
                    .members_merge(fence, member(receiver), snapshot, vec![], |_| {})
                    .await
                    .unwrap_err()
                    .is::<MembersError>()
            );
            // The token pairing did not bulk-enable the peer's unrelated ask link.
            assert_eq!(
                m.s(token_peer)
                    .setup_state()
                    .links
                    .iter()
                    .find(|l| l.device == id(1))
                    .unwrap()
                    .scope,
                SetupScope::Ask
            );
        }
    }
    Ok(())
}
#[tokio::test]
async fn setup_allow_refresh_same_connection_and_incompatible_id_never_enable_consent() -> Result<()>
{
    let mut m = Migration::new(3).await?;
    m.found(1, 2).await?;
    m.hello(2, 1).await?;
    m.allow(2, 1).await?;
    let connection = m.connection;
    let f = SetupFence {
        generation: 0,
        binding_generation: 1,
        device: id(1),
        fingerprint: member(1).fingerprint,
        connection,
        current: Arc::new(AtomicU64::new(connection)),
    };
    m.s(2)
        .setup_activate(
            f,
            member(2),
            member(1),
            m.s(1).setup_hello(Some(&id(2)), SetupScope::Enabled),
            |_| {},
        )
        .await?;
    assert!(
        m.s(2)
            .setup_state()
            .links
            .iter()
            .all(|l| l.scope == SetupScope::Ask)
    );
    m.s(2).setup_link(id(3), SetupScope::Enabled).await?;
    m.deliver(
        2,
        3,
        SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(88)),
        },
    )
    .await?;
    m.hello(2, 1).await?;
    assert_eq!(
        m.s(2)
            .setup_state()
            .links
            .iter()
            .find(|l| l.device == id(1))
            .unwrap()
            .scope,
        SetupScope::Disabled
    );
    Ok(())
}
#[tokio::test]
async fn setup_members_receipts_are_bounded_hashes_and_share_operation_namespace() -> Result<()> {
    let f = Fixture::new().await?;
    let snapshot = f.snapshot(vec![member(3)]);
    let transfer = snapshot.transfer_id.clone();
    f.merge(snapshot.clone()).await?;
    assert!(f.merge(snapshot).await?);
    let max: i64 =
        f.s.call(|c| {
            Ok(
                c.query_row("SELECT max(length(meta)) FROM members_receipts", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await?;
    assert!(max < 128);
    let mut settings = f.s.settings_outgoing(id(2), id(1), false).await?;
    settings.transfer_id = transfer;
    assert!(
        f.s.settings_merge(id(2), id(1), false, settings)
            .await
            .unwrap_err()
            .is::<ReceiptConflict>()
    );
    Ok(())
}
#[tokio::test]
async fn setup_retained_phone_pin_cannot_be_exported_under_token_computer_id() -> Result<()> {
    let f = Fixture::new().await?;
    let mut phone = member(3);
    phone.role = DeviceRole::Phone;
    f.merge(f.snapshot(vec![phone.clone()])).await?;
    f.s.unpair_then(id(3), |_| {}).await?;
    let hello = Hello {
        version: 1,
        device_id: id(4),
        name: "Computer".into(),
        capabilities: vec!["mesh".into(), "setup".into()],
        paused: None,
        role: Some(DeviceRole::Computer),
        addresses: Some(vec![]),
        setup: Some(SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(0)),
        }),
    };
    f.s.pair_then_hello_scoped(
        id(4),
        phone.fingerprint.clone(),
        Some(hello),
        Some(member(2)),
        SetupScope::Enabled,
        false,
        || {},
    )
    .await?;
    let active =
        f.s.setup_state()
            .roster
            .into_iter()
            .filter(|m| m.status == MemberStatus::Member && m.fingerprint == phone.fingerprint)
            .collect::<Vec<_>>();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].device_id, id(3));
    Ok(())
}

#[tokio::test]
async fn setup_plain_unpair_clears_discovery_and_bounded_separate_markers() -> Result<()> {
    let mut m = Migration::new(2).await?;
    m.found(1, 2).await?;
    m.s(1).setup_link(id(2), SetupScope::Ask).await?;
    m.link(2, 1).await?;
    m.hello(2, 1).await?;
    assert_eq!(m.s(2).setup_state().waiting_for, Some(id(1)));
    m.s(2).unpair_then(id(1), |_| {}).await?;
    assert!(m.s(2).setup_state().waiting_for.is_none());
    m.expire(2).await?;
    assert!(m.s(2).setup_state().setup_id.is_some());
    assert!(m.s(2).setup_state().links.is_empty());
    Ok(())
}
#[tokio::test]
async fn setup_legacy_introduced_link_keeps_protection_and_survives_withdrawal() -> Result<()> {
    for own in [true, false] {
        let f = Fixture::new().await?;
        f.merge(f.snapshot(vec![member(3)])).await?;
        let legacy = SetupFence {
            device: id(3),
            fingerprint: member(3).fingerprint,
            ..f.f.clone()
        };
        f.s.setup_legacy_ready(legacy).await?;
        assert!(!f.s.setup_scoped(&id(3)));
        assert!(f.s.setup_protected(&id(3)));
        f.s.call(|c| {
            c.execute("INSERT INTO outbox_artifacts(id,operation,meta) VALUES('legacy','operation','{}')",[])?;
            c.execute("INSERT INTO outbox_deliveries(artifact,device,generation,fingerprint) VALUES('legacy',?,1,?)",params![id(3).as_str(),member(3).fingerprint.as_str()])?;Ok(())
        }).await?;
        if own {
            let mut tombstone = member(2);
            tombstone.status = MemberStatus::Removed;
            tombstone.hlc = crate::now_ms() + 1;
            f.merge(f.snapshot(vec![tombstone])).await?;
        } else {
            f.s.setup_withdraw(f.f.clone(), |_| {}).await?;
        }
        assert!(f.pin(3).await?.is_some());
        let count: i64 =
            f.s.call(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM outbox_deliveries WHERE device=?",
                    [id(3).as_str()],
                    |r| r.get(0),
                )?)
            })
            .await?;
        assert_eq!(count, 1);
    }
    Ok(())
}
#[tokio::test]
async fn setup_retired_denial_is_not_exported_when_new_setup_receives_legitimate_identity()
-> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    f.s.setup_remove(id(2), id(3), |_| {}).await?;
    f.s.setup_remove(id(2), id(1), |_| {}).await?;
    f.s.pair(id(4), member(4).fingerprint).await?;
    f.s.call(|c| {
        c.execute("UPDATE peers SET role='computer'", [])?;
        Ok(())
    })
    .await?;
    f.s.setup_link(id(4), SetupScope::Enabled).await?;
    let mut fence = SetupFence {
        generation: f.s.setup_state().generation,
        device: id(4),
        fingerprint: member(4).fingerprint,
        ..f.f.clone()
    };
    f.s.setup_activate(
        fence.clone(),
        member(2),
        member(4),
        SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(9)),
        },
        |_| {},
    )
    .await?;
    fence.generation = f.s.setup_state().generation;
    f.s.members_merge(
        fence,
        member(2),
        Members {
            transfer_id: id(70),
            setup_id: id(9),
            members: vec![member(2), member(3), member(4)],
        },
        vec![],
        |_| {},
    )
    .await?;
    assert!(
        f.s.setup_state()
            .roster
            .iter()
            .all(|m| m.device_id != id(3))
    );
    assert!(f.pin(3).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn setup_advisory_conflicts_coalesce_bound_and_clear_on_resolution() -> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    for n in 100..180u64 {
        let mut conflicting = member(3);
        conflicting.fingerprint = Hash::new(format!("{n:064x}"))?;
        f.merge(f.snapshot(vec![conflicting])).await?;
    }
    let count =
        f.s.call(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM setup_conflicts", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await?;
    assert_eq!(count, 1);
    // Conflicting IDs are untrusted too: one retained pin cannot grow status.
    for n in 100..180u8 {
        let mut conflicting = member(n);
        conflicting.fingerprint = member(3).fingerprint;
        let mut snapshot = f.snapshot(vec![conflicting]);
        snapshot.members.retain(|m| m.device_id != id(3));
        f.merge(snapshot).await?;
    }
    let count =
        f.s.call(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM setup_conflicts", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await?;
    assert_eq!(count, 64);
    struct NoActions;
    #[async_trait::async_trait]
    impl crate::actions::PcActions for NoActions {
        async fn clipboard_text(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clipboard_image(&self, _: &std::path::Path, _: &str) -> Result<()> {
            Ok(())
        }
        async fn notify(&self, _: &std::path::Path, _: &str) -> Result<()> {
            Ok(())
        }
        async fn open(&self, _: &std::path::Path) -> Result<()> {
            Ok(())
        }
    }
    // Read a SQLite-consistent copy through the production status path without
    // violating the fixture store's exclusive daemon ownership.
    let status_dir = crate::test_support::scratch()?;
    let root = status_dir.path();
    let copy = root.join("state.sqlite3");
    f.s.call(move |c| {
        c.execute("VACUUM INTO ?", [copy.to_str().unwrap()])?;
        Ok(())
    })
    .await?;
    let core = crate::core::Core::new(
        crate::config::Paths {
            data: root.to_path_buf(),
            cache: root.join("cache"),
            config: root.join("config.toml"),
            socket: root.join("pneud.sock"),
        },
        crate::config::Config {
            save_path: root.join("saved"),
            ..Default::default()
        },
        Arc::new(NoActions),
    )
    .await?;
    let status = core.setup_status().await?;
    assert_eq!(status["conflicts"].as_array().unwrap().len(), 64);
    assert!(serde_json::to_vec(&status["conflicts"])?.len() < 64 * 1024);
    core.shutdown_actions().await?;
    // Replacing the direct binding is insufficient while the roster still
    // retains its identity; terminal removal resolves both sources together.
    f.s.setup_remove(id(2), id(3), |_| {}).await?;
    let count =
        f.s.call(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM setup_conflicts", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn setup_advisory_joined_notices_end_with_binding_and_keep_current_generation() -> Result<()>
{
    let f = Fixture::new().await?;
    let mut new_member = member(3);
    new_member.hlc =
        f.s.setup_state()
            .roster
            .iter()
            .map(|m| m.hlc)
            .max()
            .unwrap()
            + 1;
    f.merge(f.snapshot(vec![new_member.clone()])).await?;
    assert_eq!(
        f.s.call(
            |c| Ok(c.query_row("SELECT count(*) FROM setup_notices", [], |r| r
                .get::<_, i64>(0))?)
        )
        .await?,
        1
    );
    f.s.unpair_then(id(3), |_| {}).await?;
    assert_eq!(
        f.s.call(
            |c| Ok(c.query_row("SELECT count(*) FROM setup_notices", [], |r| r
                .get::<_, i64>(0))?)
        )
        .await?,
        0
    );
    f.merge(f.snapshot(vec![new_member])).await?;
    let generation = f.s.pairing_generation(&id(3)).unwrap();
    assert_eq!(
        f.s.call(|c| Ok(
            c.query_row("SELECT generation FROM setup_notices", [], |r| r
                .get::<_, i64>(0))?
        ))
        .await?,
        generation as i64
    );
    f.s.pair(id(3), member(3).fingerprint).await?;
    assert_eq!(
        f.s.call(
            |c| Ok(c.query_row("SELECT count(*) FROM setup_notices", [], |r| r
                .get::<_, i64>(0))?)
        )
        .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn setup_begin_link_checks_displayed_binding_and_scope_before_enabling() -> Result<()> {
    for replacement in [false, true] {
        let dir = crate::test_support::scratch()?;
        let store = Store::open(&dir.path().join("state.sqlite3"))?;
        for n in [1, 2] {
            store.pair(id(n), member(n).fingerprint).await?;
            store.setup_link(id(n), SetupScope::Ask).await?;
        }
        let shown_binding = store.pairing_generation(&id(1)).unwrap();
        if replacement {
            store.pair(id(1), member(3).fingerprint).await?;
            store.setup_link(id(1), SetupScope::Ask).await?;
        } else {
            store.setup_link(id(1), SetupScope::Disabled).await?;
        }
        let before = store.setup_state();
        let published = Arc::new(AtomicU64::new(0));
        let observed = published.clone();
        assert!(
            !store
                .setup_begin_link(
                    id(1),
                    Some((shown_binding, SetupScope::Ask)),
                    vec![(id(1), 42)],
                    move |delta| {
                        if delta.reconnect {
                            observed.fetch_add(1, Ordering::SeqCst);
                        }
                    },
                )
                .await?,
            "replacement={replacement}"
        );
        assert_eq!(store.setup_state(), before);
        assert_eq!(published.load(Ordering::SeqCst), 0);
        let connection: Option<i64> = store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT last_connection FROM setup_links WHERE device=?",
                    [id(1).as_str()],
                    |r| r.get(0),
                )?)
            })
            .await?;
        assert_eq!(connection, Some(0));
        let scope = if replacement {
            SetupScope::Ask
        } else {
            SetupScope::Disabled
        };
        assert!(
            store
                .setup_begin_link(
                    id(1),
                    Some((store.pairing_generation(&id(1)).unwrap(), scope)),
                    vec![],
                    |_| {},
                )
                .await?
        );
        assert!(
            store
                .setup_state()
                .links
                .iter()
                .all(|l| l.scope == SetupScope::Enabled)
        );
    }
    Ok(())
}

#[tokio::test]
async fn setup_joined_notices_use_admission_before_rename_including_candidates() -> Result<()> {
    for deferred in [false, true] {
        let f = Fixture::new().await?;
        let baseline =
            f.s.setup_state()
                .roster
                .into_iter()
                .find(|m| m.device_id == id(2))
                .unwrap()
                .hlc;
        let mut new = member(4);
        new.hlc = baseline + 1;
        let snapshot = f.snapshot(vec![member(3), new]);
        let blocked = if deferred {
            vec![
                (Some(id(3)), member(3).fingerprint),
                (Some(id(4)), member(4).fingerprint),
            ]
        } else {
            vec![]
        };
        if deferred {
            f.s.members_merge(
                f.f.clone(),
                member(2),
                snapshot.clone(),
                blocked.clone(),
                |_| {},
            )
            .await?;
            let mut renamed_remote = member(3);
            renamed_remote.hlc = baseline + 2;
            renamed_remote.name = "Renamed pre-existing member".into();
            f.s.members_merge(
                f.f.clone(),
                member(2),
                f.snapshot(vec![renamed_remote]),
                blocked,
                |_| {},
            )
            .await?;
        }
        let mut renamed = member(2);
        renamed.name = "Renamed computer".into();
        f.s.setup_refresh_self_then(renamed, || Ok(()), || {})
            .await?;
        assert!(
            f.s.setup_state()
                .roster
                .iter()
                .find(|m| m.device_id == id(2))
                .unwrap()
                .hlc
                > baseline + 1
        );
        // Reopen the serialized state before installation, as on reconnect/startup.
        drop(f.s);
        let store = Store::open(&f._dir.path().join("state.sqlite3"))?;
        if deferred {
            store
                .setup_reconcile_candidates(id(2), vec![], |_| {})
                .await?;
        } else {
            store
                .members_merge(f.f.clone(), member(2), snapshot, vec![], |_| {})
                .await?;
        }
        let devices = store
            .call(|c| {
                let mut q = c.prepare("SELECT device FROM setup_notices ORDER BY device")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        assert_eq!(devices, vec![id(4).to_string()], "deferred={deferred}");
        assert!(store.peers().await?.values().any(|id_| *id_ == id(3)));
    }
    Ok(())
}

#[tokio::test]
async fn setup_preexisting_pairing_later_link_does_not_post_joined_notice() -> Result<()> {
    let f = Fixture::new().await?;
    // This direct binding predates Link-your-devices. Its later admission has a
    // newer stamp than ours, as when an upgraded device initially declines Link.
    f.s.pair(id(3), member(3).fingerprint).await?;
    let baseline =
        f.s.setup_state()
            .roster
            .iter()
            .map(|m| m.hlc)
            .max()
            .unwrap();
    let mut existing = member(3);
    existing.hlc = baseline + 1;
    let mut new = member(4);
    new.hlc = baseline + 2;
    f.merge(f.snapshot(vec![existing.clone(), new])).await?;
    let notified =
        f.s.call(|c| {
            let mut q = c.prepare("SELECT device FROM setup_notices ORDER BY device")?;
            Ok(q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;
    assert_eq!(notified, vec![id(4).to_string()]);
    // The decision survives restart and a later rename/update.
    drop(f.s);
    let s = Store::open(&f._dir.path().join("state.sqlite3"))?;
    existing.hlc += 2;
    existing.name = "Renamed existing computer".into();
    let mut roster = s.setup_state().roster;
    roster.retain(|m| m.device_id != id(3));
    roster.push(existing);
    s.members_merge(
        f.f.clone(),
        member(2),
        Members {
            transfer_id: id(8),
            setup_id: id(0),
            members: roster,
        },
        vec![],
        |_| {},
    )
    .await?;
    let known: bool = s.call(|c| Ok(c.query_row("SELECT json_extract(payload,'$.joined_notice')=0 FROM setup_entries WHERE device=?", [id(3).as_str()], |r| r.get(0))?)).await?;
    assert!(
        known,
        "pre-existing binding stays already known after restart"
    );
    // Reconciliation can install this known member again without announcing it
    // as a new device; genuinely new members still keep their joined notice.
    s.unpair_then(id(3), |_| {}).await?;
    let roster = s.setup_state().roster;
    s.members_merge(
        SetupFence {
            generation: s.setup_state().generation,
            ..f.f.clone()
        },
        member(2),
        Members {
            transfer_id: id(9),
            setup_id: id(0),
            members: roster,
        },
        vec![],
        |_| {},
    )
    .await?;
    let notices: i64 = s
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM setup_notices WHERE device=?",
                [id(3).as_str()],
                |r| r.get(0),
            )?)
        })
        .await?;
    assert_eq!(notices, 0);

    Ok(())
}

#[tokio::test]
async fn setup_direct_repair_clears_introducer_without_resetting_binding() -> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    assert!(
        f.s.setup_state()
            .links
            .iter()
            .find(|l| l.device == id(3))
            .unwrap()
            .introduced_by
            .is_some()
    );
    let generation = f.s.pairing_generation(&id(3));
    f.s.setup_fresh_local_recovery(id(3), member(2), |_| {})
        .await?;
    let link =
        f.s.setup_state()
            .links
            .into_iter()
            .find(|l| l.device == id(3))
            .unwrap();
    assert!(link.introduced_by.is_none());
    assert!(link.introducer_name.is_none());
    assert_eq!(f.s.pairing_generation(&id(3)), generation);
    Ok(())
}

#[tokio::test]
async fn setup_withdrawn_reconnect_notice_suppression_survives_restart() -> Result<()> {
    let f = Fixture::new().await?;
    f.s.setup_withdraw(f.f.clone(), |_| {}).await?;
    let path = f._dir.path().join("state.sqlite3");
    drop(f.s);
    let reopened = Store::open(&path)?;
    assert!(
        reopened
            .setup_quiet_reconnect(member(1).fingerprint)
            .await?
    );
    assert!(
        !reopened
            .setup_quiet_reconnect(member(9).fingerprint)
            .await?
    );
    Ok(())
}

// Shared scenario clock is intentionally fixed (and may be ahead of the host).
// Keep fixture bytes identical to Android; only the validation clock is injected.
struct ReadmitVectors {
    dir: tempfile::TempDir,
    s: Store,
    local: Member,
    a: Member,
    d: Member,
    now: u64,
}
impl ReadmitVectors {
    async fn new(v: &serde_json::Value) -> Result<Self> {
        let dir = crate::test_support::scratch()?;
        let s = Store::open(&dir.path().join("state.sqlite3"))?;
        let roster: Members = serde_json::from_value(v["fixtures"]["join"]["payload"].clone())?;
        let a = roster.members[0].clone();
        let local = roster.members[1].clone();
        let d = roster.members[3].clone();
        for m in [&a, &d] {
            s.pair(m.device_id.clone(), m.fingerprint.clone()).await?;
            let m = m.clone();
            let device = m.device_id.clone();
            s.call(move |c| {
                c.execute(
                    "UPDATE peers SET name=?,role=? WHERE id=?",
                    params![
                        m.name,
                        if m.role == DeviceRole::Phone {
                            "phone"
                        } else {
                            "computer"
                        },
                        m.device_id.as_str()
                    ],
                )?;
                Ok(())
            })
            .await?;
            s.setup_link(device, SetupScope::Enabled).await?;
        }
        let initial = [a.clone(), local.clone(), d.clone()];
        s.call(move |c| {
            c.execute("UPDATE setup_state SET setup_id=?,generation=7",[id(0).as_str()])?;
            for value in initial {
                let payload=serde_json::json!({"value":value,"removal":null,"denied":false,"setup_id":id(0)});
                c.execute("INSERT INTO setup_entries VALUES(?,?,?)",params![value.device_id.as_str(),value.fingerprint.as_str(),payload.to_string()])?;
            }
            Ok(())
        }).await?;
        Self::reload(&s).await?;
        let h = Self {
            dir,
            s,
            local,
            a,
            d,
            now: v["clock_ms"].as_u64().unwrap(),
        };
        // The shared initial state names authenticated READY senders, not
        // merely locally enabled links. Establish their reciprocal authority.
        for sender in [&h.a, &h.d] {
            h.s.setup_activate(
                h.fence(sender),
                h.local.clone(),
                sender.clone(),
                SetupHello {
                    scope: SetupScope::Enabled,
                    setup_id: Some(id(0)),
                },
                |_| {},
            )
            .await?;
            if sender.role == DeviceRole::Computer {
                h.s.mark_peer_established(sender.device_id.clone()).await?;
            }
        }
        Ok(h)
    }
    async fn reload(s: &Store) -> Result<()> {
        let state = s.call(|c| members::state(c)).await?;
        *s.setup.lock().unwrap() = Some(state);
        Ok(())
    }
    fn fence(&self, sender: &Member) -> SetupFence {
        SetupFence {
            generation: self.s.setup_state().generation,
            binding_generation: self.s.pairing_generation(&sender.device_id).unwrap_or(0),
            device: sender.device_id.clone(),
            fingerprint: sender.fingerprint.clone(),
            connection: 1,
            current: Arc::new(AtomicU64::new(1)),
        }
    }
    async fn entry(&self, pin: &Hash) -> Result<serde_json::Value> {
        let pin = pin.clone();
        let subject = Id::from_bytes([0x33; 16]);
        self.s
            .call(move |c| {
                let raw: Option<String> = c
                    .query_row(
                        "SELECT payload FROM setup_entries WHERE device=? AND fingerprint=?",
                        params![subject.as_str(), pin.as_str()],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(raw
                    .map(|v| serde_json::from_str(&v))
                    .transpose()?
                    .unwrap_or(serde_json::Value::Null))
            })
            .await
    }
    async fn all_subject(&self) -> Result<Vec<serde_json::Value>> {
        self.s
            .call(|c| {
                let mut q = c.prepare(
                    "SELECT payload FROM setup_entries WHERE device=? ORDER BY fingerprint",
                )?;
                q.query_map([Id::from_bytes([0x33; 16]).as_str()], |r| {
                    r.get::<_, String>(0)
                })?
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect()
            })
            .await
    }
    async fn pair(&self, mut m: Member) -> Result<()> {
        let floor = self.now - 1;
        self.s
            .call(move |c| {
                c.execute(
                    "UPDATE mesh_clock SET last_seen=max(last_seen,?)",
                    [floor.to_string()],
                )?;
                Ok(())
            })
            .await?;
        m.status = MemberStatus::Member;
        let hello = Hello {
            version: 1,
            device_id: m.device_id.clone(),
            name: m.name,
            capabilities: vec!["mesh".into(), "setup".into()],
            paused: None,
            role: Some(m.role),
            addresses: None,
            setup: Some(SetupHello {
                scope: SetupScope::Enabled,
                setup_id: Some(id(0)),
            }),
        };
        self.s
            .pair_then_hello_scoped(
                m.device_id,
                m.fingerprint,
                Some(hello),
                Some(self.local.clone()),
                SetupScope::Enabled,
                false,
                || {},
            )
            .await?;
        Ok(())
    }
}

async fn binding_pin(s: &Store, subject: &Id) -> Result<Option<Hash>> {
    Ok(s.peers()
        .await?
        .into_iter()
        .find(|(_, d)| d == subject)
        .map(|(p, _)| p))
}

#[tokio::test]
async fn setup_shared_readmission_sequences() -> Result<()> {
    use serde_json::{Value, json};
    let v: Value =
        serde_json::from_str(include_str!("../../../protocol-vectors/readmission.json"))?;
    let old_pin = Hash::new("c".repeat(64))?;
    let new_pin = Hash::new("e".repeat(64))?;
    let subject = Id::from_bytes([0x33; 16]);
    for scenario in v["scenarios"].as_array().unwrap() {
        // The presentation-only sequence runs against the real window model in
        // ui::window::tests::setup_shared_readmission_silent_keeper_vector.
        if scenario["name"] == "silent-keeper-neutral-status" {
            continue;
        }
        let mut h = ReadmitVectors::new(&v).await?;
        let mut captured = None;
        let mut blocked = Vec::new();
        let mut filled = Vec::new();
        let mut holder_results = serde_json::Map::new();
        for step in scenario["steps"].as_array().unwrap() {
            let context = format!("{}: {step}", scenario["name"]);
            let before = h.s.setup_state();
            let before_subject = h.all_subject().await?;
            let settings_subject = subject.clone();
            let before_settings =
                h.s.call(move |c| settings::fields(c, &settings_subject))
                    .await?;
            let before_generation = h.s.pairing_generation(&subject).unwrap_or(0);
            let before_notices: i64 = h
                .s
                .call(|c| Ok(c.query_row("SELECT count(*) FROM setup_notices", [], |r| r.get(0))?))
                .await?;
            let mut dedup = None;
            let mut result = Ok(());
            if let Some(receive) = step["receive"].as_str() {
                let fixture = &v["fixtures"][receive];
                let sender_id: Id =
                    serde_json::from_value(fixture["authenticated_sender"].clone())?;
                let sender =
                    h.s.setup_state()
                        .roster
                        .into_iter()
                        .find(|m| m.device_id == sender_id && m.status == MemberStatus::Member)
                        .or_else(|| {
                            [h.a.clone(), h.d.clone()]
                                .into_iter()
                                .find(|m| m.device_id == sender_id)
                        })
                        .expect(&context);
                let snapshot: Members = serde_json::from_value(fixture["payload"].clone())?;
                result =
                    h.s.members_merge_at(
                        h.fence(&sender),
                        h.local.clone(),
                        snapshot,
                        blocked.clone(),
                        h.now,
                        |_| {},
                    )
                    .await
                    .map(|d| {
                        dedup = Some(d);
                    });
            } else if let Some(action) = step["local_action"].as_str() {
                match action {
                    "dismiss" => {}
                    "reset-to-initial-state" => {
                        h = ReadmitVectors::new(&v).await?;
                        blocked.clear();
                    }
                    "remove-C-from-all-devices" => {
                        let clock = step["stamp"]["hlc"].as_u64().unwrap();
                        h.s.call(move |c| {
                            let previous: String =
                                c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?;
                            anyhow::ensure!(
                                previous.parse::<u64>()? < clock,
                                "vector must never rewind the clock"
                            );
                            c.execute(
                                "UPDATE mesh_clock SET last_seen=?",
                                [(clock - 1).to_string()],
                            )?;
                            Ok(())
                        })
                        .await?;
                        h.s.setup_remove_at(
                            h.local.device_id.clone(),
                            subject.clone(),
                            clock,
                            |_| {},
                        )
                        .await?;
                        let actual = h
                            .all_subject()
                            .await?
                            .into_iter()
                            .find(|e| e["denied"] == true && e["removal"]["hlc"] == clock)
                            .expect(&context);
                        assert_eq!(
                            actual["removal"]["writer_device"], step["stamp"]["writer_device"],
                            "{context}"
                        );
                    }
                    "authenticate-C-ready" | "authenticate-A-ready" => {
                        let target = if action == "authenticate-C-ready" {
                            subject.clone()
                        } else {
                            h.a.device_id.clone()
                        };
                        let m =
                            h.s.setup_state()
                                .roster
                                .into_iter()
                                .find(|m| m.device_id == target && m.status == MemberStatus::Member)
                                .expect(&context);
                        h.s.setup_activate(
                            h.fence(&m),
                            h.local.clone(),
                            m.clone(),
                            SetupHello {
                                scope: SetupScope::Enabled,
                                setup_id: Some(id(0)),
                            },
                            |_| {},
                        )
                        .await?;
                        if m.role == DeviceRole::Computer {
                            h.s.mark_peer_established(target).await?;
                        }
                    }
                    "load-legacy-barrier-without-origin" => {
                        let removal: Members =
                            serde_json::from_value(v["fixtures"]["remove"]["payload"].clone())?;
                        let m = removal
                            .members
                            .into_iter()
                            .find(|m| m.device_id == subject)
                            .unwrap();
                        h.s.call(move |c| {
                            let payload =
                                json!({"value":m,"removal":m,"denied":true,"setup_id":id(0)});
                            c.execute(
                                "INSERT INTO setup_entries VALUES(?,?,?)",
                                params![
                                    m.device_id.as_str(),
                                    m.fingerprint.as_str(),
                                    payload.to_string()
                                ],
                            )?;
                            Ok(())
                        })
                        .await?;
                        ReadmitVectors::reload(&h.s).await?;
                    }
                    "load-legacy-member-without-claim" => {
                        let snapshot: Members = serde_json::from_value(
                            v["fixtures"]["later-introduction-unclaimed"]["payload"].clone(),
                        )?;
                        h.s.members_merge_at(
                            h.fence(&h.a),
                            h.local.clone(),
                            snapshot,
                            vec![],
                            h.now,
                            |_| {},
                        )
                        .await?;
                    }
                    "arrange-departed-keeper-and-fresh-repairer" => {
                        let removal: Members = serde_json::from_value(
                            v["fixtures"]["departed-keeper-removal"]["payload"].clone(),
                        )?;
                        let claimed: Members = serde_json::from_value(
                            v["fixtures"]["departed-keeper-claimed-member"]["payload"].clone(),
                        )?;
                        let r = removal
                            .members
                            .iter()
                            .find(|m| m.device_id == subject)
                            .unwrap();
                        let m = claimed
                            .members
                            .iter()
                            .find(|m| m.device_id == subject)
                            .unwrap();
                        assert_eq!(
                            json!({"hlc":r.hlc,"writer_device":r.writer_device}),
                            step["keeper"]["removal"]
                        );
                        assert!(m.readmits.as_ref().is_some_and(|c| (
                            c.hlc,
                            c.writer_device.as_str()
                        ) >= (
                            r.hlc,
                            r.writer_device.as_str()
                        )));
                    }
                    "deliver-exported-snapshot-to-independent-holders" => {
                        let payload = h.s.members_outgoing(h.fence(&h.a)).await?;
                        for holder in step["holders"].as_array().unwrap() {
                            let receiver: Id = serde_json::from_value(holder["device_id"].clone())?;
                            let mut hv = v.clone();
                            let all = v["fixtures"]["join"]["payload"]["members"]
                                .as_array()
                                .unwrap();
                            let row = |id: &Id| {
                                all.iter()
                                    .find(|m| m["device_id"] == json!(id))
                                    .unwrap()
                                    .clone()
                            };
                            let other = if receiver == h.a.device_id {
                                h.d.device_id.clone()
                            } else {
                                h.a.device_id.clone()
                            };
                            hv["fixtures"]["join"]["payload"]["members"] = json!([
                                row(&h.local.device_id),
                                row(&receiver),
                                all[2],
                                row(&other)
                            ]);
                            let trial = ReadmitVectors::new(&hv).await?;
                            let authored = holder["origin"]["authored"].as_bool().unwrap();
                            if authored {
                                let join: Members = serde_json::from_value(
                                    hv["fixtures"]["join"]["payload"].clone(),
                                )?;
                                trial
                                    .s
                                    .members_merge_at(
                                        trial.fence(&trial.a),
                                        trial.local.clone(),
                                        join,
                                        vec![],
                                        h.now,
                                        |_| {},
                                    )
                                    .await?;
                                let clock =
                                    holder["previous_local_removal"]["hlc"].as_u64().unwrap();
                                trial
                                    .s
                                    .setup_remove_at(
                                        receiver.clone(),
                                        subject.clone(),
                                        clock,
                                        |_| {},
                                    )
                                    .await?;
                            }
                            let removal: Members =
                                serde_json::from_value(v["fixtures"]["remove"]["payload"].clone())?;
                            trial
                                .s
                                .members_merge_at(
                                    trial.fence(&trial.a),
                                    trial.local.clone(),
                                    removal,
                                    vec![],
                                    h.now,
                                    |_| {},
                                )
                                .await?;
                            let notices: i64 = trial
                                .s
                                .call(|c| {
                                    Ok(c.query_row(
                                        "SELECT count(*) FROM setup_notices",
                                        [],
                                        |r| r.get(0),
                                    )?)
                                })
                                .await?;
                            trial
                                .s
                                .members_merge_at(
                                    trial.fence(&trial.a),
                                    trial.local.clone(),
                                    payload.clone(),
                                    vec![],
                                    h.now,
                                    |_| {},
                                )
                                .await?;
                            let after_notices: i64 = trial
                                .s
                                .call(|c| {
                                    Ok(c.query_row(
                                        "SELECT count(*) FROM setup_notices",
                                        [],
                                        |r| r.get(0),
                                    )?)
                                })
                                .await?;
                            holder_results.insert(if authored {"authored_holder"} else {"inherited_holder"}.into(),json!({"binding_C":trial.s.peers().await?.iter().any(|(_,d)|*d==subject),"pending_questions_for_C":trial.s.setup_state().readmissions.len(),"joined_notices_delta":after_notices-notices}));
                        }
                    }
                    "restart" => {
                        let path = h.dir.path().join("state.sqlite3");
                        drop(h.s);
                        h.s = Store::open(&path)?;
                    }
                    "evict-membership-receipts" => {
                        h.s.call(|c| {
                            c.execute("DELETE FROM members_receipts", [])?;
                            Ok(())
                        })
                        .await?
                    }
                    "capture-allow-notification" => {
                        captured =
                            h.s.setup_state()
                                .readmissions
                                .first()
                                .map(|r| r.key.clone())
                    }
                    "invoke-captured-allow" => {
                        if let Some(key) = captured.clone() {
                            result =
                                h.s.setup_readmit(
                                    key,
                                    h.local.device_id.clone(),
                                    true,
                                    blocked.clone(),
                                    |_| {},
                                )
                                .await;
                        }
                    }
                    "allow-C"
                    | "allow-C-again"
                    | "keep-removed-C"
                    | "cancel-attempt-and-retry-allow-C" => {
                        if action == "cancel-attempt-and-retry-allow-C" {
                            blocked.clear();
                        }
                        let key =
                            h.s.setup_state()
                                .readmissions
                                .first()
                                .map(|r| r.key.clone())
                                .or(captured.clone());
                        if let Some(key) = key {
                            captured = Some(key.clone());
                            result =
                                h.s.setup_readmit(
                                    key,
                                    h.local.device_id.clone(),
                                    action != "keep-removed-C",
                                    blocked.clone(),
                                    |_| {},
                                )
                                .await;
                        }
                    }
                    "fresh-token-pair-C-with-enabled-scope"
                    | "fresh-token-pair-C-new-pin-with-enabled-scope" => {
                        let mut m: Members =
                            serde_json::from_value(v["fixtures"]["join"]["payload"].clone())?;
                        let mut c = m.members.remove(2);
                        if action.contains("new-pin") {
                            c.fingerprint = new_pin.clone();
                        }
                        h.pair(c).await?;
                    }
                    "retire-and-rejoin-S-via-A" => {
                        h.s.setup_withdraw(h.fence(&h.a), |_| {}).await?;
                        h.pair(h.a.clone()).await?;
                        h.pair(h.d.clone()).await?;
                    }
                    "fresh-token-rejoin-via-A" => h.pair(h.a.clone()).await?,
                    "fill-active-roster-to-17" => {
                        let count =
                            h.s.setup_state()
                                .roster
                                .iter()
                                .filter(|m| m.status == MemberStatus::Member)
                                .count();
                        let mut roster = h.s.setup_state().roster;
                        for n in 80..80 + (17 - count) as u8 {
                            let mut m = member(n);
                            m.hlc = h.now;
                            m.writer_device = h.local.device_id.clone();
                            filled.push(m.device_id.clone());
                            roster.push(m);
                        }
                        let snapshot = Members {
                            setup_id: id(0),
                            transfer_id: Id::from_bytes([90; 16]),
                            members: roster,
                        };
                        h.s.members_merge_at(
                            h.fence(&h.a),
                            h.local.clone(),
                            snapshot,
                            vec![],
                            h.now,
                            |_| {},
                        )
                        .await?;
                    }
                    "free-active-slot-and-start-direct-attempt-targeting-C-pin" => {
                        h.s.setup_remove(h.local.device_id.clone(), filled.pop().unwrap(), |_| {})
                            .await?;
                        blocked.push((None, old_pin.clone()));
                    }
                    _ => panic!("unhandled action {action}"),
                }
            }
            let expect = &step["expect"];
            let expected_error = expect.get("error").is_some()
                || expect.get("nack_code").is_some()
                || expect.get("visible_error").is_some()
                || expect["deferred"] == true;
            assert_eq!(result.is_err(), expected_error, "{context}: {result:?}");
            if expect["error"] == "malformed_message" {
                assert!(
                    matches!(
                        result.as_ref().unwrap_err().downcast_ref::<MembersError>(),
                        Some(MembersError::Malformed)
                    ),
                    "{context}"
                );
            }
            if expect["visible_error"] == "setup_capacity" {
                assert!(
                    matches!(
                        result.as_ref().unwrap_err().downcast_ref::<MembersError>(),
                        Some(MembersError::Capacity)
                    ),
                    "{context}"
                );
            }
            if let Some(code) = expect["nack_code"].as_str() {
                if code == "id_conflict" {
                    assert!(
                        result.as_ref().unwrap_err().is::<ReceiptConflict>(),
                        "{context}"
                    );
                } else {
                    assert_eq!(code, "unsupported_kind", "{context}");
                    assert!(
                        h.s.setup_state().withdrawn
                            && result.as_ref().unwrap_err().is::<PairingRevoked>(),
                        "{context}"
                    );
                }
            }
            if expect["deferred"] == true {
                assert!(
                    result
                        .as_ref()
                        .unwrap_err()
                        .to_string()
                        .contains("Pairing in progress"),
                    "{context}"
                );
            }
            let entries = h.all_subject().await?;
            let record = entries
                .iter()
                .find_map(|e| e.get("readmission").filter(|r| !r.is_null()));
            let current_pin = record
                .and_then(|r| r["key"]["fingerprint"].as_str())
                .map(Hash::new)
                .transpose()?
                .or_else(|| {
                    entries
                        .iter()
                        .find(|e| {
                            e["denied"] == true && e["value"]["fingerprint"] == json!(new_pin)
                        })
                        .and_then(|e| e["value"]["fingerprint"].as_str())
                        .map(Hash::new)
                        .transpose()
                        .unwrap()
                })
                .or(binding_pin(&h.s, &subject).await?)
                .unwrap_or(old_pin.clone());
            let entry = h.entry(&current_pin).await?;
            let after = h.s.setup_state();
            let binding =
                h.s.peers()
                    .await?
                    .into_iter()
                    .find(|(_, d)| *d == subject)
                    .map(|(p, _)| p);
            let mut actual = serde_json::Map::new();
            actual.insert("denied".into(), json!(entry["denied"] == true));
            actual.insert("origin".into(),json!({"authored":entry["authored"].as_bool().unwrap_or(true),"revoked_binding":entry["revoked_binding"].as_bool().unwrap_or(false)}));
            actual.insert(
                "origin_effective_authored".into(),
                json!(entry["authored"].as_bool().unwrap_or(true)),
            );
            for field in ["readmits", "retained_claim"] {
                actual.insert(field.into(), entry["value"]["readmits"].clone());
            }
            for (field, value) in &holder_results {
                actual.insert(field.clone(), value.clone());
            }
            actual.insert(
                "exported_C".into(),
                json!(
                    after
                        .roster
                        .iter()
                        .find(|m| m.device_id == subject && m.fingerprint == current_pin)
                        .map(|m| if m.status == MemberStatus::Member {
                            "member"
                        } else {
                            "removed"
                        })
                ),
            );
            actual.insert("binding_C".into(), json!(binding.is_some()));
            actual.insert("binding_C_pin".into(), json!(binding));
            for field in ["removal", "removal_high_water"] {
                actual.insert(field.into(),if entry["removal"].is_null(){Value::Null}else{json!({"hlc":entry["removal"]["hlc"],"writer_device":entry["removal"]["writer_device"]})});
            }
            actual.insert(
                "record_decision".into(),
                record.map(|r| r["decision"].clone()).unwrap_or(Value::Null),
            );
            actual.insert(
                "pending_questions_for_C".into(),
                json!(
                    after
                        .readmissions
                        .iter()
                        .filter(|r| r.key.device == subject)
                        .count()
                ),
            );
            actual.insert(
                "pending_questions_for_own_key".into(),
                json!(
                    after
                        .readmissions
                        .iter()
                        .filter(|r| r.key.device == h.local.device_id)
                        .count()
                ),
            );
            actual.insert(
                "introducer".into(),
                record.map(|r| r["introducer"].clone()).unwrap_or_else(|| {
                    json!(
                        after
                            .links
                            .iter()
                            .find(|l| l.device == subject)
                            .and_then(|l| l.introduced_by.clone())
                    )
                }),
            );
            for (field, source) in [
                ("value_hlc", "hlc"),
                ("value_writer", "writer_device"),
                ("value_name", "name"),
            ] {
                actual.insert(
                    field.into(),
                    record
                        .map(|r| r["value"][source].clone())
                        .unwrap_or_else(|| entry["value"][source].clone()),
                );
            }
            actual.insert("record_barrier".into(),record.map(|r|json!({"hlc":r["key"]["removal_hlc"],"writer_device":r["key"]["removal_writer"]})).unwrap_or(Value::Null));
            actual.insert("ack_deduplicated".into(), json!(dedup));
            actual.insert("roster_resend".into(), json!(before.roster != after.roster));
            actual.insert(
                "binding_generation_delta".into(),
                json!(
                    h.s.pairing_generation(&subject).unwrap_or(0) as i128
                        - before_generation as i128
                ),
            );
            actual.insert(
                "old_pin_denied".into(),
                h.entry(&old_pin).await?["denied"].clone(),
            );
            actual.insert(
                "new_pin_denied".into(),
                h.entry(&new_pin).await?["denied"].clone(),
            );
            actual.insert(
                "old_pin_barrier_retained".into(),
                json!(!h.entry(&old_pin).await?["removal"].is_null()),
            );
            actual.insert(
                "setup_generation_advanced".into(),
                json!(after.generation > before.generation),
            );
            actual.insert("pending_record_discarded".into(), json!(record.is_none()));
            actual.insert("withdrawn".into(), json!(after.withdrawn));
            actual.insert("setup_id".into(), json!(after.setup_id));
            actual.insert(
                "retired_roster_exported".into(),
                json!(after.roster.iter().any(|m| m.device_id == subject)),
            );
            let after_notices: i64 = h
                .s
                .call(|c| Ok(c.query_row("SELECT count(*) FROM setup_notices", [], |r| r.get(0))?))
                .await?;
            actual.insert(
                "joined_notices_delta".into(),
                json!(after_notices - before_notices),
            );
            for (key, expected) in expect.as_object().into_iter().flatten() {
                if let Some(got) = actual.get(key) {
                    if key == "authored_holder" || key == "inherited_holder" {
                        for (k, v) in expected.as_object().unwrap() {
                            assert_eq!(&got[k], v, "{context}: {key}.{k}");
                        }
                    } else {
                        assert_eq!(got, expected, "{context}: {key}");
                    }
                    continue;
                }
                match key.as_str() {
                    "record" => {
                        if expected.is_null() {
                            assert!(record.is_none(), "{context}");
                        } else {
                            let r = record.expect(&context);
                            for (k, val) in expected.as_object().unwrap() {
                                let got = match k.as_str() {
                                    "value_hlc" => &r["value"]["hlc"],
                                    "value_writer" => &r["value"]["writer_device"],
                                    "value_name" => &r["value"]["name"],
                                    _ => &r[k],
                                };
                                assert_eq!(got, val, "{context}: record.{k}");
                            }
                        }
                    }
                    "state_unchanged" => {
                        assert_eq!(before, after, "{context}");
                        assert_eq!(before_subject, entries, "{context}");
                    }
                    "new_question" => assert_eq!(
                        before.readmissions.len(),
                        after.readmissions.len(),
                        "{context}"
                    ),
                    "scope" => assert_eq!(
                        after
                            .links
                            .iter()
                            .find(|l| l.device == subject)
                            .unwrap()
                            .scope,
                        SetupScope::Enabled,
                        "{context}"
                    ),
                    "readiness" => {
                        assert_eq!(
                            h.s.peer_established(subject.clone()).await?,
                            expected == 1,
                            "{context}"
                        )
                    }
                    "hello_seen" => assert!(
                        h.s.setup_state()
                            .links
                            .iter()
                            .find(|l| l.device == subject)
                            .is_some_and(|l| l.hello_seen)
                            == expected.as_bool().unwrap(),
                        "{context}"
                    ),
                    "authored_values_delta" => assert_eq!(before.roster, after.roster, "{context}"),
                    "diagnostic" => {
                        let raw: String =
                            h.s.call(move |c| {
                                Ok(c.query_row("SELECT payload FROM setup_conflicts", [], |r| {
                                    r.get(0)
                                })?)
                            })
                            .await?;
                        let m: Member = serde_json::from_str(&raw)?;
                        assert_eq!(
                            format!(
                                "{} has a different certificate · Pair with it directly",
                                m.name
                            ),
                            expected.as_str().unwrap(),
                            "{context}"
                        );
                    }
                    "dial_C" => assert!(binding.is_none(), "{context}"),
                    "new_member_tuple_greater_than" => assert!(
                        entry["value"]["hlc"].as_u64().unwrap() > expected["hlc"].as_u64().unwrap(),
                        "{context}"
                    ),
                    "member_hlc" => assert_eq!(
                        h.s.setup_state()
                            .roster
                            .iter()
                            .find(|m| m.device_id == subject)
                            .unwrap()
                            .hlc,
                        expected.as_u64().unwrap(),
                        "{context}"
                    ),
                    "settings_bootstrap" => {
                        let d = subject.clone();
                        let fields = h.s.call(move |c| settings::fields(c, &d)).await?;
                        assert_eq!(fields.len(), 2, "{context}");
                        assert!(fields.iter().all(|f| f.enabled && f.hlc == 0), "{context}");
                    }
                    "settings_reset" => {
                        let d = subject.clone();
                        assert_eq!(
                            before_settings,
                            h.s.call(move |c| settings::fields(c, &d)).await?,
                            "{context}"
                        );
                    }
                    "own_readmission_record" => assert!(
                        !after
                            .readmissions
                            .iter()
                            .any(|r| r.key.device == h.local.device_id),
                        "{context}"
                    ),
                    "own_deny_cleared" | "own_removal_high_water" => {
                        let own = h.local.clone();
                        let entry:Value=h.s.call(move |c| { let raw:String=c.query_row("SELECT payload FROM setup_entries WHERE device=? AND fingerprint=?",params![own.device_id.as_str(),own.fingerprint.as_str()],|r|r.get(0))?;Ok(serde_json::from_str(&raw)?) }).await?;
                        if key == "own_deny_cleared" {
                            assert_eq!(entry["denied"], false, "{context}");
                        } else {
                            assert_eq!(entry["removal"]["hlc"], expected["hlc"], "{context}");
                            assert_eq!(
                                entry["removal"]["writer_device"], expected["writer_device"],
                                "{context}"
                            );
                        }
                    }
                    "local_identity_unchanged" => {
                        assert_eq!(h.local.device_id, Id::from_bytes([0x22; 16]));
                        assert_eq!(h.local.fingerprint, Hash::new("b".repeat(64))?);
                    }
                    "membership_stamp_unchanged" => {
                        assert_eq!(
                            before_subject
                                .iter()
                                .find(|e| e["value"]["fingerprint"] == json!(current_pin))
                                .unwrap()["value"]["hlc"],
                            entry["value"]["hlc"],
                            "{context}"
                        );
                    }
                    "retryable" => {
                        assert_eq!(expected, false, "{context}");
                        assert!(result.is_err(), "{context}");
                    }
                    "error" | "nack_code" | "visible_error" | "deferred" => {}
                    _ => panic!("unhandled assertion {key}: {context}"),
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn setup_readmission_transaction_failures_and_independent_reach() -> Result<()> {
    let f = Fixture::new().await?;
    let mut removed = member(3);
    removed.status = MemberStatus::Removed;
    removed.hlc = 10;
    f.legacy_authored_removal(removed.clone()).await?;
    let mut newer = member(3);
    newer.hlc = 20;
    newer.readmits = Some(MemberReadmits {
        hlc: 10,
        writer_device: id(1),
    });
    newer.reach = Some(MemberReach {
        addresses: vec!["192.168.1.12:47821".into()],
        relay: vec![],
        hlc: 30,
        writer_device: id(3),
    });
    f.merge(f.snapshot(vec![newer.clone()])).await?;
    let key = f.s.setup_state().readmissions[0].key.clone();
    let mut renamed = newer.clone();
    renamed.hlc = 21;
    renamed.name = "Renamed".into();
    renamed.reach.as_mut().unwrap().hlc = 25;
    f.merge(f.snapshot(vec![renamed])).await?;
    let record = &f.s.setup_state().readmissions[0];
    assert_eq!(record.value.name, "Renamed");
    assert_eq!(record.value.reach.as_ref().unwrap().hlc, 30);
    let before = f.s.setup_state();
    for trigger in [
        "CREATE TEMP TRIGGER readmit_fail BEFORE INSERT ON peers BEGIN SELECT RAISE(ABORT,'install failed'); END;",
        "CREATE TEMP TRIGGER readmit_fail BEFORE INSERT ON peer_settings BEGIN SELECT RAISE(ABORT,'settings failed'); END;",
    ] {
        f.s.call(move |c| {
            c.execute_batch(trigger)?;
            Ok(())
        })
        .await?;
        assert!(
            f.s.setup_readmit(key.clone(), id(2), true, vec![], |_| panic!(
                "failed transaction published"
            ))
            .await
            .is_err()
        );
        assert_eq!(f.s.setup_state(), before);
        assert!(f.pin(3).await?.is_none());
        f.s.call(|c| {
            c.execute_batch("DROP TRIGGER readmit_fail;")?;
            Ok(())
        })
        .await?;
    }
    f.s.fail_next("members_before_commit", 1);
    assert!(
        f.s.setup_readmit(key.clone(), id(2), true, vec![], |_| panic!(
            "failed commit published"
        ))
        .await
        .is_err()
    );
    assert_eq!(f.s.setup_state(), before);
    assert!(f.pin(3).await?.is_none());
    // A conflict or any live attempt (including unresolved fingerprint only)
    // leaves the record pending. Ending an attempt makes the same Allow work.
    for blocked in [
        vec![(Some(id(3)), member(9).fingerprint)],
        vec![(None, member(3).fingerprint.clone())],
    ] {
        assert!(
            f.s.setup_readmit(key.clone(), id(2), true, blocked, |_| {})
                .await
                .is_err()
        );
        assert_eq!(f.s.setup_state(), before);
    }
    f.s.pair(id(8), member(3).fingerprint.clone()).await?;
    assert!(
        f.s.setup_readmit(key.clone(), id(2), true, vec![], |_| {})
            .await
            .is_err()
    );
    assert!(f.pin(3).await?.is_none());
    f.s.unpair_then(id(8), |_| {}).await?;
    f.s.setup_readmit(key.clone(), id(2), true, vec![], |_| {})
        .await?;
    assert!(f.s.setup_protected(&id(3)));
    let generation = f.s.pairing_generation(&id(3));
    // An edited setting and its outgoing receipt survive repeated Allow/replay.
    f.s.call(|c| {
        c.execute(
            "UPDATE peer_settings SET enabled=0,hlc='23' WHERE device=?",
            [id(3).as_str()],
        )?;
        Ok(())
    })
    .await?;
    let settings = f.s.call(|c| settings::fields(c, &id(3))).await?;
    f.s.setup_readmit(key, id(2), true, vec![], |_| {}).await?;
    newer.hlc = 22;
    f.merge(f.snapshot(vec![newer])).await?;
    assert_eq!(settings, f.s.call(|c| settings::fields(c, &id(3))).await?);
    assert_eq!(generation, f.s.pairing_generation(&id(3)));
    assert_eq!(
        f.s.setup_state()
            .links
            .iter()
            .find(|l| l.device == id(3))
            .unwrap()
            .introduced_by,
        Some(id(1))
    );
    let count: i64 =
        f.s.call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM setup_notices WHERE device=?",
                [id(3).as_str()],
                |r| r.get(0),
            )?)
        })
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn setup_readmission_is_per_key_and_keeps_introduced_protection_after_legacy_hello()
-> Result<()> {
    let f = Fixture::new().await?;
    let mut removed = member(3);
    removed.status = MemberStatus::Removed;
    removed.hlc = 10;
    f.legacy_authored_removal(removed).await?;
    let mut newer = member(3);
    newer.hlc = 20;
    newer.readmits = Some(MemberReadmits {
        hlc: 10,
        writer_device: id(1),
    });
    f.merge(f.snapshot(vec![newer])).await?;
    let key = f.s.setup_state().readmissions[0].key.clone();
    f.s.pair(id(4), member(4).fingerprint).await?;
    f.s.setup_readmit(key, id(2), true, vec![], |_| {}).await?;
    assert_eq!(
        f.s.setup_state()
            .links
            .iter()
            .find(|l| l.device == id(4))
            .unwrap()
            .scope,
        SetupScope::Ask
    );
    let fence = SetupFence {
        generation: f.s.setup_state().generation,
        binding_generation: f.s.pairing_generation(&id(3)).unwrap(),
        device: id(3),
        fingerprint: member(3).fingerprint,
        connection: 3,
        current: Arc::new(AtomicU64::new(3)),
    };
    f.s.setup_legacy_ready(fence).await?;
    let link =
        f.s.setup_state()
            .links
            .into_iter()
            .find(|l| l.device == id(3))
            .unwrap();
    assert!(link.hello_seen);
    assert!(!link.negotiated);
    assert!(f.s.setup_protected(&id(3)));
    // Allowed decisions survive retirement and a non-token HELLO activation.
    // A later genuinely fresh local token confirmation drops just this record.
    f.s.setup_withdraw(f.f.clone(), |_| {}).await?;
    assert!(f.pin(3).await?.is_some());
    let hello = Hello {
        version: 1,
        device_id: id(1),
        name: "Introducer".into(),
        capabilities: vec!["mesh".into(), "setup".into()],
        paused: None,
        role: Some(DeviceRole::Computer),
        addresses: None,
        setup: Some(SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(0)),
        }),
    };
    f.s.pair_then_hello_scoped(
        id(1),
        member(1).fingerprint,
        Some(hello),
        Some(member(2)),
        SetupScope::Enabled,
        false,
        || {},
    )
    .await?;
    let fence = SetupFence {
        generation: f.s.setup_state().generation,
        binding_generation: f.s.pairing_generation(&id(3)).unwrap(),
        device: id(3),
        fingerprint: member(3).fingerprint,
        connection: 4,
        current: Arc::new(AtomicU64::new(4)),
    };
    f.s.setup_activate(
        fence,
        member(2),
        member(3),
        SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(0)),
        },
        |_| {},
    )
    .await?;
    let decision:String=f.s.call(|c|Ok(c.query_row("SELECT json_extract(payload,'$.readmission.decision') FROM setup_entries WHERE device=? AND fingerprint=?",params![id(3).as_str(),member(3).fingerprint.as_str()],|r|r.get(0))?)).await?;
    assert_eq!(decision, "allowed");
    f.s.setup_fresh_local_recovery(id(3), member(2), |_| {})
        .await?;
    let cleared:bool=f.s.call(|c|Ok(c.query_row("SELECT json_extract(payload,'$.readmission') IS NULL AND json_extract(payload,'$.removal.hlc')=10 FROM setup_entries WHERE device=? AND fingerprint=?",params![id(3).as_str(),member(3).fingerprint.as_str()],|r|r.get(0))?)).await?;
    assert!(cleared);
    Ok(())
}

#[tokio::test]
async fn setup_readmission_observation_and_keep_do_not_change_outgoing_union() -> Result<()> {
    let f = Fixture::new().await?;
    let mut removed = member(3);
    removed.hlc = 10;
    removed.status = MemberStatus::Removed;
    f.legacy_authored_removal(removed).await?;
    let before = f.s.members_outgoing(f.f.clone()).await?;
    f.s.members_ack(f.f.clone(), before.transfer_id.clone())
        .await?;
    let mut newer = member(3);
    newer.hlc = 20;
    newer.readmits = Some(MemberReadmits {
        hlc: 10,
        writer_device: id(1),
    });
    f.s.members_merge(
        f.f.clone(),
        member(2),
        f.snapshot(vec![newer.clone()]),
        vec![],
        |delta| {
            assert!(!delta.roster_changed);
            assert!(delta.added.is_empty());
            assert!(delta.removed.is_empty());
        },
    )
    .await?;
    let record = f.s.setup_state().readmissions[0].clone();
    assert_eq!(before, f.s.members_outgoing(f.f.clone()).await?);
    f.s.members_ack(f.f.clone(), before.transfer_id.clone())
        .await?;
    f.s.setup_readmit(record.key, id(2), false, vec![], |delta| {
        assert!(!delta.roster_changed)
    })
    .await?;
    assert_eq!(before, f.s.members_outgoing(f.f.clone()).await?);
    f.s.members_ack(f.f.clone(), before.transfer_id.clone())
        .await?;
    newer.hlc = 21;
    f.s.members_merge(
        f.f.clone(),
        member(2),
        f.snapshot(vec![newer]),
        vec![],
        |delta| assert!(!delta.roster_changed),
    )
    .await?;
    assert_eq!(before, f.s.members_outgoing(f.f).await?);
    Ok(())
}

#[tokio::test]
async fn setup_new_pin_direct_pair_resolves_diagnostic_and_accepts_metadata() -> Result<()> {
    let f = Fixture::new().await?;
    let mut removed = member(3);
    removed.status = MemberStatus::Removed;
    removed.hlc = 10;
    f.merge(f.snapshot(vec![removed])).await?;
    let mut replacement = member(3);
    replacement.fingerprint = member(9).fingerprint;
    replacement.hlc = 20;
    f.merge(f.snapshot(vec![replacement.clone()])).await?;
    assert!(f.pin(3).await?.is_none());
    let hello = Hello {
        version: 1,
        device_id: id(3),
        name: "Direct".into(),
        capabilities: vec!["mesh".into(), "setup".into()],
        paused: None,
        role: Some(DeviceRole::Computer),
        addresses: None,
        setup: Some(SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(0)),
        }),
    };
    f.s.pair_then_hello_scoped(
        id(3),
        replacement.fingerprint.clone(),
        Some(hello),
        Some(member(2)),
        SetupScope::Enabled,
        false,
        || {},
    )
    .await?;
    let count: i64 =
        f.s.call(|c| Ok(c.query_row("SELECT count(*) FROM setup_conflicts", [], |r| r.get(0))?))
            .await?;
    assert_eq!(count, 0);
    replacement.hlc =
        f.s.setup_state()
            .roster
            .iter()
            .find(|m| m.device_id == id(3) && m.status == MemberStatus::Member)
            .unwrap()
            .hlc
            + 1;
    replacement.name = "Renamed after direct pairing".into();
    f.merge(f.snapshot(vec![replacement.clone()])).await?;
    let count: i64 =
        f.s.call(|c| Ok(c.query_row("SELECT count(*) FROM setup_conflicts", [], |r| r.get(0))?))
            .await?;
    assert_eq!(count, 0);
    assert_eq!(
        f.s.setup_state()
            .roster
            .iter()
            .find(|m| m.device_id == id(3) && m.status == MemberStatus::Member)
            .unwrap()
            .name,
        replacement.name
    );
    assert!(f.s.setup_state().readmissions.is_empty());
    let old_denied:bool=f.s.call(|c|Ok(c.query_row("SELECT json_extract(payload,'$.denied') FROM setup_entries WHERE device=? AND fingerprint=?",params![id(3).as_str(),member(3).fingerprint.as_str()],|r|r.get(0))?)).await?;
    assert!(old_denied);
    Ok(())
}

#[tokio::test]
async fn setup_phone_ready_evidence_is_local_durable_and_generation_scoped() -> Result<()> {
    let mut f = Fixture::new().await?;
    let mut phone = member(3);
    phone.role = DeviceRole::Phone;
    f.merge(f.snapshot(vec![phone.clone()])).await?;
    assert!(!f.s.setup_binding_ready(id(3)).await?);
    let roster = f.s.setup_state().roster;
    let fence = SetupFence {
        generation: f.s.setup_state().generation,
        binding_generation: f.s.pairing_generation(&id(3)).unwrap(),
        device: id(3),
        fingerprint: phone.fingerprint.clone(),
        connection: 3,
        current: Arc::new(AtomicU64::new(3)),
    };
    f.s.setup_activate(
        fence,
        member(2),
        phone.clone(),
        SetupHello {
            scope: SetupScope::Enabled,
            setup_id: Some(id(0)),
        },
        |_| {},
    )
    .await?;
    assert!(f.s.setup_binding_ready(id(3)).await?);
    assert!(
        !f.s.peer_established(id(3)).await?,
        "phone READY must not supply computer revocation evidence"
    );
    assert_eq!(
        f.s.setup_state().roster,
        roster,
        "READY is local, never a roster write"
    );
    let path = f._dir.path().join("state.sqlite3");
    drop(f.s);
    f.s = Store::open(&path)?;
    assert!(f.s.setup_binding_ready(id(3)).await?);
    f.s.pair(id(3), phone.fingerprint).await?;
    assert!(!f.s.setup_binding_ready(id(3)).await?);
    Ok(())
}

async fn new_pin_with_same_snapshot_removal(old_first: bool) -> Result<()> {
    for reverse_wire_order in [false, true] {
        let f = Fixture::new().await?;
        let mut removed = member(3);
        removed.fingerprint = member(if old_first { 4 } else { 9 }).fingerprint;
        removed.status = MemberStatus::Removed;
        removed.hlc = 10;
        let mut replacement = member(3);
        replacement.fingerprint = member(if old_first { 9 } else { 4 }).fingerprint;
        replacement.hlc = 20;
        let mut snapshot = f.snapshot(vec![removed.clone(), replacement]);
        if reverse_wire_order {
            snapshot.members.reverse();
        }
        assert!(f.pin(3).await?.is_none());
        assert!(!f.merge(snapshot.clone()).await?);
        assert!(
            f.pin(3).await?.is_none(),
            "a same-snapshot other-pin denial must block installation"
        );
        assert!(f.merge(snapshot).await?, "receipt replay");
        f.s.setup_reconcile_candidates(id(2), vec![], |_| {})
            .await?;
        assert!(f.pin(3).await?.is_none());
        assert!(f.s.setup_state().readmissions.is_empty());
        assert!(f.s.setup_state().roster.contains(&removed));
        assert!(
            !f.s.setup_state()
                .roster
                .iter()
                .any(|m| m.device_id == id(3) && m.status == MemberStatus::Member)
        );
        let conflicts: i64 = f
            .s
            .call(|c| Ok(c.query_row("SELECT count(*) FROM setup_conflicts", [], |r| r.get(0))?))
            .await?;
        assert_eq!(conflicts, 1);
    }
    Ok(())
}

#[tokio::test]
async fn setup_regression_review_new_pin_before_removed_pin() -> Result<()> {
    new_pin_with_same_snapshot_removal(false).await
}

#[tokio::test]
async fn setup_regression_review_new_pin_after_removed_pin() -> Result<()> {
    new_pin_with_same_snapshot_removal(true).await
}

#[tokio::test]
async fn setup_regression_review_legacy_phone_ready_is_generation_fenced() -> Result<()> {
    let mut f = Fixture::new().await?;
    let mut phone = member(3);
    phone.role = DeviceRole::Phone;
    f.merge(f.snapshot(vec![phone.clone()])).await?;
    let roster = f.s.setup_state().roster;
    let fence = SetupFence {
        generation: f.s.setup_state().generation,
        binding_generation: f.s.pairing_generation(&id(3)).unwrap(),
        device: id(3),
        fingerprint: phone.fingerprint.clone(),
        connection: 3,
        current: Arc::new(AtomicU64::new(3)),
    };
    let mut stale = fence.clone();
    stale.connection = 2;
    assert!(f.s.setup_legacy_ready(stale).await.is_err());
    assert!(!f.s.setup_binding_ready(id(3)).await?);
    f.s.setup_legacy_ready(fence.clone()).await?;
    assert!(
        f.s.setup_binding_ready(id(3)).await?,
        "legacy phone READY must count for Also linked"
    );
    assert!(!f.s.peer_established(id(3)).await?);
    assert_eq!(f.s.setup_state().roster, roster);
    assert!(f.s.setup_protected(&id(3)));
    let path = f._dir.path().join("state.sqlite3");
    drop(f.s);
    f.s = Store::open(&path)?;
    assert!(f.s.setup_binding_ready(id(3)).await?);
    assert!(
        !f.s.setup_state()
            .links
            .iter()
            .find(|l| l.device == id(3))
            .unwrap()
            .hello_seen
    );
    assert!(
        !f.s.setup_state()
            .links
            .iter()
            .find(|l| l.device == id(3))
            .unwrap()
            .negotiated
    );
    f.s.setup_legacy_ready(fence.clone()).await?;
    assert!(
        f.s.setup_state()
            .links
            .iter()
            .find(|l| l.device == id(3))
            .unwrap()
            .hello_seen
    );
    f.s.pair(id(3), phone.fingerprint).await?;
    assert!(!f.s.setup_binding_ready(id(3)).await?);
    assert!(f.s.setup_legacy_ready(fence).await.is_err());
    assert!(!f.s.setup_binding_ready(id(3)).await?);
    Ok(())
}

#[tokio::test]
async fn setup_removed_certificate_stays_quiet_before_during_and_after_question() -> Result<()> {
    let f = Fixture::new().await?;
    let pin = member(3).fingerprint;
    assert!(!f.s.setup_quiet_reconnect(pin.clone()).await?);
    let mut removed = member(3);
    removed.status = MemberStatus::Removed;
    removed.hlc = 10;
    f.legacy_authored_removal(removed).await?;
    assert!(f.s.setup_quiet_reconnect(pin.clone()).await?);
    let mut newer = member(3);
    newer.hlc = 20;
    newer.readmits = Some(MemberReadmits {
        hlc: 10,
        writer_device: id(1),
    });
    f.merge(f.snapshot(vec![newer])).await?;
    let key = f.s.setup_state().readmissions[0].key.clone();
    assert!(f.s.setup_quiet_reconnect(pin.clone()).await?);
    f.s.setup_readmit(key, id(2), false, vec![], |_| {}).await?;
    assert!(f.s.setup_quiet_reconnect(pin).await?);
    assert!(!f.s.setup_quiet_reconnect(member(4).fingerprint).await?);
    Ok(())
}

#[tokio::test]
async fn setup_sender_members_rename_updates_direct_binding_without_reconnect() -> Result<()> {
    let f = Fixture::new().await?;
    let mut renamed = member(1);
    renamed.hlc = crate::now_ms() + 100;
    renamed.name = "Office-PC 2".into();
    f.merge(f.snapshot(vec![renamed])).await?;
    assert_eq!(
        f.s.peer_info()
            .await?
            .iter()
            .find(|p| p.id == id(1))
            .unwrap()
            .name,
        "Office-PC 2"
    );
    Ok(())
}

#[tokio::test]
async fn setup_readmits_invalid_vectors_are_atomic_including_sender_self() -> Result<()> {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../../protocol-vectors/readmission.json"))?;
    for invalid in v["invalid_readmits_entries"].as_array().unwrap() {
        for sender_self in [false, true] {
            if sender_self && invalid["also_validate_on_sender_self_row"] != true {
                continue;
            }
            let h = ReadmitVectors::new(&v).await?;
            let subject: Member =
                serde_json::from_value(v["fixtures"]["join"]["payload"]["members"][2].clone())?;
            if sender_self {
                h.pair(subject.clone()).await?;
            }
            let sender = if sender_self { &subject } else { &h.a };
            let before = h.s.setup_state();
            let before_rows = h.all_subject().await?;
            let bookkeeping = |c: &mut rusqlite::Connection| -> Result<(String, i64)> {
                Ok((
                    c.query_row("SELECT last_seen FROM mesh_clock", [], |r| r.get(0))?,
                    c.query_row("SELECT count(*) FROM members_receipts", [], |r| r.get(0))?,
                ))
            };
            let before_bookkeeping = h.s.call(bookkeeping).await?;
            let raw = format!(
                "{{\"transfer_id\":\"{}\",\"setup_id\":\"{}\",\"members\":[{},{}]}}",
                id(70),
                id(0),
                serde_json::to_string(&h.a)?,
                invalid["raw_entry"].as_str().unwrap()
            );
            let mut frame = Vec::new();
            frame.extend_from_slice(&((raw.len() + 1) as u32).to_be_bytes());
            frame.push(0x22);
            frame.extend_from_slice(raw.as_bytes());
            match pneu_proto::decode_frame(&frame) {
                Err(_) => assert_ne!(invalid["name"], "claim-future-hlc"),
                Ok(Message::Members(snapshot)) => {
                    let error =
                        h.s.members_merge_at(
                            h.fence(sender),
                            h.local.clone(),
                            snapshot,
                            vec![],
                            h.now,
                            |_| panic!("invalid claim published"),
                        )
                        .await
                        .unwrap_err();
                    assert!(
                        matches!(
                            error.downcast_ref::<MembersError>(),
                            Some(MembersError::Malformed)
                        ),
                        "{invalid}: {error}"
                    );
                }
                Ok(_) => panic!("expected MEMBERS"),
            }
            assert_eq!(h.s.setup_state(), before, "{invalid}");
            assert_eq!(h.all_subject().await?, before_rows, "{invalid}");
            assert_eq!(
                h.s.call(bookkeeping).await?,
                before_bookkeeping,
                "{invalid}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn setup_readmits_review_withdrawal_validates_final_capacity() -> Result<()> {
    let f = Fixture::new().await?;
    // Fifteen direct-only peers plus the scoped sender fill the binding cap.
    for n in 10..25 {
        f.s.pair(id(n), member(n).fingerprint).await?;
    }
    assert_eq!(f.s.peers().await?.len(), 16);
    let mut removed = member(2);
    removed.status = MemberStatus::Removed;
    removed.hlc = crate::now_ms() + 1;
    f.merge(f.snapshot(vec![removed.clone(), member(3)]))
        .await?;
    let state = f.s.setup_state();
    assert!(state.withdrawn);
    assert!(state.setup_id.is_none());
    let own: serde_json::Value =
        f.s.call(|c| {
            let raw: String = c.query_row(
                "SELECT payload FROM setup_entries WHERE device=? AND fingerprint=?",
                params![id(2).as_str(), member(2).fingerprint.as_str()],
                |r| r.get(0),
            )?;
            Ok(serde_json::from_str(&raw)?)
        })
        .await?;
    assert_eq!(own["removal"], serde_json::to_value(removed)?);
    assert_eq!(own["denied"], true);
    assert!(f.pin(1).await?.is_none());
    assert!(f.pin(3).await?.is_none());
    assert_eq!(f.s.peers().await?.len(), 15);
    for n in 10..25 {
        assert_eq!(f.pin(n).await?, Some(member(n).fingerprint));
    }
    Ok(())
}

#[tokio::test]
async fn setup_readmits_review_replaces_stale_candidate_introducer() -> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(4)])).await?;
    let blocked = vec![(Some(id(3)), member(3).fingerprint)];
    f.s.members_merge(
        f.f.clone(),
        member(2),
        f.snapshot(vec![member(3)]),
        blocked.clone(),
        |_| {},
    )
    .await?;
    let mut removed = member(3);
    removed.status = MemberStatus::Removed;
    removed.hlc = 10;
    f.merge(f.snapshot(vec![removed])).await?;
    let stale: String =
        f.s.call(|c| {
            Ok(c.query_row(
                "SELECT introduced_by FROM setup_candidates WHERE device=?",
                [id(3).as_str()],
                |r| r.get(0),
            )?)
        })
        .await?;
    assert_eq!(stale, id(1).as_str());
    let mut candidate = member(3);
    candidate.hlc = 20;
    candidate.readmits = Some(MemberReadmits {
        hlc: 10,
        writer_device: id(1),
    });
    let mut introducer = f.f.clone();
    introducer.device = id(4);
    introducer.fingerprint = member(4).fingerprint;
    introducer.binding_generation = f.s.pairing_generation(&id(4)).unwrap();
    f.s.members_merge(
        introducer,
        member(2),
        f.snapshot(vec![candidate]),
        blocked,
        |_| {},
    )
    .await?;
    assert!(f.pin(3).await?.is_none());
    f.s.setup_reconcile_candidates(id(2), vec![], |_| {})
        .await?;
    let link =
        f.s.setup_state()
            .links
            .into_iter()
            .find(|l| l.device == id(3))
            .unwrap();
    assert_eq!(link.introduced_by, Some(id(4)));
    assert_eq!(link.introducer_name.as_deref(), Some("Device 4"));
    Ok(())
}

#[tokio::test]
async fn setup_inherited_readmits_follow_guards_and_restart_reconciliation() -> Result<()> {
    for block_by_id in [false, true] {
        let mut f = Fixture::new().await?;
        let mut removed = member(3);
        removed.status = MemberStatus::Removed;
        removed.hlc = 10;
        f.merge(f.snapshot(vec![removed.clone()])).await?;
        let mut candidate = member(3);
        candidate.hlc = 20;
        candidate.readmits = Some(MemberReadmits {
            hlc: 10,
            writer_device: id(1),
        });
        let blocked = if block_by_id {
            vec![(Some(id(3)), member(9).fingerprint)]
        } else {
            vec![(None, member(3).fingerprint)]
        };
        f.s.members_merge(
            f.f.clone(),
            member(2),
            f.snapshot(vec![candidate.clone()]),
            blocked.clone(),
            |delta| {
                assert!(!delta.roster_changed);
                assert!(delta.added.is_empty());
            },
        )
        .await?;
        assert!(f.pin(3).await?.is_none());
        assert!(f.s.setup_state().readmissions.is_empty());
        assert!(f.s.setup_state().roster.contains(&removed));
        // The candidate and its first introducer survive a restart without a
        // prompt or an early deny clear, then install when the attempt ends.
        let path = f._dir.path().join("state.sqlite3");
        drop(f.s);
        f.s = Store::open(&path)?;
        f.s.setup_reconcile_candidates(id(2), blocked, |d| assert!(d.added.is_empty()))
            .await?;
        assert!(f.pin(3).await?.is_none());
        f.s.fail_next("members_before_commit", 1);
        let before = f.s.setup_state();
        assert!(
            f.s.setup_reconcile_candidates(id(2), vec![], |_| panic!("failed follow published"))
                .await
                .is_err()
        );
        assert_eq!(before, f.s.setup_state());
        assert!(f.pin(3).await?.is_none());
        f.s.setup_reconcile_candidates(id(2), vec![], |d| assert_eq!(d.added, vec![id(3)]))
            .await?;
        assert_eq!(f.pin(3).await?, Some(member(3).fingerprint));
        assert!(f.s.setup_state().roster.contains(&candidate));
        let generation = f.s.pairing_generation(&id(3));
        f.s.mark_peer_established(id(3)).await?;
        f.merge(f.snapshot(vec![removed])).await?;
        assert_eq!(generation, f.s.pairing_generation(&id(3)));
        assert!(f.s.peer_established(id(3)).await?);
    }
    Ok(())
}

#[tokio::test]
async fn setup_inherited_follow_rolls_back_storage_and_capacity_failures() -> Result<()> {
    for fault in ["peers", "peer_settings", "capacity", "commit"] {
        let f = Fixture::new().await?;
        let mut removed = member(3);
        removed.status = MemberStatus::Removed;
        removed.hlc = 10;
        f.merge(f.snapshot(vec![removed])).await?;
        let mut candidate = member(3);
        candidate.hlc = 20;
        candidate.readmits = Some(MemberReadmits {
            hlc: 10,
            writer_device: id(1),
        });
        if fault == "capacity" {
            f.merge(f.snapshot((10..25).map(member).collect())).await?;
        } else if fault == "commit" {
            f.s.fail_next("members_before_commit", 1);
        } else {
            let sql = format!(
                "CREATE TEMP TRIGGER follow_fail BEFORE INSERT ON {fault} BEGIN SELECT RAISE(ABORT,'follow failed'); END;"
            );
            f.s.call(move |c| {
                c.execute_batch(&sql)?;
                Ok(())
            })
            .await?;
        }
        let before = f.s.setup_state();
        let generation = f.s.pairing_generation(&id(3));
        assert!(
            f.s.members_merge(
                f.f.clone(),
                member(2),
                f.snapshot(vec![candidate]),
                vec![],
                |_| panic!("failed follow published")
            )
            .await
            .is_err(),
            "{fault}"
        );
        assert_eq!(f.s.setup_state(), before, "{fault}");
        assert_eq!(f.s.pairing_generation(&id(3)), generation, "{fault}");
        assert!(f.pin(3).await?.is_none(), "{fault}");
        assert!(
            f.s.setup_quiet_reconnect(member(3).fingerprint).await?,
            "{fault}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn setup_claim_eligibility_is_independent_of_member_stamp_and_own_metadata() -> Result<()> {
    for authored in [false, true] {
        let f = Fixture::new().await?;
        let mut removed = member(3);
        removed.status = MemberStatus::Removed;
        removed.hlc = 10;
        if authored {
            f.legacy_authored_removal(removed).await?;
        } else {
            f.merge(f.snapshot(vec![removed])).await?;
        }
        let mut old_member = member(3);
        old_member.readmits = Some(MemberReadmits {
            hlc: 10,
            writer_device: id(1),
        });
        f.merge(f.snapshot(vec![old_member.clone()])).await?;
        if authored {
            let key = f.s.setup_state().readmissions[0].key.clone();
            f.s.setup_readmit(key, id(2), true, vec![], |_| {}).await?;
        }
        assert_eq!(f.pin(3).await?, Some(member(3).fingerprint));
        assert!(f.s.setup_state().roster.contains(&old_member));
    }
    // A third-party claim for this installation may merge without accepting
    // the round-trip row as an identity, name or membership-register update.
    let f = Fixture::new().await?;
    let own =
        f.s.setup_state()
            .roster
            .into_iter()
            .find(|m| m.device_id == id(2))
            .unwrap();
    let mut forwarded = own.clone();
    forwarded.name = "Do not rename me".into();
    forwarded.hlc += 10;
    forwarded.readmits = Some(MemberReadmits {
        hlc: 10,
        writer_device: id(1),
    });
    f.merge(f.snapshot(vec![forwarded])).await?;
    let mut expected = own;
    expected.readmits = Some(MemberReadmits {
        hlc: 10,
        writer_device: id(1),
    });
    assert!(f.s.setup_state().roster.contains(&expected));
    let mut removed = member(2);
    removed.status = MemberStatus::Removed;
    removed.hlc = 10;
    f.merge(f.snapshot(vec![removed])).await?;
    assert!(!f.s.setup_state().withdrawn);
    assert!(f.s.setup_state().roster.contains(&expected));
    Ok(())
}

#[tokio::test]
async fn setup_inherited_candidate_freezes_joined_notice_before_deferred_rename() -> Result<()> {
    let f = Fixture::new().await?;
    let mut removed = member(3);
    removed.status = MemberStatus::Removed;
    removed.hlc = 10;
    f.merge(f.snapshot(vec![removed])).await?;
    let mut candidate = member(3);
    candidate.hlc = 20;
    candidate.readmits = Some(MemberReadmits {
        hlc: 10,
        writer_device: id(1),
    });
    let blocked = vec![(Some(id(3)), candidate.fingerprint.clone())];
    f.s.members_merge(
        f.f.clone(),
        member(2),
        f.snapshot(vec![candidate.clone()]),
        blocked.clone(),
        |_| {},
    )
    .await?;
    // Its original admission predates this receiver's assignment. A rename
    // beyond that baseline cannot turn an initial import into a joined notice.
    candidate.hlc = crate::now_ms() + 10;
    candidate.name = "Renamed".into();
    f.s.members_merge(
        f.f.clone(),
        member(2),
        f.snapshot(vec![candidate]),
        blocked,
        |_| {},
    )
    .await?;
    f.s.setup_reconcile_candidates(id(2), vec![], |d| assert_eq!(d.added, vec![id(3)]))
        .await?;
    let count: i64 =
        f.s.call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM setup_notices WHERE device=?",
                [id(3).as_str()],
                |r| r.get(0),
            )?)
        })
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn setup_authored_origin_is_committed_atomically() -> Result<()> {
    let f = Fixture::new().await?;
    f.merge(f.snapshot(vec![member(3)])).await?;
    let before = f.s.setup_state();
    f.s.fail_next("members_before_commit", 1);
    assert!(
        f.s.setup_remove(id(2), id(3), |_| panic!("failed removal published"))
            .await
            .is_err()
    );
    assert_eq!(f.s.setup_state(), before);
    assert!(f.pin(3).await?.is_some());
    f.s.setup_remove(id(2), id(3), |_| {}).await?;
    let origin:(bool,bool)=f.s.call(|c|Ok(c.query_row("SELECT json_extract(payload,'$.authored'),json_extract(payload,'$.revoked_binding') FROM setup_entries WHERE device=?",[id(3).as_str()],|r|Ok((r.get(0)?,r.get(1)?)))?)).await?;
    assert_eq!(origin, (true, true));
    Ok(())
}
