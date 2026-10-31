use super::*;
use std::path::Path;
struct NoActions;
#[async_trait::async_trait]
impl PcActions for NoActions {
    async fn clipboard_image(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn clipboard_text(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn notify(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn open(&self, _: &Path) -> Result<()> {
        Ok(())
    }
}
struct Running {
    core: Arc<Core>,
    server: Option<tokio::task::JoinHandle<Result<()>>>,
    _dir: tempfile::TempDir,
}
impl Running {
    async fn new() -> Result<Self> {
        let dir = crate::test_support::scratch()?;
        let listener = TcpListener::bind("0.0.0.0:0").await?;
        let paths = Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("pneud.sock"),
        };
        let config = Config {
            port: listener.local_addr()?.port(),
            save_path: dir.path().join("saved"),
            clipboard_mode: crate::config::ClipboardMode::Off,
            clipboard_sync_to_phone: false,
            ..Config::default()
        };
        let core = Core::new(paths, config, Arc::new(NoActions)).await?;
        let server = tokio::spawn(core.clone().serve(listener, false));
        Ok(Self {
            core,
            server: Some(server),
            _dir: dir,
        })
    }
    async fn stop(mut self) -> Result<()> {
        self.core.request_shutdown();
        self.server.take().unwrap().await??;
        Ok(())
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.core.request_shutdown();
    }
}
async fn until(mut check: impl FnMut() -> bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
async fn pair(a: &Core, b: &Core, scope: bool) -> Result<()> {
    let uri = a
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", a.bound_port())])
        .await?;
    a.set_pairing_scope(scope).await?;
    let preview = b.join(&uri.build()?).await?;
    let attempt = preview["attempt"]
        .as_str()
        .context("expected join preview")?;
    b.confirm_join_scoped(attempt, scope).await?;
    until(|| b.join_status(attempt).is_ok_and(|v| v["state"] == "paired")).await?;
    until(|| {
        a.paired_devices().contains(&b.identity.device_id)
            && b.paired_devices().contains(&a.identity.device_id)
    })
    .await
}

#[tokio::test]
async fn setup_remote_scope_status_drives_existing_window_states() -> Result<()> {
    use crate::store::members::SetupFence;
    use crate::ui::window_setup::{Remembered, Row, Setup, Top};
    let a = Running::new().await?;
    let device = Id::from_bytes([3; 16]);
    let pin = Hash::new(format!("{:064x}", 3))?;
    a.core.store.pair(device.clone(), pin.clone()).await?;
    let status = a.core.setup_status().await?;
    assert!(status["links"][0].get("remote_scope").is_none());

    let mut hello = a.core.computer_hello();
    hello.device_id = device.clone();
    hello.name = "Other computer".into();
    hello.setup = Some(SetupHello {
        scope: SetupScope::Disabled,
        setup_id: None,
    });
    a.core
        .store
        .pair_then_hello_scoped(
            device.clone(),
            pin.clone(),
            Some(hello),
            Some(a.core.self_member()),
            SetupScope::Enabled,
            false,
            || {},
        )
        .await?;
    // Recovery preserves the previous local Ask decision; opt in locally.
    a.core
        .store
        .setup_link(device.clone(), SetupScope::Enabled)
        .await?;
    let status = a.core.setup_status().await?;
    assert_eq!(status["links"][0]["remote_scope"], "disabled");

    let mut snapshot = crate::ui::Snapshot::default();
    snapshot.devices.push(mesh::DeviceSnapshot {
        device: device.clone(),
        name: "Other computer".into(),
        role: "computer".into(),
        state: "connected".into(),
        last_seen: 0,
        dialer: None,
        route: None,
    });
    let peers = crate::ui::window::peers(&snapshot, &[]);
    let remembered = Remembered {
        offer: Some(crate::ui::window_setup::LINKED.into()),
        ..Remembered::default()
    };
    let current = Arc::new(AtomicU64::new(1));
    for (scope, text, row) in [
        (SetupScope::Disabled, "disabled", Row::Theirs),
        (SetupScope::Ask, "ask", Row::Waiting),
        (SetupScope::Enabled, "enabled", Row::None),
    ] {
        let fence = SetupFence {
            generation: a.core.store.setup_state().generation,
            binding_generation: a.core.store.pairing_generation(&device).unwrap(),
            device: device.clone(),
            fingerprint: pin.clone(),
            connection: 1,
            current: current.clone(),
        };
        let remote = Member {
            device_id: device.clone(),
            fingerprint: pin.clone(),
            role: DeviceRole::Computer,
            name: "Other computer".into(),
            status: MemberStatus::Member,
            hlc: 0,
            writer_device: device.clone(),
            reach: None,
            readmits: None,
        };
        a.core
            .store
            .setup_activate(
                fence,
                a.core.self_member(),
                remote,
                SetupHello {
                    scope,
                    setup_id: (scope == SetupScope::Enabled).then(|| Id::from_bytes([4; 16])),
                },
                |_| {},
            )
            .await?;
        let status = a.core.setup_status().await?;
        assert_eq!(status["links"][0]["remote_scope"], text);
        assert_eq!(status["links"][0]["scope"], "enabled");
        let setup = Setup::parse(&serde_json::json!({"setup": status}));
        assert_eq!(crate::ui::window_setup::row(&setup, &peers[0]), row);
        assert_eq!(
            setup.unanswered().len(),
            usize::from(scope == SetupScope::Ask)
        );
        let top = crate::ui::window_setup::top(&setup, &peers, &remembered);
        assert_eq!(matches!(top, Top::Waiting { .. }), scope == SetupScope::Ask);
    }
    assert_eq!(
        crate::ui::window_setup::row_text(Row::Theirs, "Other computer").1,
        "Link on Other computer too"
    );
    let fence = SetupFence {
        generation: a.core.store.setup_state().generation,
        binding_generation: a.core.store.pairing_generation(&device).unwrap(),
        device,
        fingerprint: pin,
        connection: 1,
        current,
    };
    a.core.store.setup_legacy_ready(fence).await?;
    assert!(
        a.core.setup_status().await?["links"][0]
            .get("remote_scope")
            .is_none()
    );
    a.stop().await
}

#[tokio::test]
async fn setup_three_daemons_join_through_member_and_remove_from_all() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    let c = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    until(|| {
        a.core.store.setup_state().setup_id.is_some()
            && a.core.store.setup_state().setup_id == b.core.store.setup_state().setup_id
    })
    .await?;
    pair(&a.core, &c.core, true).await?;
    until(|| {
        [&a.core, &b.core, &c.core].iter().all(|core| {
            core.paired_devices().len() == 2
                && core.connected_devices().len() == 2
                && core.store.setup_state().roster.len() == 3
        })
    })
    .await?;
    assert_eq!(
        b.core
            .store
            .setup_state()
            .links
            .iter()
            .find(|l| l.device == c.core.identity.device_id)
            .unwrap()
            .introduced_by,
        Some(a.core.identity.device_id.clone())
    );
    let state = b.core.setup_status().await?;
    let notice = state["notices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["device"] == c.core.identity.device_id.as_str())
        .unwrap();
    assert_eq!(
        notice["binding_generation"],
        b.core
            .store
            .pairing_generation(&c.core.identity.device_id)
            .unwrap()
    );
    let fixture = a._dir.path().join("setup-content.bin");
    std::fs::write(&fixture, b"direct only")?;
    a.core
        .send_files(
            vec![fixture],
            crate::outbox::Targets::Devices(vec![b.core.identity.device_id.clone()]),
        )
        .await?;
    let sender = a.core.identity.device_id.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let id = sender.clone();
            let count = b
                .core
                .store
                .call(move |db| {
                    Ok(db.query_row(
                        "SELECT count(*) FROM receipts WHERE device=?",
                        [id.as_str()],
                        |r| r.get::<_, i64>(0),
                    )?)
                })
                .await?;
            if count == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert_eq!(
        c.core
            .store
            .call(|db| Ok(
                db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, i64>(0))?
            ))
            .await?,
        0
    );
    assert_eq!(
        b.core
            .store
            .call(|db| Ok(
                db.query_row("SELECT count(*) FROM outbox_artifacts", [], |r| r
                    .get::<_, i64>(0))?
            ))
            .await?,
        0
    );
    b.core
        .remove_setup_device(c.core.identity.device_id.clone())
        .await?;
    until(|| {
        !a.core.paired_devices().contains(&c.core.identity.device_id)
            && !b.core.paired_devices().contains(&c.core.identity.device_id)
    })
    .await?;
    assert!(a.core.paired_devices().contains(&b.core.identity.device_id));
    a.stop().await?;
    b.stop().await?;
    c.stop().await
}
#[tokio::test]
async fn setup_three_device_migration_ask_prompts_allow_without_resets() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    let c = Running::new().await?;
    pair(&a.core, &b.core, false).await?;
    pair(&a.core, &c.core, false).await?;
    pair(&b.core, &c.core, false).await?;
    until(|| {
        let sessions = b.core.sessions.lock().unwrap();
        sessions
            .sessions
            .get(&c.core.identity.device_id)
            .is_some_and(|session| {
                session.close.borrow().is_none()
                    && b.core
                        .settings_ready
                        .lock()
                        .unwrap()
                        .get(&c.core.identity.device_id)
                        == Some(&session.id)
            })
    })
    .await?;
    for left in [&a.core, &b.core, &c.core] {
        for right in [&a.core, &b.core, &c.core] {
            if left.identity.device_id != right.identity.device_id {
                left.store
                    .setup_link(right.identity.device_id.clone(), SetupScope::Ask)
                    .await?;
            }
        }
    }
    let generations = [
        a.core.store.pairing_generation(&b.core.identity.device_id),
        b.core.store.pairing_generation(&c.core.identity.device_id),
        c.core.store.pairing_generation(&a.core.identity.device_id),
    ];
    let key = format!(
        "{}/{}/clipboard",
        b.core.identity.device_id, c.core.identity.device_id
    );
    b.core
        .store
        .settings_edit(
            b.core.identity.device_id.clone(),
            c.core.identity.device_id.clone(),
            false,
            key,
            false,
        )
        .await?;
    a.core
        .link_existing_device(b.core.identity.device_id.clone(), None)
        .await?;
    until(|| a.core.store.setup_state().setup_id.is_some()).await?;
    for target in [&b.core, &c.core] {
        until(|| target.store.setup_state().links.iter().any(|l| l.prompt)).await?;
        let state = target.store.setup_state();
        let prompt = state.links.iter().find(|l| l.prompt).unwrap();
        let notice = target
            .setup_request_notice()
            .await
            .context("link request notification")?;
        assert_eq!(notice.body, "New devices then pair just once.");
        assert!(
            notice
                .setup_requests
                .iter()
                .any(|r| r.device == prompt.device
                    && r.setup_generation == state.generation
                    && Some(r.binding_generation)
                        == target.store.pairing_generation(&prompt.device))
        );
        target
            .decide_setup(
                prompt.device.clone(),
                state.generation,
                target.store.pairing_generation(&prompt.device).unwrap(),
                true,
            )
            .await?;
    }
    until(|| {
        let id = a.core.store.setup_state().setup_id;
        [&a.core, &b.core, &c.core].iter().all(|core| {
            let s = core.store.setup_state();
            s.setup_id == id
                && s.links
                    .iter()
                    .all(|l| l.scope == SetupScope::Enabled && !l.prompt)
                && s.roster.len() == 3
        })
    })
    .await?;
    assert_eq!(
        generations,
        [
            a.core.store.pairing_generation(&b.core.identity.device_id),
            b.core.store.pairing_generation(&c.core.identity.device_id),
            c.core.store.pairing_generation(&a.core.identity.device_id)
        ]
    );
    assert!(
        b.core
            .store
            .settings_fields(
                b.core.identity.device_id.clone(),
                c.core.identity.device_id.clone(),
                false
            )
            .await?
            .iter()
            .any(|f| !f.enabled)
    );
    a.stop().await?;
    b.stop().await?;
    c.stop().await
}
#[tokio::test]
async fn setup_independent_bootstraps_stay_direct_without_merging() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    let c = Running::new().await?;
    let d = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    pair(&c.core, &d.core, true).await?;
    until(|| {
        a.core.store.setup_state().setup_id.is_some()
            && c.core.store.setup_state().setup_id.is_some()
    })
    .await?;
    let s = a.core.store.setup_state().setup_id;
    let t = c.core.store.setup_state().setup_id;
    assert_ne!(s, t);
    pair(&a.core, &c.core, true).await?;
    until(|| {
        a.core.store.setup_state().links.iter().any(|l| {
            l.device == c.core.identity.device_id
                && l.outcome.as_deref() == Some("Direct link kept; separate setups not linked")
        })
    })
    .await?;
    assert_eq!(a.core.store.setup_state().setup_id, s);
    assert_eq!(c.core.store.setup_state().setup_id, t);
    assert_eq!(b.core.paired_devices().len(), 1);
    assert_eq!(d.core.paired_devices().len(), 1);
    a.stop().await?;
    b.stop().await?;
    c.stop().await?;
    d.stop().await
}
#[tokio::test]
async fn setup_switch_off_on_either_endpoint_keeps_plain_direct_link() -> Result<()> {
    for inviter_on in [false, true] {
        let a = Running::new().await?;
        let b = Running::new().await?;
        let uri = a
            .core
            .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", a.core.bound_port())])
            .await?;
        a.core.set_pairing_scope(inviter_on).await?;
        let preview = b.core.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        b.core.confirm_join_scoped(attempt, !inviter_on).await?;
        until(|| {
            b.core
                .join_status(attempt)
                .is_ok_and(|v| v["state"] == "paired")
        })
        .await?;
        assert!(a.core.store.setup_state().setup_id.is_none());
        assert!(b.core.store.setup_state().setup_id.is_none());
        assert!(!a.core.store.setup_scoped(&b.core.identity.device_id));
        assert!(!b.core.store.setup_scoped(&a.core.identity.device_id));
        assert_eq!(
            a.core.store.setup_state().links[0].scope,
            if inviter_on {
                SetupScope::Enabled
            } else {
                SetupScope::Disabled
            }
        );
        a.stop().await?;
        b.stop().await?;
    }
    Ok(())
}
#[tokio::test]
async fn setup_legacy_peer_does_not_found_or_exchange_members() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    let uri = a
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", a.core.bound_port())])
        .await?;
    let mut legacy = b.core.computer_hello();
    legacy.capabilities.retain(|c| c != "setup");
    legacy.setup = None;
    a.core
        .finish_pairing_hello(
            &b.core.identity.device_id,
            &b.core.identity.fingerprint,
            &uri.token,
            Some(legacy),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert!(a.core.store.setup_state().setup_id.is_none());
    assert!(!a.core.store.setup_scoped(&b.core.identity.device_id));
    assert_eq!(
        a.core
            .store
            .call(|c| Ok(
                c.query_row("SELECT count(*) FROM members_outgoing", [], |r| r
                    .get::<_, i64>(0))?
            ))
            .await?,
        0
    );
    a.stop().await?;
    b.stop().await
}

