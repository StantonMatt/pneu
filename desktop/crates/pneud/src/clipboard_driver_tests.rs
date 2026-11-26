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
        file_group: None,
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
        file_group: None,
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
                    file_group: None,
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
        #[cfg(target_os = "linux")]
        ClipboardInput::Files(files) => {
            Content::Text(String::from_utf8(crate::actions::file_uri_list(files)?)?)
        }
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
        #[cfg(target_os = "linux")]
        let input = match input {
            ClipboardInput::Files(files) => {
                let cache = files[0].path().parent().unwrap();
                ClipboardInput::Files(crate::actions::named_clipboard_files(files.clone(), cache)?)
            }
            input => input,
        };
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
        file_group: None,
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
#[cfg(target_os = "linux")]
#[tokio::test]
async fn cached_file_winner_repair_reoffers_identical_named_uris() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let actions = Arc::new(PayloadActions::default());
    let core = payload_core(dir.path(), actions.clone()).await?;
    let older = Content::Text("older selection".into());
    assert!(submit_payload(&core, &older).await?.applied);
    let path = core.paths.cache.join(".pneu-cache-random-document.pdf");
    std::fs::write(&path, b"%PDF synthetic")?;
    let input = ClipboardInput::Files(vec![
        crate::actions::ReceivedFile::open(&path)?.with_clipboard_name("document.pdf".into()),
    ]);
    let token = core.clipboard.token();
    let source = Id::from_bytes([2; 16]);
    let (reply, _) = oneshot::channel();
    assert!(
        core.clipboard
            .submit(
                Operation {
                    token,
                    id: Id::from_bytes([20; 16]),
                    source: source.clone(),
                    key: Some(Key { hlc: 200, source }),
                    created_at: 200,
                    content: None,
                    payload: None,
                    hashes: Vec::new(),
                    offer: None,
                    received: true,
                    legacy: false,
                    file_group: None,
                    restore: false,
                    repair: false,
                    observation: None,
                },
                Request {
                    input,
                    claim: None,
                    store: core.store.clone(),
                    authority: None,
                    kind: None,
                    policy: vec![],
                    reply,
                },
            )
            .await?
            .applied
    );
    let register = core.store.clipboard_snapshot().await?.0;
    let winner = actions.installed.lock().unwrap()[1].clone();
    assert!(
        matches!(&winner, Content::Text(uri) if uri.ends_with("/document.pdf\r\n") && !uri.contains(".pneu-cache-"))
    );
    // Once installed, the winner itself must keep the private alias alive.
    std::fs::remove_file(path)?;
    observe_payload(&core, older.clone()).await?;
    assert_eq!(
        *actions.installed.lock().unwrap(),
        vec![older, winner.clone(), winner]
    );
    assert_eq!(core.store.clipboard_snapshot().await?.0, register);
    core.shutdown_actions().await?;
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
        file_group: None,
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

#[tokio::test]
async fn initial_selection_driver_never_publishes_until_manual_share() -> Result<()> {
    for mode in [
        ClipboardMode::Automatic,
        ClipboardMode::Manual,
        ClipboardMode::Off,
    ] {
        for value in payload_formats("already on clipboard") {
            let dir = crate::test_support::scratch()?;
            let actions = Arc::new(PayloadActions::default());
            let core = payload_core(dir.path(), actions.clone()).await?;
            core.set_setting("clipboard_include_images".into(), true.into())
                .await?;
            core.set_setting("clipboard_mode".into(), serde_json::to_value(mode)?)
                .await?;
            let (_peer, _connection) = connect_clipboard_peer(&core, 3).await?;
            let (offer, token) = core.observe_watched_clipboard_kind(false, false, true);
            assert!(offer.is_none());
            core.clipboard_test_read(token, Some(value.clone())).await;
            core.clipboard_test_idle().await;
            assert!(core.automatic.lock().unwrap().selections.is_empty());
            let result = core
                .share_clipboard(crate::outbox::Targets::All("all".into()))
                .await;
            assert_eq!(result.is_ok(), mode == ClipboardMode::Manual);
            core.clipboard_test_idle().await;
            core.automatic.lock().unwrap().invalidate();
            // Off keeps observing. Enabling either mode must not publish the
            // old selection, even if the bounded initial read is still pending.
            for next in [ClipboardMode::Automatic, ClipboardMode::Manual] {
                core.set_setting("clipboard_mode".into(), "off".into())
                    .await?;
                let (_, token) = core.observe_watched_clipboard_kind(false, false, true);
                core.set_setting("clipboard_mode".into(), serde_json::to_value(next)?)
                    .await?;
                core.clipboard_test_read(token, Some(value.clone())).await;
                core.clipboard_test_idle().await;
                assert!(core.automatic.lock().unwrap().selections.is_empty());
            }
            assert!(
                core.share_clipboard(crate::outbox::Targets::All("all".into()))
                    .await
                    .is_ok()
            );
            core.clipboard_test_idle().await;
            assert_eq!(core.automatic.lock().unwrap().selections.len(), 1);
            assert!(actions.installed.lock().unwrap().is_empty());
            core.shutdown_actions().await?;
        }
    }
    Ok(())
}

