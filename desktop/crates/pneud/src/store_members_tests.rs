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
    let before = f.s.setup_state();
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
        let old = f.s.setup_state();
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
async fn setup_legacy_roster_link_keeps_loss_handling_and_survives_withdrawal() -> Result<()> {
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
        assert!(!f.s.setup_protected(&id(3)));
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
