fn phone_control_hello(device: &Id, ring: bool) -> Hello {
    Hello {
        version: 1,
        device_id: device.clone(),
        name: "Galaxy S25+".into(),
        role: Some(DeviceRole::Phone),
        addresses: Some(vec![]),
        paused: Some(false),
        setup: None,
        capabilities: if ring {
            vec!["mesh".into(), CAP_RING.into()]
        } else {
            vec!["mesh".into()]
        },
    }
}
type PhoneControlLink = (
    Store,
    Hello,
    mpsc::Receiver<Message>,
    watch::Receiver<Option<CloseReason>>,
);
fn phone_control_link(
    core: &Core,
    device: &Id,
    connection: u64,
    ring: bool,
) -> Result<PhoneControlLink> {
    let close = admit_ui_test_phone(core, device, connection)?;
    let hello = phone_control_hello(device, ring);
    {
        let mut sessions = core.sessions.lock().unwrap();
        let session = sessions.sessions.get_mut(device).unwrap();
        session.mesh = true;
        session.capabilities = hello.capabilities.clone();
    }
    let mut store = core.store.with_current_pairing();
    store.connection_id = Some(connection);
    store.mesh = true;
    let (tx, rx) = mpsc::channel(32);
    core.attach_phone_control(&hello, &store, tx);
    Ok((store, hello, rx, close))
}
#[tokio::test(start_paused = true)]
async fn phone_controls_battery_clear_pause_expiry_reconnect_and_stale_session() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let (store, hello, _rx, close) = phone_control_link(&core, &device, 1, true)?;
    let sample = BatteryState {
        level: Some(82),
        charging: Some(true),
    };
    core.receive_phone_battery(&store, &hello, sample.clone());
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, Some(82));
    assert!(core.ui_snapshot().phone_controls[&device].charging);
    core.persist_phone(1).await?;
    for filename in ["phone.json", "phones.json"] {
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(core.paths.data.join(filename))?)?;
        assert!(!metadata.to_string().contains("battery"));
        assert!(!metadata.to_string().contains("ringing"));
    }

    core.receive_phone_battery(
        &store,
        &hello,
        BatteryState {
            level: None,
            charging: None,
        },
    );
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, None);
    core.receive_phone_battery(&store, &hello, sample.clone());
    tokio::time::advance(Duration::from_secs(599)).await;
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, Some(82));
    tokio::time::advance(Duration::from_secs(1)).await;
    core.expire_phone_control(&hello, &store).await;
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, None);
    core.receive_phone_battery(&store, &hello, sample.clone());
    core.set_phone_paused(&device, 1, true);
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, None);
    core.receive_phone_battery(&store, &hello, sample.clone());
    core.set_phone_paused(&device, 1, false);
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, None);
    core.receive_phone_battery(&store, &hello, sample.clone());
    assert!(core.finish_connection(&device, 1));
    let offline = core.ui_snapshot().phone_controls[&device].clone();
    assert_eq!(offline.battery, None);
    assert!(offline.ring_supported);
    assert!(offline.connection.is_none());
    let (next_store, next_hello, _next_rx, _next_close) =
        phone_control_link(&core, &device, 2, false)?;
    assert!(!core.ui_snapshot().phone_controls[&device].ring_supported);
    core.receive_phone_battery(&store, &hello, sample.clone());
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, None);
    core.receive_phone_battery(
        &next_store,
        &next_hello,
        BatteryState {
            level: Some(0),
            charging: Some(false),
        },
    );
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, Some(0));
    assert!(!core.finish_connection(&device, 1));
    assert_eq!(core.ui_snapshot().phone_controls[&device].battery, Some(0));
    drop(close);
    core.finish_connection(&device, 2);
    core.unpair_device(device.clone()).await?;
    assert!(!core.ui_snapshot().phone_controls.contains_key(&device));
    core.shutdown_actions().await
}
#[tokio::test(start_paused = true)]
async fn phone_controls_ring_optimistic_response_timeout_and_no_replay() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let (store, hello, mut rx, _close) = phone_control_link(&core, &device, 1, true)?;
    core.ring_phone(device.clone(), 1, RingAction::Start)?;
    assert!(matches!(
        rx.try_recv()?,
        Message::Ring(Ring {
            action: RingAction::Start
        })
    ));
    assert!(core.ui_snapshot().phone_controls[&device].ringing);
    tokio::time::advance(Duration::from_secs(5)).await;
    core.set_window_visible(core.window_request(), true);
    core.expire_phone_control(&hello, &store).await;
    assert!(!core.ui_snapshot().phone_controls[&device].ringing);
    assert_eq!(
        core.ui_snapshot()
            .ring_failure
            .as_ref()
            .map(|f| f.message.as_str()),
        Some("Galaxy S25+ didn't respond")
    );
    assert!(core.pending_send().is_none());
    assert_eq!(
        core.window_request(),
        0,
        "a timeout must not activate or navigate the window"
    );
    core.ring_phone(device.clone(), 1, RingAction::Start)?;
    rx.try_recv()?;
    core.receive_phone_ring(&store, &hello, RingState { ringing: false });
    assert!(!core.ui_snapshot().phone_controls[&device].ringing);
    // State sent after another computer starts/stops is reflected here too.
    core.receive_phone_ring(&store, &hello, RingState { ringing: true });
    assert!(core.ui_snapshot().phone_controls[&device].ringing);
    core.ring_phone(device.clone(), 1, RingAction::Stop)?;
    assert!(matches!(
        rx.try_recv()?,
        Message::Ring(Ring {
            action: RingAction::Stop
        })
    ));
    core.receive_phone_ring(&store, &hello, RingState { ringing: false });
    core.finish_connection(&device, 1);
    assert!(
        core.ring_phone(device.clone(), 1, RingAction::Start)
            .is_err()
    );
    let (next_store, next_hello, mut next_rx, _next_close) =
        phone_control_link(&core, &device, 2, true)?;
    assert!(next_rx.try_recv().is_err());
    assert!(!core.ui_snapshot().phone_controls[&device].ringing);
    assert!(
        core.ring_phone(device.clone(), 1, RingAction::Start)
            .is_err()
    );
    core.receive_phone_ring(&store, &hello, RingState { ringing: true });
    assert!(!core.ui_snapshot().phone_controls[&device].ringing);
    core.receive_phone_ring(&next_store, &next_hello, RingState { ringing: true });
    assert!(core.ui_snapshot().phone_controls[&device].ringing);
    core.finish_connection(&device, 2);
    core.shutdown_actions().await
}
#[test]
fn phone_controls_capability_and_direction_gates() {
    let device = Id::from_bytes([1; 16]);
    let mut hello = phone_control_hello(&device, false);
    assert!(phone_controls::eligible(&hello, CAP_BATTERY)); // receiver advertises battery
    assert!(!phone_controls::eligible(&hello, CAP_RING));
    hello.capabilities.push(CAP_RING.into());
    assert!(phone_controls::eligible(&hello, CAP_RING));
    hello.role = Some(DeviceRole::Computer);
    assert!(!phone_controls::eligible(&hello, CAP_BATTERY));
    assert!(!phone_controls::eligible(&hello, CAP_RING));
    hello.role = None;
    assert!(!phone_controls::eligible(&hello, CAP_RING));
    hello.role = Some(DeviceRole::Phone);
    hello.capabilities.retain(|c| c != "mesh");
    assert!(!phone_controls::eligible(&hello, CAP_BATTERY));
}