async fn connect_clipboard_peer(
    core: &Core,
    n: u8,
) -> Result<(Id, watch::Receiver<Option<crate::core::CloseReason>>)> {
    use crate::core::{CloseReason, Session};
    let peer = Id::from_bytes([n; 16]);
    let fingerprint = pneu_proto::Hash::new(format!("{n:x}").repeat(64))?;
    core.store.pair(peer.clone(), fingerprint.clone()).await?;
    core.admission
        .lock()
        .unwrap()
        .peers
        .insert(fingerprint, peer.clone());
    let (close, connection) = watch::channel::<Option<CloseReason>>(None);
    core.sessions.lock().unwrap().sessions.insert(
        peer.clone(),
        Session {
            device: peer.clone(),
            id: 1,
            close,
            wake: Arc::new(tokio::sync::Notify::new()),
            paused: false,
            capabilities: vec![
                "mesh".into(),
                "clipboard_text".into(),
                "clipboard_image".into(),
            ],
            outbound: false,
            mesh: true,
            role: "phone".into(),
            phone: Default::default(),
        },
    );
    Ok((peer, connection))
}

#[tokio::test]
async fn initial_received_echo_driver_survives_daemon_restart_and_excludes_source() -> Result<()> {
    for older in [false, true] {
        for value in payload_formats("received before restart") {
            let dir = crate::test_support::scratch()?;
            let core = payload_core(dir.path(), Arc::new(PayloadActions::default())).await?;
            submit_payload(&core, &value).await?;
            if older {
                submit_payload(&core, &Content::Text("later winner".into())).await?;
            }
            let before = core.store.clipboard_snapshot().await?.0;
            core.shutdown_actions().await?;
            drop(core);
            let actions = Arc::new(PayloadActions::default());
            let core = payload_core(dir.path(), actions.clone()).await?;
            core.set_setting("clipboard_include_images".into(), true.into())
                .await?;
            let (source, _source_connection) = connect_clipboard_peer(&core, 2).await?;
            let (other, _other_connection) = connect_clipboard_peer(&core, 3).await?;
            let (offer, token) = core.observe_watched_clipboard_kind(false, false, true);
            assert!(offer.is_none());
            core.clipboard_test_read(token, Some(value.clone())).await;
            core.clipboard_test_idle().await;
            assert_eq!(core.store.clipboard_snapshot().await?.0, before);
            assert!(core.automatic.lock().unwrap().selections.is_empty());
            assert!(actions.installed.lock().unwrap().is_empty());
            core.set_setting("clipboard_mode".into(), "manual".into())
                .await?;
            assert!(
                core.share_clipboard(crate::outbox::Targets::All("all".into()))
                    .await
                    .is_ok()
            );
            core.clipboard_test_idle().await;
            let selections = core.automatic.lock().unwrap().selections.clone();
            assert!(!selections.contains_key(&source));
            assert!(selections.contains_key(&other));
            core.shutdown_actions().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn initial_selection_driver_recovers_after_compositor_disconnect() -> Result<()> {
    for value in payload_formats("restored selection") {
        let dir = crate::test_support::scratch()?;
        let actions = Arc::new(PayloadActions::default());
        let core = payload_core(dir.path(), actions.clone()).await?;
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        submit_payload(&core, &value).await?;
        let (source, _source_connection) = connect_clipboard_peer(&core, 2).await?;
        let (other, _other_connection) = connect_clipboard_peer(&core, 3).await?;
        core.set_setting("clipboard_mode".into(), "manual".into())
            .await?;
        // The watcher invalidates the cached selection when its connection dies.
        let (_, token) = core.observe_watched_clipboard_kind(false, true, true);
        core.clipboard_test_read(token, None).await;
        core.clipboard_test_idle().await;
        assert!(
            core.share_clipboard(crate::outbox::Targets::All("all".into()))
                .await
                .is_err()
        );
        let before = core.store.clipboard_snapshot().await?.0;
        let (offer, token) = core.observe_watched_clipboard_kind(false, false, true);
        assert!(offer.is_none());
        core.clipboard_test_read(token, Some(value)).await;
        core.clipboard_test_idle().await;
        assert_eq!(core.store.clipboard_snapshot().await?.0, before);
        assert!(core.automatic.lock().unwrap().selections.is_empty());
        assert!(
            core.share_clipboard(crate::outbox::Targets::All("all".into()))
                .await
                .is_ok()
        );
        core.clipboard_test_idle().await;
        {
            let automatic = core.automatic.lock().unwrap();
            assert!(!automatic.selections.contains_key(&source));
            assert!(automatic.selections.contains_key(&other));
        }
        assert_eq!(actions.installed.lock().unwrap().len(), 1);
        core.shutdown_actions().await?;
    }
    Ok(())
}

#[tokio::test]
async fn clipboard_mode_driver_matrix_respects_pair_direction_and_never_queues() -> Result<()> {
    use crate::core::{CloseReason, Session};
    for mode in [
        ClipboardMode::Automatic,
        ClipboardMode::Manual,
        ClipboardMode::Off,
    ] {
        for allowed in [true, false] {
            let dir = crate::test_support::scratch()?;
            let core = Core::new(
                Paths {
                    data: dir.path().join("data"),
                    cache: dir.path().join("cache"),
                    config: dir.path().join("config/config.toml"),
                    socket: dir.path().join("socket"),
                },
                Config {
                    port: 49101,
                    save_path: dir.path().join("saved"),
                    clipboard_mode: mode,
                    queue_offline: true,
                    ..Config::default()
                },
                Arc::new(Actions(0)),
            )
            .await?;
            let peer = Id::from_bytes([9; 16]);
            let fp = pneu_proto::Hash::new("9".repeat(64))?;
            core.store.pair(peer.clone(), fp.clone()).await?;
            core.admission
                .lock()
                .unwrap()
                .peers
                .insert(fp, peer.clone());
            core.store.pair_settings.lock().unwrap().insert(
                peer.clone(),
                vec![pneu_proto::PeerSetting {
                    key: format!("{}/{peer}/clipboard", core.identity.device_id),
                    enabled: allowed,
                    hlc: 1,
                    writer_device: core.identity.device_id.clone(),
                }],
            );
            let (close, _rx) = watch::channel::<Option<CloseReason>>(None);
            core.sessions.lock().unwrap().sessions.insert(
                peer.clone(),
                Session {
                    device: peer.clone(),
                    id: 1,
                    close,
                    wake: Arc::new(tokio::sync::Notify::new()),
                    paused: false,
                    capabilities: vec!["mesh".into(), "clipboard_text".into()],
                    outbound: false,
                    mesh: true,
                    role: "phone".into(),
                    phone: Default::default(),
                },
            );
            let (_, token) = core.observe_watched_clipboard(false);
            core.clipboard_test_read(
                token,
                Some(crate::ui::clipboard::Content::Text(
                    "manual candidate".into(),
                )),
            )
            .await;
            core.clipboard_test_idle().await;
            assert_eq!(
                core.automatic.lock().unwrap().selections.len(),
                usize::from(mode == ClipboardMode::Automatic && allowed)
            );
            let result = core
                .share_clipboard(crate::outbox::Targets::All("all".into()))
                .await;
            assert_eq!(result.is_ok(), mode == ClipboardMode::Manual && allowed);
            core.clipboard_test_idle().await;
            core.sessions.lock().unwrap().sessions.clear();
            core.automatic.lock().unwrap().invalidate();
            assert!(
                core.share_clipboard(crate::outbox::Targets::All("all".into()))
                    .await
                    .is_err()
            );
            assert!(core.outbox.snapshot(&[]).is_empty());
            core.shutdown_actions().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn legacy_clipboard_off_migrates_and_the_new_mode_can_enable_sharing() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let paths = Paths {
        data: dir.path().join("data"),
        cache: dir.path().join("cache"),
        config: dir.path().join("config/config.toml"),
        socket: dir.path().join("socket"),
    };
    paths.prepare()?;
    let config = Config {
        port: 49102,
        save_path: dir.path().join("saved"),
        clipboard_sync_to_phone: false,
        ..Config::default()
    };
    config.save(&paths.config)?;
    let text = std::fs::read_to_string(&paths.config)?
        .lines()
        .filter(|l| !l.starts_with("clipboard_mode"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&paths.config, text)?;
    let config = Config::load(&paths.config)?;
    assert_eq!(config.clipboard_mode, ClipboardMode::Off);
    let core = Core::new(paths, config, Arc::new(Actions(0))).await?;
    core.set_setting("clipboard_mode".into(), "manual".into())
        .await?;
    assert_eq!(core.config().clipboard_mode, ClipboardMode::Manual);
    assert!(core.config().clipboard_sync_to_phone);
    core.set_setting("clipboard_mode".into(), "off".into())
        .await?;
    assert!(!core.config().clipboard_sync_to_phone);
    core.shutdown_actions().await?;
    Ok(())
}

#[tokio::test]
async fn manual_share_verifies_file_backed_image_against_captured_representations() -> Result<()> {
    use crate::core::{CloseReason, Session};
    for edited in [false, true] {
        let dir = crate::test_support::scratch()?;
        let actions = Arc::new(PayloadActions::default());
        let core = payload_core(dir.path(), actions).await?;
        core.set_setting("clipboard_mode".into(), "manual".into())
            .await?;
        core.set_setting("clipboard_include_images".into(), true.into())
            .await?;
        let peer = Id::from_bytes([3; 16]);
        let fingerprint = pneu_proto::Hash::new("3".repeat(64))?;
        core.store.pair(peer.clone(), fingerprint.clone()).await?;
        core.admission
            .lock()
            .unwrap()
            .peers
            .insert(fingerprint, peer.clone());
        let (close, _receiver) = watch::channel::<Option<CloseReason>>(None);
        core.sessions.lock().unwrap().sessions.insert(
            peer.clone(),
            Session {
                device: peer.clone(),
                id: 1,
                close,
                wake: Arc::new(tokio::sync::Notify::new()),
                paused: false,
                capabilities: vec!["clipboard_text".into(), "clipboard_image".into()],
                outbound: false,
                mesh: false,
                role: "phone".into(),
                phone: Default::default(),
            },
        );
        let bytes = b"captured png bytes".to_vec();
        submit_payload(&core, &Content::Image(bytes.clone())).await?;
        if edited {
            for entry in std::fs::read_dir(&core.paths.cache)? {
                let path = entry?.path();
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("received-")
                {
                    std::fs::write(path, b"edited png bytes")?;
                }
            }
        }
        let result = core
            .share_clipboard(crate::outbox::Targets::All("all".into()))
            .await;
        assert_eq!(result.is_ok(), !edited);
        {
            let automatic = core.automatic.lock().unwrap();
            if edited {
                assert!(automatic.selections.is_empty());
            } else {
                let work = automatic.selections.get(&peer).unwrap();
                let crate::core::OutgoingSelection::Image(_, actual) = &work.selection else {
                    panic!("image selection")
                };
                assert_eq!(**actual, bytes);
            }
        }
        core.shutdown_actions().await?;
    }
    Ok(())
}
