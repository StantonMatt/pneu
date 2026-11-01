#[tokio::test]
async fn phone_controls_timeout_banner_preserves_visible_page_and_does_not_activate() -> Result<()>
{
    if in_offscreen_child(
        "ui::window::tests::phone_controls_timeout_banner_preserves_visible_page_and_does_not_activate",
        &[],
    ).await? {
        return Ok(());
    }
    headless()?;
    let window = PneuWindow::new()?;
    let dir = tempfile::tempdir()?;
    let (mut worker, mut actions) = model_worker(&window, dir.path());
    state(
        &mut worker,
        vec![device(1, "Galaxy S25+", true, "connected", 0)],
        vec![],
    );
    window.show()?;
    window.invoke_go(Page::Settings);
    drain(&mut worker, &mut actions).await;
    let visible = AtomicBool::new(true);
    let before = worker.view();
    let request = worker.request;
    let mut status = status_value(&worker);
    status["window_request"] = json!(request);
    status["snapshot"]["ring_failure"] = json!({
        "request": request, "revision": 1, "message": "Galaxy S25+ didn't respond"
    });
    worker.refresh(status.clone(), &visible).await?;
    flush_ui();
    assert_eq!(
        window.global::<Ui>().get_error().as_str(),
        "Galaxy S25+ didn't respond"
    );
    assert_eq!(worker.request, request);
    assert_eq!(worker.view(), before);
    assert_eq!(window.global::<Ui>().get_page(), Page::Settings);
    assert!(window.window().is_visible());
    assert!(
        actions.try_recv().is_err(),
        "banner must not request navigation or activation"
    );
    assert!(worker.send_request_id.is_none());
    assert!(worker.handled_send.is_none());

    worker.act(Action::Navigated).await;
    worker.refresh(status.clone(), &visible).await?;
    flush_ui();
    assert!(
        window.global::<Ui>().get_error().is_empty(),
        "dismissed banners must not replay"
    );
    status["snapshot"]["ring_failure"]["revision"] = json!(2);
    worker.refresh(status.clone(), &visible).await?;
    flush_ui();
    assert_eq!(
        window.global::<Ui>().get_error().as_str(),
        "Galaxy S25+ didn't respond"
    );

    // A delayed status reply after closing must neither reopen the window nor
    // bring the old banner back when it is next shown.
    worker.act(Action::Navigated).await;
    visible.store(false, Ordering::SeqCst);
    window.hide()?;
    status["snapshot"]["ring_failure"]["revision"] = json!(3);
    worker.refresh(status.clone(), &visible).await?;
    flush_ui();
    assert!(!window.window().is_visible());
    assert!(worker.action_error.is_empty());
    visible.store(true, Ordering::SeqCst);
    worker.refresh(status.clone(), &visible).await?;
    flush_ui();
    assert!(window.global::<Ui>().get_error().is_empty());
    status["snapshot"]["ring_failure"]["request"] = json!(request.wrapping_add(2));
    status["snapshot"]["ring_failure"]["revision"] = json!(4);
    worker.refresh(status, &visible).await?;
    assert!(
        worker.action_error.is_empty(),
        "banner belongs to its original window request"
    );
    Ok(())
}