#[tokio::test(start_paused = true)]
async fn phone_controls_ring_timeout_with_closed_window_posts_notice() -> Result<()> {
    #[derive(Default)]
    struct RingNotices(Mutex<Vec<String>>);
    #[async_trait::async_trait]
    impl PcActions for RingNotices {
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
        async fn notify_ring_failure(&self, name: &str) -> Result<()> {
            self.0.lock().unwrap().push(name.into());
            Ok(())
        }
    }
    let notices = Arc::new(RingNotices::default());
    let (_dir, core, device) = fixture(notices.clone()).await?;
    let (store, hello, mut rx, _close) = phone_control_link(&core, &device, 1, true)?;
    core.set_window_visible(core.window_request(), true);
    core.ring_phone(device.clone(), 1, RingAction::Start)?;
    rx.try_recv()?;
    core.set_window_visible(core.window_request(), false);
    tokio::time::advance(Duration::from_secs(5)).await;
    core.expire_phone_control(&hello, &store).await;
    assert_eq!(*notices.0.lock().unwrap(), ["Galaxy S25+"]);
    assert!(core.pending_send().is_none());
    assert_eq!(
        core.window_request(),
        0,
        "a timeout must not reopen a closed window"
    );
    assert!(!core.ui_snapshot().phone_controls[&device].ringing);
    core.finish_connection(&device, 1);
    core.shutdown_actions().await
}

#[tokio::test]
async fn phone_controls_ring_rejects_unpairing_at_admission() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let (_store, _hello, mut rx, _close) = phone_control_link(&core, &device, 1, true)?;
    core.sessions
        .lock()
        .unwrap()
        .unpairing
        .insert(device.clone());
    for action in [RingAction::Start, RingAction::Stop] {
        assert!(core.ring_phone(device.clone(), 1, action).is_err());
        assert!(rx.try_recv().is_err());
    }
    assert!(!core.ui_snapshot().phone_controls[&device].ringing);
    core.shutdown_actions().await
}