#[tokio::test]
async fn setup_regression_review_already_legacy_ready_records_hello() -> Result<()> {
    let a = Running::new().await?;
    for role in [DeviceRole::Computer, DeviceRole::Phone] {
        let device = Id::from_bytes([if role == DeviceRole::Phone { 3 } else { 4 }; 16]);
        let pin = Hash::new(crate::random_hex(32))?;
        a.core.store.pair(device.clone(), pin.clone()).await?;
        a.core
            .admission
            .lock()
            .unwrap()
            .peers
            .insert(pin, device.clone());
        let scope = ActionScope::new();
        scope.registration_generation.store(7, Ordering::SeqCst);
        a.core
            .action_scope
            .lock()
            .unwrap()
            .insert(device.clone(), scope);
        let (close, _receiver) = watch::channel(None);
        a.core.sessions.lock().unwrap().sessions.insert(
            device.clone(),
            Session {
                outbound: false,
                mesh: false,
                role: if role == DeviceRole::Phone {
                    "phone"
                } else {
                    "computer"
                }
                .into(),
                device: device.clone(),
                id: 7,
                close,
                wake: Arc::new(Notify::new()),
                paused: false,
                capabilities: vec![],
                phone: crate::ui::Phone::default(),
            },
        );
        let mut store = a.core.store.with_current_pairing();
        store.connection_id = Some(7);
        let mut hello = a.core.computer_hello();
        hello.device_id = device.clone();
        hello.role = Some(role);
        hello.capabilities.clear();
        hello.setup = None;
        // Matches a restarted legacy binding: no current HELLO and no setup
        // negotiation. A current authenticated READY must still be recorded.
        let link = || {
            a.core
                .store
                .setup_state()
                .links
                .into_iter()
                .find(|l| l.device == device)
                .unwrap()
        };
        assert!(!link().negotiated);
        assert!(!link().hello_seen);
        store.connection_id = Some(6);
        assert!(a.core.setup_ready(&store, &hello, None).await?.is_none());
        assert!(
            !link().hello_seen,
            "obsolete session must not supply evidence"
        );
        store.connection_id = Some(7);
        assert!(a.core.setup_ready(&store, &hello, None).await?.is_none());
        assert!(
            link().hello_seen,
            "already-legacy connections must record HELLO"
        );
        assert!(!link().negotiated);
        assert!(!link().participating);
        assert_eq!(link().scope, SetupScope::Ask);
        assert!(a.core.setup_ready(&store, &hello, None).await?.is_none());
        assert!(link().hello_seen);
    }
    assert!(a.core.store.setup_state().setup_id.is_none());
    a.stop().await
}
#[tokio::test]
async fn setup_partial_knowledge_refusals_and_established_unauthorized_retain_trust() -> Result<()>
{
    let a = Running::new().await?;
    let b = Running::new().await?;
    let uri = a
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", a.core.bound_port())])
        .await?;
    let mut hello = b.core.computer_hello();
    hello.addresses = Some(vec![format!("127.0.0.1:{}", b.core.bound_port())]);
    a.core
        .finish_pairing_hello(
            &b.core.identity.device_id,
            &b.core.identity.fingerprint,
            &uri.token,
            Some(hello),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    until(|| {
        a.core.link_problem(&b.core.identity.device_id) == Some(mesh::LinkProblem::Unauthorized)
    })
    .await?;
    assert!(a.core.paired_devices().contains(&b.core.identity.device_id));
    assert!(
        !a.core
            .store
            .peer_established(b.core.identity.device_id.clone())
            .await?
    );
    let uri = b
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", b.core.bound_port())])
        .await?;
    let mut hello = a.core.computer_hello();
    hello.addresses = Some(vec![format!("127.0.0.1:{}", a.core.bound_port())]);
    b.core
        .finish_pairing_hello(
            &a.core.identity.device_id,
            &a.core.identity.fingerprint,
            &uri.token,
            Some(hello),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    until(|| {
        a.core
            .connected_devices()
            .contains(&b.core.identity.device_id)
            && b.core
                .connected_devices()
                .contains(&a.core.identity.device_id)
            && a.core.store.setup_state().setup_id.is_some()
            && a.core.store.setup_state().setup_id == b.core.store.setup_state().setup_id
    })
    .await?;
    let generation = a.core.store.pairing_generation(&b.core.identity.device_id);
    let mut events = a.core.subscribe();
    b.core
        .sessions
        .lock()
        .unwrap()
        .sessions
        .get(&a.core.identity.device_id)
        .unwrap()
        .close
        .send_replace(Some(CloseReason::Error(ErrorCode::Unauthorized)));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Event::ConnectionChanged {
                device,
                connected: false,
            }) = events.recv().await
                && device == b.core.identity.device_id
            {
                break;
            }
        }
    })
    .await?;
    assert_eq!(
        a.core.store.pairing_generation(&b.core.identity.device_id),
        generation
    );
    assert!(a.core.paired_devices().contains(&b.core.identity.device_id));
    a.stop().await?;
    b.stop().await
}
#[tokio::test]
async fn setup_incoming_attempt_freezes_scope_and_old_cancel_does_not_poison_new_token()
-> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    let uri = a
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", a.core.bound_port())])
        .await?;
    a.core.set_pairing_scope(false).await?;
    let _old = a
        .core
        .track_setup_incoming(100, b.core.identity.fingerprint.clone());
    let target = a
        .core
        .setup_incoming
        .lock()
        .unwrap()
        .get(&100)
        .unwrap()
        .clone();
    a.core.set_pairing_scope(true).await?;
    assert!(!target.scope);
    target.cancelled.store(true, Ordering::SeqCst);
    assert!(matches!(
        a.core
            .finish_pairing_hello_target(
                &b.core.identity.device_id,
                &b.core.identity.fingerprint,
                &uri.token,
                Some(b.core.computer_hello()),
                Some(target)
            )
            .await,
        Err(PairCode::InvalidToken)
    ));
    let _fresh = a
        .core
        .track_setup_incoming(101, b.core.identity.fingerprint.clone());
    let fresh = a
        .core
        .setup_incoming
        .lock()
        .unwrap()
        .get(&101)
        .unwrap()
        .clone();
    a.core
        .finish_pairing_hello_target(
            &b.core.identity.device_id,
            &b.core.identity.fingerprint,
            &uri.token,
            Some(b.core.computer_hello()),
            Some(fresh),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert!(a.core.paired_devices().contains(&b.core.identity.device_id));
    a.stop().await?;
    b.stop().await
}
#[tokio::test]
async fn setup_withdrawal_cancels_unrelated_preexisting_preview_without_reciprocal_removal()
-> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    let c = Running::new().await?;
    let d = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    until(|| {
        a.core.store.setup_state().setup_id.is_some()
            && a.core.store.setup_state().setup_id == b.core.store.setup_state().setup_id
            && b.core
                .connected_devices()
                .contains(&a.core.identity.device_id)
    })
    .await?;
    pair(&b.core, &c.core, false).await?;
    until(|| {
        let sessions = b.core.sessions.lock().unwrap();
        sessions
            .sessions
            .get(&c.core.identity.device_id)
            .is_some_and(|session| {
                session.close.borrow().is_none()
                    && b.core
                        .settings_ready
                        .lock()
                        .unwrap()
                        .get(&c.core.identity.device_id)
                        == Some(&session.id)
            })
    })
    .await?;
    let unscoped = b
        .core
        .sessions
        .lock()
        .unwrap()
        .sessions
        .get(&c.core.identity.device_id)
        .unwrap()
        .id;
    let invitation = d
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", d.core.bound_port())])
        .await?;
    let preview = b.core.join(&invitation.build()?).await?;
    let attempt = preview["attempt"].as_str().unwrap();
    let mut captured = b.core.store.with_current_pairing();
    captured.connection_id = Some(
        b.core
            .sessions
            .lock()
            .unwrap()
            .sessions
            .get(&a.core.identity.device_id)
            .unwrap()
            .id,
    );
    captured.setup_generation = Some(b.core.store.setup_state().generation);
    let _incoming = b
        .core
        .track_setup_incoming(200, d.core.identity.fingerprint.clone());
    let pending_inbound = b
        .core
        .setup_incoming
        .lock()
        .unwrap()
        .get(&200)
        .unwrap()
        .clone();
    b.core
        .withdraw_setup(&captured, &a.core.computer_hello())
        .await?;
    assert_eq!(b.core.join_status(attempt)?["state"], "cancelled");
    assert!(pending_inbound.cancelled.load(Ordering::SeqCst));
    assert!(b.core.store.setup_state().withdrawn);
    assert_eq!(
        b.core.paired_devices(),
        vec![c.core.identity.device_id.clone()]
    );
    {
        let sessions = b.core.sessions.lock().unwrap();
        let session = sessions.sessions.get(&c.core.identity.device_id).unwrap();
        assert_eq!(session.id, unscoped);
        assert!(session.close.borrow().is_none());
    }
    assert!(a.core.paired_devices().contains(&b.core.identity.device_id));
    a.stop().await?;
    b.stop().await?;
    c.stop().await?;
    d.stop().await
}
#[tokio::test]
async fn setup_tombstone_cancels_matching_preview_but_preserves_other_pin_attempt() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    let c = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    until(|| {
        b.core.store.setup_state().setup_id.is_some()
            && b.core.store.setup_state().setup_id == a.core.store.setup_state().setup_id
            && b.core
                .connected_devices()
                .contains(&a.core.identity.device_id)
    })
    .await?;
    let uri = c
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", c.core.bound_port())])
        .await?;
    let preview = b.core.join(&uri.build()?).await?;
    let attempt = preview["attempt"].as_str().unwrap();
    let mut captured = b.core.store.with_current_pairing();
    captured.connection_id = Some(
        b.core
            .sessions
            .lock()
            .unwrap()
            .sessions
            .get(&a.core.identity.device_id)
            .unwrap()
            .id,
    );
    captured.setup_generation = Some(b.core.store.setup_state().generation);
    let fence = b.core.setup_fence(&captured, &a.core.computer_hello())?;
    let mut roster = b.core.store.setup_state().roster;
    let removed = Member {
        device_id: c.core.identity.device_id.clone(),
        fingerprint: c.core.identity.fingerprint.clone(),
        role: DeviceRole::Computer,
        name: c.core.config().name,
        status: MemberStatus::Removed,
        hlc: crate::now_ms(),
        writer_device: a.core.identity.device_id.clone(),
        reach: None,
        readmits: None,
    };
    roster.push(removed.clone());
    let message = Members {
        transfer_id: Id::from_bytes(rand::random()),
        setup_id: b.core.store.setup_state().setup_id.unwrap(),
        members: roster,
    };
    assert!(matches!(
        b.core
            .receive_members(&captured, &a.core.computer_hello(), Some(&fence), message)
            .await?,
        Message::Ack(Ack {
            deduplicated: false,
            ..
        })
    ));
    assert_eq!(b.core.join_status(attempt)?["state"], "cancelled");
    assert!(!b.core.paired_devices().contains(&c.core.identity.device_id));
    // A new local attempt after removal captures the current barrier. Replaying
    // retained removal history must not cancel fresh consent to the same pin.
    let fresh = b.core.join(&uri.build()?).await?;
    let fresh = fresh["attempt"].as_str().unwrap();
    b.core.reconcile_setup_candidates().await?;
    assert_eq!(b.core.join_status(fresh)?["state"], "confirmation");
    b.core.cancel_join(fresh).await?;
    // A new preview to the same ID with a different pin is protected from an old-key removal.
    let mut replacement = uri;
    replacement.fingerprint = Hash::new("d".repeat(64))?;
    let next = b.core.join(&replacement.build()?).await?;
    let next = next["attempt"].as_str().unwrap();
    b.core
        .mesh
        .cancel_removed(&[(removed.device_id, removed.fingerprint)]);
    assert_eq!(b.core.join_status(next)?["state"], "confirmation");
    a.stop().await?;
    b.stop().await?;
    c.stop().await
}
#[tokio::test]
async fn setup_own_tombstone_flushes_durable_ack_before_closing_transport() -> Result<()> {
    let mut a = Running::new().await?;
    let b = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    until(|| {
        a.core.store.setup_state().setup_id.is_some()
            && a.core.store.setup_state().setup_id == b.core.store.setup_state().setup_id
    })
    .await?;
    a.core.request_shutdown();
    a.server.take().unwrap().await??;
    until(|| b.core.connected_devices().is_empty()).await?;
    let config = crate::tls::client_config(&a.core.identity, b.core.identity.fingerprint.clone())?;
    let tcp = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", b.core.bound_port())).await?;
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
        .await?;
    write_frame(
        &mut tls,
        &Message::Hello(a.core.computer_hello_for(
            &b.core.identity.fingerprint,
            Some(&b.core.identity.device_id),
            true,
        )),
    )
    .await?;
    assert!(matches!(
        read_frame(&mut tls).await?,
        Some(Message::Hello(_))
    ));
    let state = b.core.store.setup_state();
    let mut roster = state.roster;
    let own = roster
        .iter_mut()
        .find(|m| m.device_id == b.core.identity.device_id)
        .unwrap();
    own.status = MemberStatus::Removed;
    own.reach = None;
    own.hlc = crate::now_ms() + 1;
    own.writer_device = a.core.identity.device_id.clone();
    let id = Id::from_bytes(rand::random());
    write_frame(
        &mut tls,
        &Message::Members(Members {
            transfer_id: id.clone(),
            setup_id: state.setup_id.unwrap(),
            members: roster,
        }),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match read_frame(&mut tls)
                .await?
                .context("withdrawal closed before ACK")?
            {
                Message::Ack(ack) if ack.transfer_id == id => {
                    assert!(!ack.deduplicated);
                    break;
                }
                Message::PeerSettings(snapshot) => {
                    write_frame(
                        &mut tls,
                        &Message::Ack(Ack {
                            transfer_id: snapshot.transfer_id,
                            deduplicated: false,
                        }),
                    )
                    .await?
                }
                Message::Members(snapshot) => {
                    write_frame(
                        &mut tls,
                        &Message::Ack(Ack {
                            transfer_id: snapshot.transfer_id,
                            deduplicated: false,
                        }),
                    )
                    .await?
                }
                Message::Ping(p) => write_frame(&mut tls, &Message::Pong(p)).await?,
                other => bail!("unexpected frame before withdrawal receipt: {other:?}"),
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(matches!(
        read_frame(&mut tls).await?,
        Some(Message::Error(ErrorMessage {
            code: ErrorCode::Closing,
            ..
        }))
    ));
    assert!(b.core.store.setup_state().withdrawn);
    assert!(b.core.paired_devices().is_empty());
    drop(tls);
    b.stop().await
}

#[tokio::test]
async fn setup_plain_unpair_confirmation_cannot_become_global_removal() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    until(|| {
        a.core.store.setup_state().setup_id.is_some()
            && a.core.store.setup_state().roster.len() == 2
    })
    .await?;
    let before = a.core.store.setup_state();
    assert!(
        a.core
            .unpair_device(b.core.identity.device_id.clone())
            .await
            .is_err()
    );
    assert_eq!(a.core.store.setup_state(), before);
    assert!(a.core.paired_devices().contains(&b.core.identity.device_id));
    assert!(
        a.core
            .remove_setup_device_confirmed(
                b.core.identity.device_id.clone(),
                before.generation + 1,
                a.core.store.pairing_generation(&b.core.identity.device_id),
                b.core.identity.fingerprint.clone()
            )
            .await
            .is_err()
    );
    a.stop().await?;
    b.stop().await
}

#[tokio::test]
async fn setup_advisory_rename_failure_between_writes_repairs_config_on_restart() -> Result<()> {
    for before_commit in [true, false] {
        let mut a = Running::new().await?;
        let mut b = Running::new().await?;
        pair(&a.core, &b.core, true).await?;
        until(|| {
            a.core.store.setup_state().roster.len() == 2
                && a.core.store.setup_state().setup_id == b.core.store.setup_state().setup_id
        })
        .await?;
        a.core.request_shutdown();
        b.core.request_shutdown();
        a.server.take().unwrap().await??;
        b.server.take().unwrap().await??;
        a.core.store.drain().await;
        b.core.store.drain().await;
        a.core.config().save(&a.core.paths.config)?;
        let old = a.core.config().name;
        a.core.store.fail_next(
            if before_commit {
                "members_before_commit"
            } else {
                "setup_config_after_commit"
            },
            1,
        );
        assert!(a.core.rename("Recovered name".into()).await.is_err());
        assert_eq!(Config::load(&a.core.paths.config)?.name, old);
        let expected = if before_commit {
            old.as_str()
        } else {
            "Recovered name"
        };
        assert!(
            a.core
                .store
                .setup_state()
                .roster
                .iter()
                .any(|m| m.device_id == a.core.identity.device_id && m.name == expected)
        );
        let paths = a.core.paths.clone();
        let config = Config::load(&paths.config)?;
        let retained_dir = std::mem::replace(&mut a._dir, crate::test_support::scratch()?);
        drop(a);
        let restarted = Core::new(paths, config, Arc::new(NoActions)).await?;
        assert_eq!(restarted.config().name, expected);
        assert_eq!(Config::load(&restarted.paths.config)?.name, expected);
        restarted.shutdown_actions().await?;
        drop(restarted);
        drop(retained_dir);
    }
    Ok(())
}

#[tokio::test]
async fn setup_advisory_remove_bindingless_member_fences_absence_and_cancels_preview() -> Result<()>
{
    let a = Running::new().await?;
    let b = Running::new().await?;
    let c = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    until(|| {
        b.core.store.setup_state().setup_id.is_some()
            && b.core
                .connected_devices()
                .contains(&a.core.identity.device_id)
    })
    .await?;
    let uri = c
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", c.core.bound_port())])
        .await?;
    let preview = b.core.join(&uri.build()?).await?;
    let attempt = preview["attempt"].as_str().unwrap();
    let mut captured = b.core.store.with_current_pairing();
    captured.connection_id =
        Some(b.core.sessions.lock().unwrap().sessions[&a.core.identity.device_id].id);
    captured.setup_generation = Some(b.core.store.setup_state().generation);
    let hello = a.core.computer_hello();
    let fence = b.core.setup_fence(&captured, &hello)?;
    let mut roster = b.core.store.setup_state().roster;
    let mut member = c.core.self_member();
    member.hlc = crate::now_ms();
    roster.push(member);
    b.core
        .receive_members(
            &captured,
            &hello,
            Some(&fence),
            Members {
                transfer_id: Id::from_bytes(rand::random()),
                setup_id: b.core.store.setup_state().setup_id.unwrap(),
                members: roster,
            },
        )
        .await?;
    assert_eq!(
        b.core.store.pairing_generation(&c.core.identity.device_id),
        None
    );
    let state = b.core.setup_status().await?;
    assert!(
        state["roster"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["device_id"] == c.core.identity.device_id.as_str())
    );
    let generation = b.core.store.setup_state().generation;
    let guard = b.core.pairing_gate.clone().lock_owned().await;
    let mut removal = Box::pin(b.core.remove_setup_device_confirmed(
        c.core.identity.device_id.clone(),
        generation,
        None,
        c.core.identity.fingerprint.clone(),
    ));
    assert!(futures_util::poll!(removal.as_mut()).is_pending());
    b.core
        .store
        .pair(
            c.core.identity.device_id.clone(),
            c.core.identity.fingerprint.clone(),
        )
        .await?;
    drop(guard);
    assert!(
        removal
            .await
            .unwrap_err()
            .to_string()
            .contains("stale setup removal")
    );
    b.core
        .store
        .unpair_then(c.core.identity.device_id.clone(), |_| {})
        .await?;
    assert!(
        b.core
            .store
            .pairing_generation(&c.core.identity.device_id)
            .is_some()
    );
    assert_eq!(
        b.core
            .store
            .setup_binding_generation(&c.core.identity.device_id),
        None
    );
    // The actual IPC accepts null and checks the captured absence.
    let command: crate::ipc::Command = serde_json::from_value(
        serde_json::json!({"command":"remove_from_setup","device":c.core.identity.device_id,"setup_generation":b.core.store.setup_state().generation,"binding_generation":null,"fingerprint":c.core.identity.fingerprint}),
    )?;
    if let crate::ipc::Command::RemoveFromSetup {
        device,
        setup_generation,
        binding_generation,
        fingerprint,
    } = command
    {
        b.core
            .remove_setup_device_confirmed(
                device,
                setup_generation,
                binding_generation,
                fingerprint,
            )
            .await?;
    } else {
        bail!("wrong IPC command");
    }
    assert_eq!(b.core.join_status(attempt)?["state"], "cancelled");
    assert!(
        b.core
            .store
            .setup_state()
            .roster
            .iter()
            .any(|m| m.device_id == c.core.identity.device_id && m.status == MemberStatus::Removed)
    );
    assert!(b.core.paired_devices().contains(&a.core.identity.device_id));
    a.stop().await?;
    b.stop().await?;
    c.stop().await
}

#[tokio::test]
async fn setup_joined_notice_failed_post_releases_claim_and_keeps_durable_notice() -> Result<()> {
    let a = Running::new().await?;
    let device = Id::from_bytes([93; 16]);
    a.core
        .store
        .pair(device.clone(), Hash::new(format!("{:064x}", 93))?)
        .await?;
    let device_for_row = device.clone();
    a.core.store.call(move |c| {
        c.execute("INSERT INTO setup_notices VALUES(?,1,?)", rusqlite::params![device_for_row.as_str(), serde_json::json!({"device":device_for_row,"name":"New device","introducer_name":"Other device"}).to_string()])?;
        Ok(())
    }).await?;
    let mut first = crate::ui::window_setup::joined(&a.core).await;
    assert_eq!(first.len(), 1);
    assert!(
        crate::ui::window_setup::joined(&a.core).await.is_empty(),
        "in-flight claim"
    );
    first
        .pop()
        .unwrap()
        .posted(&a.core, Err(anyhow::anyhow!("UI child not ready")))
        .await;
    drop(first);
    let mut retry = crate::ui::window_setup::joined(&a.core).await;
    assert_eq!(
        retry.len(),
        1,
        "failed post remains durable and claim is released"
    );
    retry.pop().unwrap().posted(&a.core, Ok(1)).await;
    drop(retry);
    assert!(crate::ui::window_setup::joined(&a.core).await.is_empty());
    a.stop().await
}

#[tokio::test]
async fn setup_committed_removal_succeeds_when_outbox_cleanup_fails() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    until(|| a.core.store.setup_state().roster.len() == 2).await?;
    a.core.store.fail_next("outbox_reconcile", 1);
    a.core
        .remove_setup_device(b.core.identity.device_id.clone())
        .await?;
    assert_eq!(a.core.store.faults_remaining("outbox_reconcile"), 0);
    assert!(!a.core.paired_devices().contains(&b.core.identity.device_id));
    assert!(a.core.store.setup_state().setup_id.is_none());
    // Cleanup remains idempotent on the next ordinary reconcile.
    a.core.outbox.reconcile().await?;
    a.stop().await?;
    b.stop().await
}