#[test]
fn phone_controls_rows_pages_and_trays_follow_live_state() {
    use super::super::{PhoneControl, tray_model};
    let mut snapshot = snapshot(vec![device(1, "Galaxy S25+", true, "connected", 0)]);
    snapshot.phone_controls.insert(
        id(1),
        PhoneControl {
            battery: Some(82),
            charging: false,
            ring_supported: true,
            ringing: false,
            connection: Some(42),
        },
    );
    let projected = peers(&snapshot, &[]);
    assert_eq!(projected[0].item().battery, 82);
    assert!(!projected[0].item().charging);
    assert_eq!(device_summary(&projected[0]).0.detail, "Wi-Fi");
    assert_eq!(
        tray_model::battery_lines(&projected),
        ["Galaxy S25+ · Battery 82%"]
    );
    assert_eq!(projected[0].short(), "Galaxy S25+ · Wi-Fi · Battery 82%");
    let rings = tray_model::ring_items(&projected);
    assert_eq!(rings[0].label, "Ring Phone");
    assert_eq!(rings[0].connection, Some(42));
    assert_eq!(rings[0].action, pneu_proto::RingAction::Start);
    let control = snapshot.phone_controls.get_mut(&id(1)).unwrap();
    control.battery = Some(12);
    control.charging = true;
    control.ringing = true;
    let projected = peers(&snapshot, &[]);
    assert_eq!(projected[0].item().battery, 12);
    assert!(projected[0].item().charging);
    assert!(projected[0].item().ringing);
    assert_eq!(
        tray_model::battery_lines(&projected),
        ["Galaxy S25+ · Battery 12%, charging"]
    );
    let rings = tray_model::ring_items(&projected);
    assert_eq!(rings[0].label, "Stop Ringing");
    assert_eq!(rings[0].action, pneu_proto::RingAction::Stop);
    let entries = tray_model::menu(&snapshot, &Config::default());
    assert!(
        matches!(&entries[1], tray_model::MenuEntry::Item { enabled:false, command:None, label, .. } if label == "Galaxy S25+ · Battery 12%, charging")
    );
    assert!(entries.iter().any(|e| matches!(e, tray_model::MenuEntry::Item { label, enabled:true, command: Some(super::super::Command::Ring { connection:42, action:pneu_proto::RingAction::Stop, .. }), .. } if label == "Stop Ringing")));
    snapshot.paused_devices.push(id(1));
    let projected = peers(&snapshot, &[]);
    assert_eq!(projected[0].item().battery, -1);
    assert!(!projected[0].item().ringing);
    assert_eq!(tray_model::ring_items(&projected)[0].connection, None);
    snapshot.paused_devices.clear();
    snapshot.connected_devices.clear();
    // Even a cosmetic reconnect grace must not revive ephemeral samples.
    snapshot.devices[0].state = "disconnected".into();
    snapshot.refreshing_devices.push(id(1));
    let projected = peers(&snapshot, &[]);
    assert_eq!(projected[0].item().battery, -1);
    assert!(!projected[0].item().ringing);
    assert!(tray_model::battery_lines(&projected).is_empty());
    assert_eq!(tray_model::ring_items(&projected)[0].connection, None);
    snapshot
        .phone_controls
        .get_mut(&id(1))
        .unwrap()
        .ring_supported = false;
    assert!(tray_model::ring_items(&peers(&snapshot, &[])).is_empty());
    // Computers never expose phone controls, even if stale/malformed UI data contains them.
    snapshot.devices[0].role = "computer".into();
    assert_eq!(peers(&snapshot, &[])[0].item().battery, -1);
    assert!(!peers(&snapshot, &[])[0].item().ring_supported);
}
#[test]
fn phone_controls_multi_phone_tray_labels_and_truncation() {
    use super::super::{PhoneControl, tray_model};
    let mut snapshot = snapshot(vec![
        device(1, "First phone", true, "connected", 0),
        device(2, &"長".repeat(50), true, "connected", 0),
    ]);
    for (n, ringing) in [(1, false), (2, true)] {
        snapshot.phone_controls.insert(
            id(n),
            PhoneControl {
                battery: Some(50),
                ring_supported: true,
                ringing,
                connection: Some(u64::from(n)),
                ..Default::default()
            },
        );
    }
    let projected = peers(&snapshot, &[]);
    let rings = tray_model::ring_items(&projected);
    assert_eq!(rings[0].label, "First phone");
    assert!(rings[1].label.starts_with("Stop Ringing "));
    assert_eq!(
        rings[1]
            .label
            .trim_start_matches("Stop Ringing ")
            .chars()
            .count(),
        40
    );
    assert_eq!(tray_model::phone_name(&"長".repeat(40)).chars().count(), 40);
    let entries = tray_model::menu(&snapshot, &Config::default());
    assert!(entries.iter().any(|e| matches!(e, tray_model::MenuEntry::Submenu {label, items} if label == "Ring Phone" && items.len() == 2)));
}