#[tokio::test]
async fn phone_controls_ring_writer_rechecks_queued_and_pending_authority() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Exercise the same queue drain used between file/image frames, both before
    // polling the socket and after it returned Pending without accepting a byte.
    for pending in [false, true] {
        for invalidation in ["unpairing", "binding", "connection", "paused", "capability"] {
            let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
            let (store, hello, rx, _close) = phone_control_link(&core, &device, 1, true)?;
            let mut normal = phone_controls::FrameQueue::new(rx, &hello, &store);
            core.ring_phone(device.clone(), 1, RingAction::Start)?;
            let (_priority, mut priority) = mpsc::channel(1);
            let (mut stream, mut reader) = tokio::io::duplex(1);
            stream.write_all(&[42]).await?;
            let mut writer = ObservedWriter {
                inner: stream,
                polled: Arc::new(Notify::new()),
            };
            let mut send = Box::pin(send_pending_messages(
                &core,
                &mut writer,
                &mut priority,
                &mut normal,
                None,
            ));
            if pending {
                assert!(futures_util::poll!(send.as_mut()).is_pending());
            }
            match invalidation {
                "unpairing" => {
                    core.sessions
                        .lock()
                        .unwrap()
                        .unpairing
                        .insert(device.clone());
                }
                "binding" => {
                    core.store.unpair_then(device.clone(), |_| {}).await?;
                }
                "connection" => {
                    core.sessions
                        .lock()
                        .unwrap()
                        .sessions
                        .get_mut(&device)
                        .unwrap()
                        .id = 2;
                }
                "paused" => {
                    core.sessions
                        .lock()
                        .unwrap()
                        .sessions
                        .get_mut(&device)
                        .unwrap()
                        .paused = true;
                }
                "capability" => {
                    core.sessions
                        .lock()
                        .unwrap()
                        .sessions
                        .get_mut(&device)
                        .unwrap()
                        .capabilities
                        .retain(|c| c != CAP_RING);
                }
                _ => unreachable!(),
            }
            assert_eq!(reader.read_u8().await?, 42);
            send.await?;
            drop(writer);
            assert_eq!(
                reader.read_u8().await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof,
                "pending={pending}, invalidation={invalidation}: stale RING wrote bytes"
            );
            core.shutdown_actions().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn phone_controls_ring_writer_drops_queued_ring_and_keeps_stream_usable() -> Result<()> {
    let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
    let (store, hello, mut rx, close) = phone_control_link(&core, &device, 1, true)?;
    core.ring_phone(device.clone(), 1, RingAction::Start)?;
    let (tx, messages) = mpsc::channel(2);
    tx.send(rx.try_recv()?).await?;
    let ping = Message::Ping(Ping {
        nonce: Nonce::new("0123456789abcdef")?,
    });
    tx.send(ping.clone()).await?;
    drop(tx);
    let (_priority, priority) = mpsc::channel(1);
    let (_automatic, automatic) = mpsc::channel(1);
    let (_text, text) = mpsc::channel(1);
    let (_files, files) = mpsc::channel(1);
    let (reports, _reported) = mpsc::channel(1);
    let mut bytes = Vec::new();
    let removing = core.clone();
    writer_loop(
        core.clone(),
        &mut bytes,
        WriterQueues {
            normal: phone_controls::FrameQueue::new(messages, &hello, &store),
            priority,
            automatic,
            text,
            files,
            reports,
            close,
            before_queue_poll: Some(Box::new(move || {
                removing.sessions.lock().unwrap().unpairing.insert(device);
            })),
        },
    )
    .await?;
    let mut wire = bytes.as_slice();
    assert_eq!(read_frame(&mut wire).await?, Some(ping));
    assert!(read_frame(&mut wire).await?.is_none());
    core.shutdown_actions().await
}

#[tokio::test]
async fn phone_controls_ring_writer_finishes_started_frame_and_allows_paused_stop() -> Result<()> {
    for action in [RingAction::Start, RingAction::Stop] {
        let (_dir, core, device) = fixture(Arc::new(NoActions)).await?;
        let (store, hello, rx, _close) = phone_control_link(&core, &device, 1, true)?;
        if action == RingAction::Stop {
            core.sessions
                .lock()
                .unwrap()
                .sessions
                .get_mut(&device)
                .unwrap()
                .paused = true;
        }
        core.ring_phone(device.clone(), 1, action)?;
        let mut queue = phone_controls::FrameQueue::new(rx, &hello, &store);
        let message = queue.recv().await.unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let mut send = Box::pin(queue.send(&core, &mut writer, &message));
        assert!(futures_util::poll!(send.as_mut()).is_pending()); // first byte accepted
        core.sessions
            .lock()
            .unwrap()
            .unpairing
            .insert(device.clone());
        let (sent, frame) = tokio::join!(send, read_frame(&mut reader));
        sent?;
        assert_eq!(frame?, Some(Message::Ring(Ring { action })));
        core.shutdown_actions().await?;
    }
    Ok(())
}
