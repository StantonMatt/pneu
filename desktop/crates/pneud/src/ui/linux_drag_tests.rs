//! Opt-in native test seam: an external process must drag into the real window
//! on a private compositor. No daemon or owner device is contacted.
use super::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[test]
#[ignore = "requires a private Wayland compositor and an external drag source"]
fn external_drop_reaches_shared_action() -> Result<()> {
    let control =
        PathBuf::from(std::env::var_os("PNEU_DROP_CONTROL").context("private test control path")?);
    let expected = std::env::var("PNEU_DROP_EXPECT").context("drop expectation")?;
    let window = PneuWindow::new()?;
    let ui = window.global::<Ui>();
    ui.set_page(Page::Main);
    ui.set_devices(ModelRc::new(VecModel::from(vec![
        DeviceItem {
            id: "11".repeat(16).into(),
            name: "Scratch online".into(),
            connected: true,
            ..Default::default()
        },
        DeviceItem {
            id: "33".repeat(16).into(),
            name: "Scratch offline".into(),
            connected: false,
            ..Default::default()
        },
    ])));
    let frame = View {
        targets: Some(vec![
            SendTarget {
                device: "11".repeat(16),
                name: "Scratch online".into(),
                eligible: true,
                generation: 7,
                fingerprint: Some("22".repeat(32)),
                ..Default::default()
            },
            SendTarget {
                device: "33".repeat(16),
                name: "Scratch offline".into(),
                eligible: true,
                would_queue: true,
                generation: 9,
                fingerprint: Some("44".repeat(32)),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let view = Arc::new(Mutex::new(frame.clone()));
    let (commands, mut rx) = tokio::sync::mpsc::unbounded_channel();
    install_drop(&window, &view, &commands);
    assert!(ui.get_drop_supported());
    window.show()?;
    let weak = window.as_weak();
    let observed = Rc::new(RefCell::new(Vec::new()));
    let received = observed.clone();
    let last = RefCell::new(String::new());
    let started = Instant::now();
    let timed_out = Rc::new(Cell::new(false));
    let timeout = timed_out.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(20),
        move || {
            let w = weak.upgrade().unwrap();
            let ui = w.global::<Ui>();
            let g = w.get_drop_geometry();
            let state = json!({"active":ui.get_drop_active(), "hover":ui.get_drop_hover(),
            "geometry":{"x":g.x,"y":g.y,"width":g.width,"height":g.height,"scroll":g.scroll},
            "scale":w.window().scale_factor(), "qt_scale":super::super::linux_drag::device_pixel_ratio(&w)})
            .to_string();
            if *last.borrow() != state {
                println!("DROP_STATE {state}");
                *last.borrow_mut() = state;
            }
            while let Ok(action) = rx.try_recv() {
                let Action::DropFiles {
                    files,
                    targets,
                    origin,
                    targeted,
                } = action
                else {
                    panic!("unexpected action")
                };
                assert_eq!(origin, frame);
                println!(
                    "DROP_ACTION {}",
                    json!({"files":files,"targets":targets,"targeted":targeted})
                );
                received.borrow_mut().push((files, targets, targeted));
            }
            match std::fs::read_to_string(&control)
                .unwrap_or_default()
                .as_str()
            {
                "disconnect" => {
                    let mut peers = ui.get_devices().iter().collect::<Vec<_>>();
                    peers[0].connected = false;
                    ui.set_devices(ModelRc::new(VecModel::from(peers)));
                }
                "navigate" => view.lock().unwrap().revision = 1,
                "quit" => {
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
            if started.elapsed() > Duration::from_secs(30) {
                timeout.set(true);
                slint::quit_event_loop().unwrap();
            }
        },
    );
    println!("DROP_READY");
    slint::run_event_loop_until_quit()?;
    timer.stop();
    super::super::linux_drag::cleanup();
    assert!(!timed_out.get(), "external driver timed out");
    assert!(!ui.get_drop_active());
    assert_eq!(ui.get_drop_hover(), -1);
    let actions = observed.borrow();
    if expected == "none" {
        assert!(actions.is_empty());
    } else {
        assert_eq!(actions.len(), 1);
        let (files, targets, targeted) = &actions[0];
        let mut actual = files.clone();
        let mut expected_files = vec![
            PathBuf::from(std::env::var_os("DROP_FILE").unwrap()),
            PathBuf::from(std::env::var_os("DROP_FILE2").unwrap()),
        ];
        actual.sort();
        expected_files.sort();
        assert_eq!(actual, expected_files);
        assert_eq!(*targeted, expected == "row");
        if expected == "outside" {
            assert!(targets.is_empty());
        } else {
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].device, "11".repeat(16));
            assert_eq!(targets[0].generation, 7);
            assert_eq!(targets[0].fingerprint, "22".repeat(32));
        }
    }
    Ok(())
}