#[tokio::test]
async fn setup_known_peer_consumes_only_matching_invitation_and_preserves_binding() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    pair(&a.core, &b.core, false).await?;
    let device = b.core.identity.device_id.clone();
    let generation = a.core.store.pairing_generation(&device);
    let uri = a
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", a.core.bound_port())])
        .await?;
    let store = {
        let sessions = a.core.sessions.lock().unwrap();
        let session = sessions.sessions.get(&device).context("repair session")?;
        let mut store = a.core.store.with_current_pairing();
        store.connection_id = Some(session.id);
        store
    };
    a.core
        .consume_repair_invitation(
            &store,
            &b.core.computer_hello(),
            &Hash::new("ef".repeat(32))?,
        )
        .await?;
    assert!(a.core.admission.lock().unwrap().invitation.is_some());
    a.core
        .consume_repair_invitation(&store, &b.core.computer_hello(), &uri.token)
        .await?;
    assert!(a.core.admission.lock().unwrap().invitation.is_none());
    let done = a
        .core
        .ui_snapshot()
        .pairing_completion
        .context("consumed invitation")?;
    assert_eq!(done.device, device);
    assert_eq!(done.token, uri.token);
    assert_eq!(a.core.store.pairing_generation(&device), generation);
    // Exercise the production known-peer PAIR_REQUEST path over TLS; the
    // computer Join preview deliberately short-circuits already-paired pins.
    let invitation = a
        .core
        .start_pairing_with_addresses(vec![format!("127.0.0.1:{}", a.core.bound_port())])
        .await?;
    let config = crate::tls::client_config(&b.core.identity, a.core.identity.fingerprint.clone())?;
    let hello = b.core.computer_hello();
    // Stop its automatic dials so they cannot replace this test-owned stream.
    b.stop().await?;
    until(|| a.core.connected_devices().is_empty()).await?;
    let tcp = TcpStream::connect(("127.0.0.1", a.core.bound_port())).await?;
    let mut tls: TlsStream<TcpStream> = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(rustls::pki_types::ServerName::try_from("pneu.local")?, tcp)
        .await?
        .into();
    pneu_proto::write_frame(&mut tls, &Message::Hello(hello)).await?;
    assert!(matches!(read_handshake(&mut tls).await?, Message::Hello(_)));
    pneu_proto::write_frame(
        &mut tls,
        &Message::PairRequest(PairRequest {
            token: invitation.token.clone(),
        }),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Message::PairResult(result) = read_handshake(&mut tls).await? {
                ensure!(result.accepted, "known-peer repair was refused");
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let completion = a
        .core
        .ui_snapshot()
        .pairing_completion
        .context("TLS pairing completion")?;
    assert_eq!(completion.token, invitation.token);
    drop(tls);
    assert_eq!(a.core.store.pairing_generation(&device), generation);
    assert!(a.core.admission.lock().unwrap().invitation.is_none());
    a.stop().await
}

#[test]
fn setup_refresh_grace_does_not_cross_binding_replacement() {
    let ui = Mutex::new(crate::ui::State::default());
    let device = Id::from_bytes([1; 16]);
    ui.lock().unwrap().refreshing_scope(&device);
    update_ui_state(
        &ui,
        &Event::Paired {
            device: device.clone(),
        },
    );
    assert!(ui.lock().unwrap().scope_refresh.is_empty());
    ui.lock().unwrap().refreshing_scope(&device);
    update_ui_state(&ui, &Event::Unpaired { device });
    assert!(ui.lock().unwrap().scope_refresh.is_empty());
}

#[tokio::test]
async fn setup_readmission_actions_recheck_lifecycle_gate_and_publish_only_commit() -> Result<()> {
    use crate::store::members::SetupFence;
    let a = Running::new().await?;
    let sender = Id::from_bytes([1; 16]);
    let pin = Hash::new("a".repeat(64))?;
    a.core.store.pair(sender.clone(), pin.clone()).await?;
    a.core
        .store
        .call(|c| {
            c.execute("UPDATE peers SET role='computer',name='Introducer'", [])?;
            Ok(())
        })
        .await?;
    a.core
        .store
        .setup_link(sender.clone(), SetupScope::Enabled)
        .await?;
    let local = a.core.self_member();
    let remote = Member {
        device_id: sender.clone(),
        fingerprint: pin.clone(),
        role: DeviceRole::Computer,
        name: "Introducer".into(),
        status: MemberStatus::Member,
        hlc: 1,
        writer_device: sender.clone(),
        reach: None,
        readmits: None,
    };
    let mut fence = SetupFence {
        device: sender.clone(),
        fingerprint: pin,
        binding_generation: 1,
        generation: 0,
        connection: 1,
        current: Arc::new(AtomicU64::new(1)),
    };
    a.core
        .store
        .setup_activate(
            fence.clone(),
            local.clone(),
            remote,
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: Some(Id::from_bytes([0; 16])),
            },
            |_| {},
        )
        .await?;
    fence.generation = a.core.store.setup_state().generation;
    let mut subject = Member {
        device_id: Id::from_bytes([3; 16]),
        fingerprint: Hash::new("c".repeat(64))?,
        role: DeviceRole::Computer,
        name: "Laptop".into(),
        status: MemberStatus::Removed,
        hlc: 10,
        writer_device: sender,
        reach: None,
        readmits: None,
    };
    let snapshot = |m: Member| {
        let mut roster = a.core.store.setup_state().roster;
        roster.retain(|v| v.device_id != m.device_id);
        roster.push(m);
        Members {
            transfer_id: Id::from_bytes(rand::random()),
            setup_id: Id::from_bytes([0; 16]),
            members: roster,
        }
    };
    a.core
        .store
        .members_merge(
            fence.clone(),
            local.clone(),
            snapshot(subject.clone()),
            vec![],
            |_| {},
        )
        .await?;
    // Exercise the legacy-authored migration through the real Core action path.
    a.core.store.call(|c| {c.execute("UPDATE setup_entries SET payload=json_remove(payload,'$.authored','$.revoked_binding') WHERE json_extract(payload,'$.denied')=1",[])?;Ok(())}).await?;
    subject.readmits = Some(pneu_proto::MemberReadmits {
        hlc: 10,
        writer_device: subject.writer_device.clone(),
    });
    subject.status = MemberStatus::Member;
    subject.hlc = 20;
    a.core
        .store
        .members_merge(
            fence.clone(),
            local.clone(),
            snapshot(subject.clone()),
            vec![],
            |_| {},
        )
        .await?;
    let old_key = a.core.store.setup_state().readmissions[0].key.clone();
    let gate = a.core.pairing_gate.lock().await;
    let core = a.core.clone();
    let key = old_key.clone();
    let (entered, started) = tokio::sync::oneshot::channel();
    let action = tokio::spawn(async move {
        let _ = entered.send(());
        core.decide_readmission(key, true).await
    });
    started.await?;
    subject.readmits = None;
    subject.status = MemberStatus::Removed;
    subject.hlc = 30;
    a.core
        .store
        .members_merge(
            fence.clone(),
            local.clone(),
            snapshot(subject.clone()),
            vec![],
            |_| {},
        )
        .await?;
    drop(gate);
    action.await??;
    assert!(!a.core.paired_devices().contains(&subject.device_id));
    subject.readmits = Some(pneu_proto::MemberReadmits {
        hlc: 30,
        writer_device: subject.writer_device.clone(),
    });
    subject.status = MemberStatus::Member;
    subject.hlc = 40;
    a.core
        .store
        .members_merge(fence, local, snapshot(subject.clone()), vec![], |_| {})
        .await?;
    let key = a.core.store.setup_state().readmissions[0].key.clone();
    let attempt = a.core.track_setup_incoming(99, subject.fingerprint.clone());
    assert!(a.core.decide_readmission(key.clone(), true).await.is_err());
    assert!(
        a.core.setup_status().await?["readmissions"][0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("Pairing in progress"))
    );
    assert_eq!(a.core.store.setup_state().readmissions[0].key, key);
    assert!(!a.core.paired_devices().contains(&subject.device_id));
    drop(attempt);
    a.core.decide_readmission(key.clone(), true).await?;
    assert!(a.core.paired_devices().contains(&subject.device_id));
    let generation = a.core.store.pairing_generation(&subject.device_id);
    a.core.decide_readmission(key, true).await?;
    assert_eq!(
        a.core.store.pairing_generation(&subject.device_id),
        generation
    );
    assert!(a.core.store.setup_protected(&subject.device_id));
    assert!(
        a.core.setup_status().await?["notices"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    a.stop().await
}

#[tokio::test]
async fn setup_live_rename_reaches_direct_peer_without_reconnect() -> Result<()> {
    let a = Running::new().await?;
    let b = Running::new().await?;
    pair(&a.core, &b.core, true).await?;
    until(|| {
        a.core.store.setup_state().setup_id.is_some()
            && a.core.store.setup_state().setup_id == b.core.store.setup_state().setup_id
            && a.core
                .connected_devices()
                .contains(&b.core.identity.device_id)
    })
    .await?;
    let generation = a.core.store.pairing_generation(&b.core.identity.device_id);
    b.core.rename("Office-PC 2".into()).await?;
    until(|| {
        a.core
            .ui_snapshot()
            .devices
            .iter()
            .any(|d| d.device == b.core.identity.device_id && d.name == "Office-PC 2")
    })
    .await?;
    assert_eq!(
        a.core.store.pairing_generation(&b.core.identity.device_id),
        generation
    );
    assert_eq!(
        a.core
            .store
            .peer_info()
            .await?
            .iter()
            .find(|p| p.id == b.core.identity.device_id)
            .unwrap()
            .name,
        "Office-PC 2"
    );
    a.stop().await?;
    b.stop().await
}

#[tokio::test]
async fn setup_readmits_review_core_resumes_after_cancel_and_restart() -> Result<()> {
    use crate::store::members::SetupFence;
    let mut outcomes = Vec::new();
    for restart in [false, true] {
        // No server or supervisor: exercise Core's reconciliation predicate
        // deterministically, including startup before any supervisor tick.
        let dir = crate::test_support::scratch()?;
        let paths = Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("pneud.sock"),
        };
        let config = Config {
            save_path: dir.path().join("saved"),
            clipboard_mode: crate::config::ClipboardMode::Off,
            clipboard_sync_to_phone: false,
            ..Config::default()
        };
        let mut core = Core::new(paths.clone(), config.clone(), Arc::new(NoActions)).await?;
        let sender = Id::from_bytes([1; 16]);
        let subject = Id::from_bytes([3; 16]);
        let mut remote = core.self_member();
        remote.device_id = sender.clone();
        remote.fingerprint = Hash::new("a".repeat(64))?;
        remote.name = "Introducer".into();
        remote.hlc = 1;
        remote.writer_device = sender.clone();
        remote.reach = None;
        core.store
            .pair(sender.clone(), remote.fingerprint.clone())
            .await?;
        core.store
            .call(|c| {
                c.execute("UPDATE peers SET role='computer',name='Introducer'", [])?;
                Ok(())
            })
            .await?;
        core.store
            .setup_link(sender.clone(), SetupScope::Enabled)
            .await?;
        let local = core.self_member();
        let mut fence = SetupFence {
            device: sender.clone(),
            fingerprint: remote.fingerprint.clone(),
            binding_generation: 1,
            generation: 0,
            connection: 1,
            current: Arc::new(AtomicU64::new(1)),
        };
        core.store
            .setup_activate(
                fence.clone(),
                local.clone(),
                remote.clone(),
                SetupHello {
                    scope: SetupScope::Enabled,
                    setup_id: Some(Id::from_bytes([0; 16])),
                },
                |_| {},
            )
            .await?;
        fence.generation = core.store.setup_state().generation;
        let snapshot = |value: Member| Members {
            transfer_id: Id::from_bytes(rand::random()),
            setup_id: Id::from_bytes([0; 16]),
            members: vec![local.clone(), remote.clone(), value],
        };
        let mut candidate = remote.clone();
        candidate.device_id = subject.clone();
        candidate.fingerprint = Hash::new("c".repeat(64))?;
        candidate.name = "Laptop".into();
        candidate.status = MemberStatus::Removed;
        candidate.hlc = 10;
        core.store
            .members_merge(
                fence.clone(),
                local.clone(),
                snapshot(candidate.clone()),
                vec![],
                |_| {},
            )
            .await?;
        let uri = PairingUri {
            version: 1,
            name: candidate.name.clone(),
            fingerprint: candidate.fingerprint.clone(),
            addresses: vec!["127.0.0.1:9".into()],
            token: Hash::new("d".repeat(64))?,
            expires_at: crate::now_ms() + 300_000,
        };
        let preview = core.join(&uri.build()?).await?;
        let attempt = preview["attempt"].as_str().unwrap();
        candidate.status = MemberStatus::Member;
        candidate.hlc = 20;
        candidate.readmits = Some(pneu_proto::MemberReadmits {
            hlc: 10,
            writer_device: sender,
        });
        let message = snapshot(candidate);
        core.store
            .members_merge(
                fence.clone(),
                local.clone(),
                message.clone(),
                core.setup_attempts(),
                |_| {},
            )
            .await?;
        core.reconcile_setup_candidates().await?;
        assert!(core.store.pairing_generation(&subject).is_none());
        let rows: i64 = core
            .store
            .call(|c| Ok(c.query_row("SELECT count(*) FROM setup_candidates", [], |r| r.get(0))?))
            .await?;
        assert_eq!(rows, 0, "the deferred claim lives only in entry JSON");
        assert!(
            core.store
                .members_merge(fence, local.clone(), message, core.setup_attempts(), |_| {})
                .await?,
            "an identical replay is deduplicated"
        );
        if restart {
            core.shutdown_actions().await?;
            core.store.drain().await;
            drop(core);
            core = Core::new(paths, config, Arc::new(NoActions)).await?;
        } else {
            core.cancel_join(attempt).await?;
            core.reconcile_setup_candidates().await?;
        }
        outcomes.push((
            restart,
            core.store.pairing_generation(&subject).is_some(),
            core.paired_devices().contains(&subject),
        ));
        assert!(core.setup_request_notice().await.is_none());
        if let Some(link) = core
            .store
            .setup_state()
            .links
            .into_iter()
            .find(|l| l.device == subject)
        {
            assert_eq!(link.introducer_name.as_deref(), Some("Introducer"));
        }
        core.shutdown_actions().await?;
    }
    assert_eq!(outcomes, vec![(false, true, true), (true, true, true)]);
    Ok(())
}

#[tokio::test]
async fn setup_readmits_notification_selection_only_asks_authored_barriers() -> Result<()> {
    use crate::store::members::SetupFence;
    let a = Running::new().await?;
    let sender = Id::from_bytes([1; 16]);
    let subject = Id::from_bytes([3; 16]);
    let pin = Hash::new("a".repeat(64))?;
    a.core.store.pair(sender.clone(), pin.clone()).await?;
    a.core
        .store
        .call(|c| {
            c.execute("UPDATE peers SET role='computer',name='Introducer'", [])?;
            Ok(())
        })
        .await?;
    a.core
        .store
        .setup_link(sender.clone(), SetupScope::Enabled)
        .await?;
    let local = a.core.self_member();
    let remote = Member {
        device_id: sender.clone(),
        fingerprint: pin.clone(),
        role: DeviceRole::Computer,
        name: "Introducer".into(),
        status: MemberStatus::Member,
        hlc: 1,
        writer_device: sender.clone(),
        reach: None,
        readmits: None,
    };
    let mut fence = SetupFence {
        device: sender.clone(),
        fingerprint: pin,
        binding_generation: 1,
        generation: 0,
        connection: 1,
        current: Arc::new(AtomicU64::new(1)),
    };
    a.core
        .store
        .setup_activate(
            fence.clone(),
            local.clone(),
            remote,
            SetupHello {
                scope: SetupScope::Enabled,
                setup_id: Some(Id::from_bytes([0; 16])),
            },
            |_| {},
        )
        .await?;
    fence.generation = a.core.store.setup_state().generation;
    let snapshot = |m: Member| {
        let mut roster = a.core.store.setup_state().roster;
        roster.retain(|r| r.device_id != m.device_id);
        roster.push(m);
        Members {
            transfer_id: Id::from_bytes(rand::random()),
            setup_id: Id::from_bytes([0; 16]),
            members: roster,
        }
    };
    let removed = Member {
        device_id: subject.clone(),
        fingerprint: Hash::new("c".repeat(64))?,
        role: DeviceRole::Computer,
        name: "Laptop".into(),
        status: MemberStatus::Removed,
        hlc: 10,
        writer_device: sender.clone(),
        reach: None,
        readmits: None,
    };
    a.core
        .store
        .members_merge(
            fence.clone(),
            local.clone(),
            snapshot(removed),
            vec![],
            |_| {},
        )
        .await?;
    let mut claimed = Member {
        device_id: subject.clone(),
        fingerprint: Hash::new("c".repeat(64))?,
        role: DeviceRole::Computer,
        name: "Laptop".into(),
        status: MemberStatus::Member,
        hlc: 20,
        writer_device: sender.clone(),
        reach: None,
        readmits: Some(pneu_proto::MemberReadmits {
            hlc: 10,
            writer_device: sender,
        }),
    };
    a.core
        .store
        .members_merge(
            fence.clone(),
            local.clone(),
            snapshot(claimed.clone()),
            vec![(Some(subject.clone()), claimed.fingerprint.clone())],
            |_| {},
        )
        .await?;
    assert!(
        a.core.setup_request_notice().await.is_none(),
        "blocked inherited follow never asks"
    );
    a.core
        .store
        .setup_reconcile_candidates(local.device_id.clone(), vec![], |_| {})
        .await?;
    assert!(
        a.core.setup_request_notice().await.is_none(),
        "inherited follow never asks"
    );
    a.core
        .store
        .setup_remove(local.device_id.clone(), subject.clone(), |_| {})
        .await?;
    let removal = a
        .core
        .store
        .setup_state()
        .roster
        .into_iter()
        .find(|m| m.device_id == subject)
        .unwrap();
    claimed.hlc = removal.hlc + 1;
    claimed.readmits = Some(pneu_proto::MemberReadmits {
        hlc: removal.hlc,
        writer_device: removal.writer_device,
    });
    a.core
        .store
        .members_merge(
            fence.clone(),
            local.clone(),
            snapshot(claimed.clone()),
            vec![],
            |_| {},
        )
        .await?;
    let first = a
        .core
        .setup_request_notice()
        .await
        .expect("authored question");
    let request = first.setup_requests[0].readmission.clone().unwrap();
    claimed.hlc += 1;
    a.core
        .store
        .members_merge(fence, local.clone(), snapshot(claimed), vec![], |_| {})
        .await?;
    let second = a
        .core
        .setup_request_notice()
        .await
        .expect("same authored question");
    assert_eq!(second.setup_requests[0].readmission, Some(request.clone()));
    a.core
        .store
        .setup_readmit(request, local.device_id, false, vec![], |_| {})
        .await?;
    assert!(
        a.core.setup_request_notice().await.is_none(),
        "Keep removed closes the question"
    );
    a.stop().await
}
