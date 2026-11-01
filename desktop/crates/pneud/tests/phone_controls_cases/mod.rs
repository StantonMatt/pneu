use super::*;
use pneud::ipc::{self, Command};

async fn until(mut condition: impl FnMut() -> bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .context("phone-control state deadline")?;
    Ok(())
}
async fn connect(h: &Harness, phone: &Phone, ring: bool, mesh: bool) -> Result<PhoneTls> {
    let mut tls = raw_connect(h, phone).await?;
    let mut capabilities = vec!["clipboard_text".into()];
    if mesh {
        capabilities.push("mesh".into());
    }
    if ring {
        capabilities.push(CAP_RING.into());
    }
    write_frame(
        &mut tls,
        &Message::Hello(Hello {
            setup: None,
            role: mesh.then_some(DeviceRole::Phone),
            addresses: mesh.then(Vec::new),
            version: 1,
            device_id: phone.identity.device_id.clone(),
            name: "Fake phone".into(),
            capabilities,
            paused: Some(false),
        }),
    )
    .await?;
    let Some(Message::Hello(hello)) = read_frame(&mut tls).await? else {
        anyhow::bail!("expected HELLO");
    };
    assert!(hello.capabilities.iter().any(|c| c == CAP_BATTERY));
    assert!(hello.capabilities.iter().any(|c| c == CAP_RING));
    until(|| {
        h.core
            .connected_devices()
            .contains(&phone.identity.device_id)
    })
    .await?;
    if mesh {
        until(|| {
            h.core
                .ui_snapshot()
                .phone_controls
                .get(&phone.identity.device_id)
                .is_some_and(|p| p.connection.is_some())
        })
        .await?;
    }
    Ok(tls)
}
async fn next(tls: &mut PhoneTls) -> Result<Message> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match read_frame(tls).await?.context("peer closed")? {
                Message::PeerSettings(settings) => {
                    write_frame(
                        tls,
                        &Message::Ack(Ack {
                            transfer_id: settings.transfer_id,
                            deduplicated: false,
                        }),
                    )
                    .await?
                }
                Message::Ping(p) => write_frame(tls, &Message::Pong(p)).await?,
                message => return Ok(message),
            }
        }
    })
    .await?
}
async fn disconnect(h: &Harness, phone: &Phone, tls: PhoneTls) -> Result<()> {
    drop(tls);
    until(|| {
        !h.core
            .connected_devices()
            .contains(&phone.identity.device_id)
    })
    .await
}
#[tokio::test]
async fn phone_controls_fake_phone_two_computers_battery_ring_and_disconnect() -> Result<()> {
    let a = Harness::new().await?;
    let b = Harness::new().await?;
    let phone = a.phone("controls-phone")?;
    let first = a.pair(&phone).await?;
    drop(first);
    until(|| a.core.connected_devices().is_empty()).await?;
    let second = b.pair(&phone).await?;
    drop(second);
    until(|| b.core.connected_devices().is_empty()).await?;
    let mut at = connect(&a, &phone, true, true).await?;
    let mut bt = connect(&b, &phone, true, true).await?;
    let id = phone.identity.device_id.clone();
    let (listener, _owner) = ipc::bind(&a.core.paths.socket)?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let a_ipc = tokio::spawn(ipc::serve(a.core.clone(), listener, rx));
    let (listener, _owner_b) = ipc::bind(&b.core.paths.socket)?;
    let b_ipc = tokio::spawn(ipc::serve(b.core.clone(), listener, stop.subscribe()));
    let outcome: Result<()> = async {
        // Receiver-only battery support: the fake phone does not advertise battery_v1.
        for level in [82, 0] {
            write_frame(
                &mut at,
                &Message::BatteryState(BatteryState {
                    level: Some(level),
                    charging: Some(level == 82),
                }),
            )
            .await?;
            until(|| a.core.ui_snapshot().phone_controls[&id].battery == Some(level)).await?;
        }
        write_frame(
            &mut at,
            &Message::BatteryState(BatteryState {
                level: None,
                charging: None,
            }),
        )
        .await?;
        until(|| a.core.ui_snapshot().phone_controls[&id].battery.is_none()).await?;
        let ac = a.core.ui_snapshot().phone_controls[&id].connection.unwrap();
        let bc = b.core.ui_snapshot().phone_controls[&id].connection.unwrap();
        ipc::request(
            &a.core.paths.socket,
            Command::Ring {
                device: id.clone(),
                connection: ac,
                action: RingAction::Start,
            },
        )
        .await?;
        assert!(matches!(
            next(&mut at).await?,
            Message::Ring(Ring {
                action: RingAction::Start
            })
        ));
        // The fake phone broadcasts global state to both authenticated computers.
        for tls in [&mut at, &mut bt] {
            write_frame(tls, &Message::RingState(RingState { ringing: true })).await?;
        }
        until(|| {
            a.core.ui_snapshot().phone_controls[&id].ringing
                && b.core.ui_snapshot().phone_controls[&id].ringing
        })
        .await?;
        ipc::request(
            &b.core.paths.socket,
            Command::Ring {
                device: id.clone(),
                connection: bc,
                action: RingAction::Stop,
            },
        )
        .await?;
        assert!(matches!(
            next(&mut bt).await?,
            Message::Ring(Ring {
                action: RingAction::Stop
            })
        ));
        for tls in [&mut at, &mut bt] {
            write_frame(tls, &Message::RingState(RingState { ringing: false })).await?;
        }
        until(|| {
            !a.core.ui_snapshot().phone_controls[&id].ringing
                && !b.core.ui_snapshot().phone_controls[&id].ringing
        })
        .await?;
        write_frame(
            &mut at,
            &Message::BatteryState(BatteryState {
                level: Some(15),
                charging: Some(false),
            }),
        )
        .await?;
        until(|| a.core.ui_snapshot().phone_controls[&id].battery == Some(15)).await?;
        disconnect(&a, &phone, at).await?;
        assert!(a.core.ui_snapshot().phone_controls[&id].battery.is_none());
        assert!(!a.core.ui_snapshot().phone_controls[&id].ringing);
        assert!(
            ipc::request(
                &a.core.paths.socket,
                Command::Ring {
                    device: id.clone(),
                    connection: ac,
                    action: RingAction::Start
                }
            )
            .await
            .is_err()
        );
        let at = connect(&a, &phone, false, true).await?;
        let control = a.core.ui_snapshot().phone_controls[&id].clone();
        assert!(control.battery.is_none());
        assert!(!control.ring_supported);
        assert!(!control.ringing);
        assert!(
            ipc::request(
                &a.core.paths.socket,
                Command::Ring {
                    device: id.clone(),
                    connection: ac,
                    action: RingAction::Start
                }
            )
            .await
            .is_err()
        );
        disconnect(&a, &phone, at).await?;
        disconnect(&b, &phone, bt).await?;
        Ok(())
    }
    .await;
    stop.send_replace(true);
    a_ipc.await??;
    b_ipc.await??;
    a.stop().await?;
    b.stop().await?;
    outcome
}
#[tokio::test]
async fn phone_controls_fake_phone_legacy_and_unnegotiated_frames() -> Result<()> {
    let h = Harness::new().await?;
    let phone = h.phone("legacy-phone")?;
    let mut legacy = h.pair(&phone).await?;
    // No unsolicited new frames: ordinary legacy traffic retains its original receipt.
    let text = phone.text("legacy text".into(), ClipboardOrigin::Clipboard)?;
    let receipt = legacy.send_text(text.clone()).await?;
    assert_eq!(receipt.transfer_id, text.transfer_id);
    let expected_json = format!(
        r#"{{"transfer_id":"{}","deduplicated":false}}"#,
        text.transfer_id
    );
    let mut expected_frame = ((expected_json.len() + 1) as u32).to_be_bytes().to_vec();
    expected_frame.push(0x13);
    expected_frame.extend_from_slice(expected_json.as_bytes());
    assert_eq!(
        encode_frame(&Message::Ack(receipt))?,
        expected_frame,
        "legacy receipt bytes must remain unchanged"
    );

    assert!(h.core.ui_snapshot().phone_controls.is_empty());
    legacy.close().await?;
    drop(legacy);
    until(|| h.core.connected_devices().is_empty()).await?;
    for (ring, mesh, message) in [
        (
            false,
            false,
            Message::BatteryState(BatteryState {
                level: Some(82),
                charging: Some(true),
            }),
        ),
        (false, true, Message::RingState(RingState { ringing: true })),
        (
            true,
            true,
            Message::Ring(Ring {
                action: RingAction::Start,
            }),
        ),
    ] {
        let mut tls = connect(&h, &phone, ring, mesh).await?;
        write_frame(&mut tls, &message).await?;
        assert!(matches!(
            next(&mut tls).await?,
            Message::Error(ErrorMessage {
                code: ErrorCode::UnexpectedMessage,
                ..
            })
        ));
        disconnect(&h, &phone, tls).await?;
    }
    for payload in [
        r#"{"level":101,"charging":false}"#,
        r#"{"level":82}"#,
        r#"{"charging":null}"#,
        r#"{"ringing":1}"#,
    ] {
        use tokio::io::AsyncWriteExt;
        let mut tls = connect(&h, &phone, true, true).await?;
        let mut frame = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
        frame.push(if payload.contains("ringing") {
            0x32
        } else {
            0x30
        });
        frame.extend_from_slice(payload.as_bytes());
        tls.write_all(&frame).await?;
        assert!(matches!(
            next(&mut tls).await?,
            Message::Error(ErrorMessage {
                code: ErrorCode::MalformedMessage,
                ..
            })
        ));
        disconnect(&h, &phone, tls).await?;
    }
    h.stop().await
}
