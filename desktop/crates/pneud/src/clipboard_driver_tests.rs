use super::*;
use crate::{
    Config, Paths,
    actions::{PcActions, PreparedClipboard},
};
use async_trait::async_trait;
use std::{path::Path, time::Duration};

struct Actions(u8);
#[async_trait]
impl PcActions for Actions {
    async fn clipboard_text(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn clipboard_image(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn notify(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn open(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    async fn prepare_clipboard(
        &self,
        input: ClipboardInput,
        _: Ticket,
    ) -> Result<PreparedClipboard> {
        match self.0 {
            1 => panic!("injected prepare panic"),
            3 => return Err(anyhow!("injected prepare error")),
            _ => {}
        }
        let mut p = PreparedClipboard::immediate(input)?;
        if self.0 == 2 {
            p.marker_confirmation = true;
            p.test_exited = true;
        }
        Ok(p)
    }
    async fn install_clipboard(&self, _: &mut PreparedClipboard) -> Result<()> {
        match self.0 {
            4 => Err(anyhow!("injected install error")),
            5 => panic!("injected install panic"),
            _ => Ok(()),
        }
    }
}
async fn scenario(mode: u8) -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let core = Core::new(
        Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("socket"),
        },
        Config {
            save_path: dir.path().join("saved"),
            ..Config::default()
        },
        Arc::new(Actions(mode)),
    )
    .await?;
    let handle = core.clipboard.clone();
    let token = handle.token();
    let content = Content::Text("remote".into());
    let operation = Operation {
        token,
        id: Id::from_bytes([10; 16]),
        source: Id::from_bytes([2; 16]),
        key: Some(Key {
            hlc: 100,
            source: Id::from_bytes([2; 16]),
        }),
        created_at: 100,
        payload: None,
        hashes: hashes(&content),
        content: Some(content),
        offer: None,
        received: true,
        legacy: false,
        restore: false,
        repair: false,
        observation: None,
    };
    let (reply, _) = oneshot::channel();
    let request = Request {
        input: ClipboardInput::Text("remote".into()),
        claim: None,
        store: core.store.clone(),
        authority: None,
        kind: None,
        policy: vec![],
        reply,
    };
    let result =
        tokio::time::timeout(Duration::from_secs(2), handle.submit(operation, request)).await;
    let stopped = tokio::time::timeout(Duration::from_secs(2), core.shutdown_actions()).await;
    assert!(stopped.is_ok(), "mode={mode}: shutdown stalled");
    stopped??;
    assert!(result.is_ok(), "mode={mode}: no terminal reply");
    assert_eq!(result?.is_ok(), mode == 0);
    Ok(())
}
#[tokio::test]
async fn review_driver_success_must_drain() -> Result<()> {
    scenario(0).await
}
#[tokio::test]
async fn review_driver_prepare_panic_must_drain() -> Result<()> {
    scenario(1).await
}
#[tokio::test]
async fn review_driver_early_helper_exit_must_drain() -> Result<()> {
    scenario(2).await
}
#[tokio::test]
async fn review_driver_prepare_error_must_drain() -> Result<()> {
    scenario(3).await
}
#[tokio::test]
async fn review_driver_install_error_must_drain() -> Result<()> {
    scenario(4).await
}
#[tokio::test]
async fn review_driver_install_panic_must_drain() -> Result<()> {
    scenario(5).await
}
#[test]
fn watcher_flood_has_bounded_admission() {
    let (handle, rx) = Handle::channel();
    for token in 1..=100_000 {
        handle.event(Event::Observe {
            token,
            id: Id::from_bytes([1; 16]),
            offer: None,
            origin: false,
            clear: false,
        });
    }
    assert!(rx.len() <= 64, "queued {} watcher observations", rx.len());
}

#[tokio::test]
async fn watcher_coalescing_cancels_reads_and_preserves_marker_and_completion_order() {
    let (handle, mut rx) = Handle::channel();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    handle.event(Event::Observe {
        token: 1,
        id: Id::from_bytes([1; 16]),
        offer: None,
        origin: false,
        clear: false,
    });
    handle
        .tx
        .send(Message::ReadHandle(1, cancel.clone()))
        .unwrap();
    handle.event(Event::Installed(42));
    handle.event(Event::Persisted {
        token: 123,
        success: true,
    });
    handle.event(Event::Stopped(42));
    for token in 2..=100_000 {
        handle.event(Event::Observe {
            token,
            id: Id::from_bytes([1; 16]),
            offer: None,
            origin: false,
            clear: false,
        });
    }
    assert!(cancel.load(Ordering::SeqCst));
    assert!(rx.len() <= 11);
    assert!(matches!(
        rx.recv().await,
        Some(Message::Event(Event::Installed(42)))
    ));
    assert!(matches!(
        rx.recv().await,
        Some(Message::Event(Event::Persisted {
            token: 123,
            success: true
        }))
    ));
    assert!(matches!(
        rx.recv().await,
        Some(Message::Event(Event::Stopped(42)))
    ));
    for token in 99_993..=100_000 {
        assert!(
            matches!(rx.recv().await,Some(Message::Event(Event::Observe{token:t,..})) if t==token)
        );
    }
    assert_eq!(rx.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn queued_request_during_persistent_clock_failure_times_out_and_shutdown_drains() -> Result<()>
{
    let dir = crate::test_support::scratch()?;
    let core = Core::new(
        Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("socket"),
        },
        Config {
            save_path: dir.path().join("saved"),
            ..Config::default()
        },
        Arc::new(Actions(0)),
    )
    .await?;
    let (handle, rx) = Handle::channel();
    let actor = tokio::spawn(run(
        Arc::downgrade(&core),
        handle.clone(),
        rx,
        State::default(),
    ));
    core.store
        .fail_next("local_observation_checked", usize::MAX);
    let observation = handle.token();
    handle.event(Event::Observe {
        token: observation,
        id: Id::from_bytes([12; 16]),
        offer: None,
        origin: false,
        clear: false,
    });
    handle.event(Event::Read {
        token: observation,
        content: Some(Content::Text("local".into())),
        fingerprints: vec![],
        debounce: false,
    });
    let content = Content::Text("queued remote".into());
    let token = handle.token();
    let operation = Operation {
        token,
        id: Id::from_bytes([11; 16]),
        source: Id::from_bytes([2; 16]),
        key: Some(Key {
            hlc: 100,
            source: Id::from_bytes([2; 16]),
        }),
        created_at: 100,
        hashes: hashes(&content),
        content: Some(content),
        payload: None,
        offer: None,
        received: true,
        legacy: false,
        restore: false,
        repair: false,
        observation: None,
    };
    let (reply, _) = oneshot::channel();
    let request = Request {
        input: ClipboardInput::Text("queued remote".into()),
        claim: None,
        store: core.store.clone(),
        authority: None,
        kind: None,
        policy: vec![],
        reply,
    };
    let submission = tokio::spawn({
        let handle = handle.clone();
        async move { handle.submit(operation, request).await }
    });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(11)).await;
    let completed = tokio::time::timeout(Duration::from_secs(1), submission).await;
    handle.stop().await;
    actor.await?;
    core.shutdown_actions().await?;
    assert!(
        completed.is_ok(),
        "an admitted request behind the ordering fence had no terminal timeout"
    );
    assert!(completed??.is_err());
    Ok(())
}

#[derive(Default)]
struct MarkerActions {
    tickets: std::sync::Mutex<Vec<Ticket>>,
    clipboard: std::sync::Mutex<String>,
}
#[async_trait]
impl PcActions for MarkerActions {
    async fn clipboard_text(&self, text: &str) -> Result<()> {
        *self.clipboard.lock().unwrap() = text.into();
        Ok(())
    }
    async fn clipboard_image(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn notify(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn open(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    async fn prepare_clipboard(
        &self,
        input: ClipboardInput,
        ticket: Ticket,
    ) -> Result<PreparedClipboard> {
        self.tickets.lock().unwrap().push(ticket);
        PreparedClipboard::immediate(input)
    }
}
#[tokio::test]
async fn retired_install_marker_is_read_as_an_echo_and_restores_current_input() -> Result<()> {
    for legacy in [false, true] {
        retired_marker_scenario(legacy).await?;
    }
    Ok(())
}
async fn retired_marker_scenario(legacy: bool) -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let actions = Arc::new(MarkerActions::default());
    let core = Core::new(
        Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("socket"),
        },
        Config {
            save_path: dir.path().join("saved"),
            ..Config::default()
        },
        actions.clone(),
    )
    .await?;
    for (n, value) in [(10u8, "old remote"), (20u8, "winner")] {
        let token = core.clipboard.token();
        let content = Content::Text(value.into());
        let (reply, _) = oneshot::channel();
        let result = core
            .clipboard
            .submit(
                Operation {
                    token,
                    id: Id::from_bytes([n; 16]),
                    source: Id::from_bytes([2; 16]),
                    key: (!legacy).then_some(Key {
                        hlc: n as u64,
                        source: Id::from_bytes([2; 16]),
                    }),
                    created_at: n as u64,
                    hashes: hashes(&content),
                    content: Some(content),
                    payload: None,
                    offer: None,
                    received: true,
                    legacy,
                    restore: false,
                    repair: false,
                    observation: None,
                },
                Request {
                    input: ClipboardInput::Text(value.into()),
                    claim: None,
                    store: core.store.clone(),
                    authority: None,
                    kind: None,
                    policy: vec![],
                    reply,
                },
            )
            .await?;
        assert!(result.applied);
    }
    let register = core.store.clipboard_snapshot().await?.0;
    let retired = actions.tickets.lock().unwrap()[0];
    let reoffer = {
        let core = core.clone();
        tokio::task::spawn_blocking(move || core.clipboard_install_marker(retired)).await?
    };
    assert!(
        !reoffer,
        "retired ticket consumed the clipboard observation"
    );
    *actions.clipboard.lock().unwrap() = "old remote".into();
    let (_, token) = core.observe_watched_clipboard(false);
    core.clipboard_observed(token, Some(Content::Text("old remote".into())), false);
    tokio::time::timeout(Duration::from_secs(3), core.clipboard_test_idle()).await?;
    assert_eq!(&*actions.clipboard.lock().unwrap(), "winner");
    assert_eq!(actions.tickets.lock().unwrap().len(), 3);
    assert_eq!(core.store.clipboard_snapshot().await?.0, register);
    core.shutdown_actions().await?;
    Ok(())
}

#[derive(Default)]
struct PayloadActions {
    prepared: std::sync::Mutex<Vec<Content>>,
    installed: std::sync::Mutex<Vec<Content>>,
}
fn input_content(input: &ClipboardInput) -> Result<Content> {
    Ok(match input {
        ClipboardInput::Text(text) => Content::Text(text.clone()),
        ClipboardInput::Image(file, _) => Content::Image(file.read_bounded(16 * 1024 * 1024)?),
        ClipboardInput::Bytes { bytes, .. } => Content::Image(bytes.clone()),
        ClipboardInput::Formats { text, png, .. } => Content::Formats(text.clone(), png.clone()),
    })
}
#[async_trait]
impl PcActions for PayloadActions {
    async fn clipboard_text(&self, _: &str) -> Result<()> {
        anyhow::bail!("test must use prepared installation")
    }
    async fn clipboard_image(&self, _: &Path, _: &str) -> Result<()> {
        anyhow::bail!("test must use prepared installation")
    }
    async fn notify(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    async fn open(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    async fn prepare_clipboard(
        &self,
        input: ClipboardInput,
        _: Ticket,
    ) -> Result<PreparedClipboard> {
        self.prepared.lock().unwrap().push(input_content(&input)?);
        PreparedClipboard::immediate(input)
    }
    async fn install_clipboard(&self, _: &mut PreparedClipboard) -> Result<()> {
        let content = self.prepared.lock().unwrap().last().unwrap().clone();
        self.installed.lock().unwrap().push(content);
        Ok(())
    }
}
fn payload_formats(label: &str) -> Vec<Content> {
    vec![
        Content::Text(label.into()),
        Content::Image(format!("{label} image").into_bytes()),
        Content::Formats(label.into(), format!("{label} image").into_bytes()),
    ]
}
async fn payload_core(dir: &Path, actions: Arc<PayloadActions>) -> Result<Arc<Core>> {
    Core::new(
        Paths {
            data: dir.join("data"),
            cache: dir.join("cache"),
            config: dir.join("config/config.toml"),
            socket: dir.join("socket"),
        },
        Config {
            save_path: dir.join("saved"),
            ..Config::default()
        },
        actions,
    )
    .await
}
async fn submit_payload(core: &Arc<Core>, content: &Content) -> Result<Outcome> {
    let token = core.clipboard.token();
    let source = Id::from_bytes([2; 16]);
    let hlc = core
        .store
        .clipboard_snapshot()
        .await?
        .0
        .map_or(100, |key| key.hlc + 1);
    let (outbound, input) = match content {
        Content::Image(bytes) => {
            // Exercise the production image path: the operation has hashes and
            // an anchored file input, but no outbound in-memory Content.
            let path = core.paths.cache.join(format!("received-{token}.png"));
            std::fs::write(&path, bytes)?;
            (
                None,
                ClipboardInput::Image(
                    crate::actions::ReceivedFile::open(&path)?,
                    "image/png".into(),
                ),
            )
        }
        _ => (
            Some(content.clone()),
            core.clipboard_input(Some(content.clone()))?,
        ),
    };
    let operation = Operation {
        token,
        id: Id::from_bytes([token as u8; 16]),
        source: source.clone(),
        key: Some(Key { hlc, source }),
        created_at: hlc,
        content: outbound,
        payload: None,
        hashes: hashes(content),
        offer: None,
        received: true,
        legacy: false,
        restore: false,
        repair: false,
        observation: None,
    };
    let (reply, _) = oneshot::channel();
    tokio::time::timeout(
        Duration::from_secs(3),
        core.clipboard.submit(
            operation,
            Request {
                input,
                claim: None,
                store: core.store.clone(),
                authority: None,
                kind: None,
                policy: vec![],
                reply,
            },
        ),
    )
    .await?
}
async fn observe_payload(core: &Arc<Core>, content: Content) -> Result<()> {
    let (_, token) = core.observe_watched_clipboard(false);
    core.clipboard_observed(token, Some(content), false);
    tokio::time::timeout(Duration::from_secs(3), core.clipboard_test_idle()).await?;
    Ok(())
}
#[tokio::test]
async fn repair_uses_local_winner_after_no_write_dedup_for_text_image_and_mixed() -> Result<()> {
    for older in payload_formats("older") {
        for winner in payload_formats("winner") {
            let mut candidates = vec![None, Some(winner.clone())];
            if let Content::Formats(text, image) = &winner {
                candidates.extend([
                    Some(Content::Text(text.clone())),
                    Some(Content::Image(image.clone())),
                    Some(Content::Formats(
                        text.clone(),
                        b"uninstalled image".to_vec(),
                    )),
                ]);
            }
            for duplicate in candidates {
                let dir = crate::test_support::scratch()?;
                let actions = Arc::new(PayloadActions::default());
                let core = payload_core(dir.path(), actions.clone()).await?;
                assert!(submit_payload(&core, &older).await?.applied);
                observe_payload(&core, winner.clone()).await?;
                assert_eq!(actions.installed.lock().unwrap().len(), 1);
                if let Some(duplicate) = duplicate {
                    let outcome = submit_payload(&core, &duplicate).await?;
                    assert!(outcome.applied && outcome.deduplicated);
                    assert_eq!(actions.installed.lock().unwrap().len(), 1);
                }
                let register = core.store.clipboard_snapshot().await?.0;
                observe_payload(&core, older.clone()).await?;
                assert_eq!(
                    *actions.installed.lock().unwrap(),
                    vec![older.clone(), winner.clone()]
                );
                assert_eq!(core.store.clipboard_snapshot().await?.0, register);
                core.shutdown_actions().await?;
            }
        }
    }
    Ok(())
}
#[tokio::test]
async fn repair_declines_a_changed_file_backing_the_register_winner() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let actions = Arc::new(PayloadActions::default());
    let core = payload_core(dir.path(), actions.clone()).await?;
    let older = Content::Image(b"older image".to_vec());
    let winner = Content::Image(b"winner image".to_vec());
    submit_payload(&core, &older).await?;
    submit_payload(&core, &winner).await?;
    let register = core.store.clipboard_snapshot().await?.0;
    for entry in std::fs::read_dir(&core.paths.cache)? {
        let path = entry?.path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("received-")
        {
            std::fs::write(path, b"changed image")?;
        }
    }
    observe_payload(&core, older.clone()).await?;
    assert_eq!(*actions.installed.lock().unwrap(), vec![older, winner]);
    assert_eq!(actions.prepared.lock().unwrap().len(), 3);
    assert_eq!(core.store.clipboard_snapshot().await?.0, register);
    core.shutdown_actions().await?;
    Ok(())
}

async fn recovery_fixture() -> Result<(tempfile::TempDir, Arc<Core>)> {
    let dir = crate::test_support::scratch()?;
    let core = Core::new(
        Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: dir.path().join("socket"),
        },
        Config {
            save_path: dir.path().join("saved"),
            ..Config::default()
        },
        Arc::new(Actions(0)),
    )
    .await?;
    Ok((dir, core))
}

fn receive_operation(token: Token, text: &pneu_proto::Clipboard) -> Operation {
    let content = Content::Text(text.text.clone());
    Operation {
        token,
        id: text.transfer_id.clone(),
        source: text.source_device.clone().unwrap(),
        key: Some(Key {
            hlc: text.hlc.unwrap(),
            source: text.source_device.clone().unwrap(),
        }),
        created_at: text.created_at,
        payload: None,
        hashes: hashes(&content),
        content: Some(content),
        offer: None,
        received: true,
        legacy: false,
        restore: false,
        repair: false,
        observation: None,
    }
}

#[tokio::test]
async fn watcher_clock_and_all_sixteen_real_claims_complete_before_request_deadlines() -> Result<()>
{
    for debounce in [false, true] {
        let (_dir, core) = recovery_fixture().await?;
        let mut claims = Vec::new();
        for n in 2..=17 {
            let peer = Id::from_bytes([n; 16]);
            core.store
                .pair(peer.clone(), pneu_proto::Hash::new(format!("{n:064x}"))?)
                .await?;
            let text = pneu_proto::Clipboard {
                transfer_id: Id::from_bytes([n + 20; 16]),
                text: format!("incoming-{n}"),
                created_at: n as u64,
                origin: pneu_proto::ClipboardOrigin::Clipboard,
                hlc: Some(n as u64),
                source_device: Some(peer.clone()),
            };
            let claim = core.store.claim_text(peer, text.clone()).await?.unwrap();
            claims.push((text, claim));
        }
        assert_eq!(core.store.admitted_jobs(), crate::work::BLOCKING_LIMIT);
        let handle = &core.clipboard;
        let token = handle.token();
        let observed = handle.waiter(token);
        handle.event(Event::Observe {
            token,
            id: Id::from_bytes([1; 16]),
            offer: None,
            origin: false,
            clear: false,
        });
        if debounce {
            handle.event(Event::Read {
                token,
                content: Some(Content::Text("local".into())),
                fingerprints: vec![],
                debounce: true,
            });
        }
        let mut replies = Vec::new();
        for (text, claim) in claims {
            let operation = receive_operation(handle.token(), &text);
            let (reply, rx) = oneshot::channel();
            let request = Request {
                input: ClipboardInput::Text(text.text),
                claim: Some(claim),
                store: core.store.clone(),
                authority: None,
                kind: None,
                policy: vec![],
                reply,
            };
            let permit = handle.admission.clone().acquire_owned().await?;
            handle
                .tx
                .send(Message::Submit(operation, Box::new(request), permit))
                .unwrap();
            replies.push(rx);
        }
        if !debounce {
            handle.event(Event::Read {
                token,
                content: Some(Content::Text("local".into())),
                fingerprints: vec![],
                debounce: false,
            });
        }
        let completed = tokio::time::timeout(Duration::from_secs(4), async {
            observed.await??;
            for reply in replies {
                reply.await??;
            }
            handle.barrier().await;
            Ok::<_, anyhow::Error>(())
        })
        .await;
        core.shutdown_actions().await?;
        completed??;
        assert_eq!(core.store.admitted_jobs(), 0);
        assert_eq!(core.store.active_jobs(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn transient_watcher_clock_failure_recovers_without_another_observation() -> Result<()> {
    let (_dir, core) = recovery_fixture().await?;
    let handle = &core.clipboard;
    core.store.fail_next("local_observation_checked", 3);
    let token = handle.token();
    let observed = handle.waiter(token);
    handle.event(Event::Observe {
        token,
        id: Id::from_bytes([1; 16]),
        offer: None,
        origin: false,
        clear: false,
    });
    handle.event(Event::Read {
        token,
        content: Some(Content::Text("local".into())),
        fingerprints: vec![],
        debounce: false,
    });
    let text = pneu_proto::Clipboard {
        transfer_id: Id::from_bytes([2; 16]),
        text: "incoming after recovery".into(),
        created_at: 100,
        origin: pneu_proto::ClipboardOrigin::Clipboard,
        hlc: Some(crate::now_ms() + 60_000),
        source_device: Some(Id::from_bytes([3; 16])),
    };
    let (reply, _) = oneshot::channel();
    let request = Request {
        input: ClipboardInput::Text(text.text.clone()),
        claim: None,
        store: core.store.clone(),
        authority: None,
        kind: None,
        policy: vec![],
        reply,
    };
    let completed = tokio::time::timeout(Duration::from_secs(4), async {
        assert!(
            handle
                .submit(receive_operation(handle.token(), &text), request)
                .await?
                .applied
        );
        observed.await??;
        handle.barrier().await;
        Ok::<_, anyhow::Error>(())
    })
    .await;
    core.shutdown_actions().await?;
    completed??;
    assert_eq!(core.store.faults_remaining("local_observation_checked"), 0);
    assert_eq!(core.store.active_jobs(), 0);
    Ok(())
}

#[tokio::test]
async fn lost_read_completion_expires_and_releases_the_actor_without_user_activity() -> Result<()> {
    let (_dir, core) = recovery_fixture().await?;
    let handle = &core.clipboard;
    let token = handle.token();
    let observed = handle.waiter(token);
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    handle.event(Event::Observe {
        token,
        id: Id::from_bytes([1; 16]),
        offer: None,
        origin: false,
        clear: false,
    });
    handle
        .tx
        .send(Message::ReadHandle(token, cancel.clone()))
        .unwrap();
    let text = pneu_proto::Clipboard {
        transfer_id: Id::from_bytes([2; 16]),
        text: "incoming after read timeout".into(),
        created_at: 100,
        origin: pneu_proto::ClipboardOrigin::Clipboard,
        hlc: Some(crate::now_ms() + 60_000),
        source_device: Some(Id::from_bytes([3; 16])),
    };
    let (reply, _) = oneshot::channel();
    let request = Request {
        input: ClipboardInput::Text(text.text.clone()),
        claim: None,
        store: core.store.clone(),
        authority: None,
        kind: None,
        policy: vec![],
        reply,
    };
    let completed = tokio::time::timeout(Duration::from_secs(8), async {
        let result = handle
            .submit(receive_operation(handle.token(), &text), request)
            .await?;
        assert!(result.applied);
        observed.await??;
        handle.barrier().await;
        Ok::<_, anyhow::Error>(())
    })
    .await;
    core.shutdown_actions().await?;
    completed??;
    assert!(cancel.load(Ordering::SeqCst));
    assert_eq!(core.store.active_jobs(), 0);
    Ok(())
}
