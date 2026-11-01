//! Shared menu-bar/tray state projection.
use super::{
    Snapshot,
    window::{Peer, contract::Transfer, display_peer_text},
};
/// The tray's first line, which also starts its tooltip.
pub(crate) fn status(snapshot: &Snapshot, peers: &[Peer], transfers: &[Transfer]) -> String {
    let sending: Vec<&str> = peers
        .iter()
        .filter(|p| p.sending())
        .map(|p| p.name.as_str())
        .collect();
    let receiving = peers
        .iter()
        .find(|p| p.receiving())
        .map(|p| p.name.clone())
        .or_else(|| {
            (snapshot.receiving && snapshot.connected)
                .then(|| display_peer_text(&snapshot.phone.name))
        });
    let connected: Vec<&Peer> = peers.iter().filter(|p| p.connected()).collect();
    if let Some(error) = &snapshot.error {
        error.tooltip()
    } else if let Some(name) = &snapshot.pairing {
        format!("Pairing with {}…", display_peer_text(name))
    } else if peers.is_empty() {
        "No devices paired".into()
    } else if let [one] = sending.as_slice() {
        format!("Sending to {one}…")
    } else if !sending.is_empty() {
        format!("Sending to {} devices…", sending.len())
    } else if let Some(name) = receiving {
        format!("Receiving from {name}…")
    } else if let [one] = peers {
        // One device keeps v2's wording.
        if one.paused() {
            format!("{} paused sending", one.name)
        } else if let (true, Some(route)) = (one.connected(), &one.route) {
            format!(
                "{} connected over {}",
                one.name,
                super::route_in_sentence(route)
            )
        } else if one.connected() {
            format!("{} connected", one.name)
        } else if one.lost() {
            format!(
                "{} doesn't recognize {}",
                one.name,
                super::window::this_computer()
            )
        } else {
            format!("{} isn't connected", one.name)
        }
    } else if connected.len() == 2 && peers.len() == 2 {
        format!("{} and {} connected", connected[0].name, connected[1].name)
    } else if connected.is_empty() {
        let _ = transfers;
        "No devices connected".into()
    } else {
        format!("{} of {} devices connected", connected.len(), peers.len())
    }
}
/// A single device has one tooltip line, with active status taking precedence.
pub(crate) fn tooltip_lines(
    snapshot: &Snapshot,
    peers: &[Peer],
    transfers: &[Transfer],
) -> Vec<String> {
    let leads = snapshot.error.is_some()
        || snapshot.pairing.is_some()
        || peers.is_empty()
        || peers.iter().any(|p| p.sending() || p.receiving())
        || (snapshot.receiving && snapshot.connected);
    if leads && peers.len() <= 1 {
        return vec![status(snapshot, peers, transfers)];
    }
    let mut lines: Vec<_> = peers.iter().map(Peer::short).collect();
    if leads {
        lines.insert(0, status(snapshot, peers, transfers));
    }
    lines
}
pub(crate) fn icon(snapshot: &Snapshot, peers: &[Peer]) -> &'static str {
    if snapshot.error.is_some() {
        "pneu-tray-error"
    } else if peers.iter().any(Peer::paused) {
        "pneu-tray-paused"
    } else if peers.iter().any(|p| p.sending() || p.receiving())
        || (snapshot.receiving && snapshot.connected)
    {
        "pneu-tray-receiving"
    } else if peers.iter().any(Peer::connected) {
        "pneu-tray-connected"
    } else {
        "pneu-tray-disconnected"
    }
}

#[cfg(any(target_os = "linux", windows, test))]
use anyhow::Result;
#[cfg(any(target_os = "linux", windows, test))]
pub const ICONS: [(&str, &str); 5] = [
    (
        "pneu-tray-connected",
        include_str!("../../../../assets/pneu-tray-connected.svg"),
    ),
    (
        "pneu-tray-receiving",
        include_str!("../../../../assets/pneu-tray-receiving.svg"),
    ),
    (
        "pneu-tray-disconnected",
        include_str!("../../../../assets/pneu-tray-disconnected.svg"),
    ),
    (
        "pneu-tray-paused",
        include_str!("../../../../assets/pneu-tray-paused.svg"),
    ),
    (
        "pneu-tray-error",
        include_str!("../../../../assets/pneu-tray-error.svg"),
    ),
];
// Scan only the original SVG: a palette color may itself equal another source
// token, and must never be substituted a second time. Fixed receiving indigo
// and all other SVG content pass through unchanged.
#[cfg(any(target_os = "linux", windows, test))]
pub(super) fn recolor_svg(svg: &str, foreground: &str, negative: &str, neutral: &str) -> String {
    let replacements = [
        ("#232629", foreground),
        ("#da4453", negative),
        ("#f67400", neutral),
    ];
    let mut output = String::with_capacity(svg.len());
    let mut remaining = svg;
    while let Some(index) = remaining.find('#') {
        output.push_str(&remaining[..index]);
        remaining = &remaining[index..];
        if let Some((color, suffix)) = replacements
            .iter()
            .find_map(|(token, color)| remaining.strip_prefix(token).map(|suffix| (*color, suffix)))
        {
            output.push_str(color);
            remaining = suffix;
        } else {
            output.push('#');
            remaining = &remaining[1..];
        }
    }
    output.push_str(remaining);
    output
}
#[cfg(any(target_os = "linux", windows, test))]
pub fn render_svg(svg: &[u8], size: u32) -> Result<Vec<u8>> {
    let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)
        .ok_or_else(|| anyhow::anyhow!("invalid icon size"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(
            size as f32 / tree.size().width(),
            size as f32 / tree.size().height(),
        ),
        &mut pixmap.as_mut(),
    );
    // tiny-skia pixels are premultiplied. SNI and Slint take straight-alpha bytes.
    for pixel in pixmap.data_mut().chunks_exact_mut(4) {
        if pixel[3] > 0 {
            for i in 0..3 {
                pixel[i] = (u32::from(pixel[i]) * 255 / u32::from(pixel[3])).min(255) as u8;
            }
        }
    }
    Ok(pixmap.take())
}

pub(crate) fn history_label(item: &crate::store::history::Item, kde: bool) -> String {
    let label = if item.image {
        format!("Image · {}", super::last_seen(item.at))
    } else {
        item.title.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let label = if label.chars().count() > 40 {
        format!("{}…", label.chars().take(39).collect::<String>())
    } else {
        label
    };
    if kde { label.replace('_', "__") } else { label }
}
#[cfg(target_os = "linux")]
pub(crate) fn history_icon(item: &crate::store::history::Item) -> Vec<u8> {
    let Some(path) = item.thumbnail.as_ref() else {
        return Vec::new();
    };
    (|| -> Result<Vec<u8>> {
        let bytes = crate::storage::read_bounded(path, 128 * 1024)?;
        let image = image::load_from_memory(&bytes)?.resize_exact(
            16,
            16,
            image::imageops::FilterType::Triangle,
        );
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png)?;
        Ok(png.into_inner())
    })()
    .unwrap_or_default()
}

// The native Windows menu uses the same presentation projection and captured
// authorities as ksni. Native menu IDs are never reused across rebuilds.
#[cfg(any(windows, test))]
#[derive(Debug)]
pub(super) enum MenuEntry {
    Separator,
    Item {
        label: String,
        enabled: bool,
        checked: Option<bool>,
        command: Option<super::Command>,
    },
    Submenu {
        label: String,
        items: Vec<MenuEntry>,
    },
}
#[cfg(any(windows, test))]
pub(super) fn menu(snapshot: &Snapshot, config: &crate::Config) -> Vec<MenuEntry> {
    use super::{
        Command, Toggle,
        window::{ClipboardMode, peers},
    };
    use MenuEntry::*;
    fn item(label: impl Into<String>, command: Command) -> MenuEntry {
        Item {
            label: label.into(),
            enabled: true,
            checked: None,
            command: Some(command),
        }
    }
    fn toggle(toggle: Toggle, config: &crate::Config, enabled: bool) -> MenuEntry {
        Item {
            label: toggle.label().into(),
            enabled,
            checked: Some(toggle.checked(config)),
            command: Some(Command::Toggle(toggle)),
        }
    }
    fn history(snapshot: &Snapshot) -> MenuEntry {
        let mut items: Vec<_> = snapshot
            .clipboard_history
            .iter()
            .take(10)
            .map(|i| {
                item(
                    history_label(i, false),
                    Command::CopyClipboardHistory(i.id.clone()),
                )
            })
            .collect();
        if items.is_empty() {
            items.push(Item {
                label: "Nothing copied yet".into(),
                enabled: false,
                checked: None,
                command: None,
            });
        } else {
            items.push(Separator);
            items.push(item("Show All", Command::ShowClipboardHistory));
        }
        Submenu {
            label: "Clipboard History".into(),
            items,
        }
    }
    let transfers = snapshot
        .transfers
        .iter()
        .map(Transfer::from)
        .collect::<Vec<_>>();
    let peers = peers(snapshot, &transfers);
    let mut items = vec![
        Item {
            label: status(snapshot, &peers, &transfers),
            enabled: false,
            checked: None,
            command: None,
        },
        Separator,
        item(
            "Open Pneu",
            Command::Window {
                activation_token: None,
            },
        ),
    ];
    if !peers.is_empty() {
        let recipients = |device: Option<&pneu_proto::Id>| {
            let ids = device.map(|id| [id.clone()]);
            crate::file_recipients::resolve(
                &snapshot.send_targets,
                match &ids {
                    Some(ids) => crate::file_recipients::Selection::Explicit(ids),
                    None => crate::file_recipients::Selection::ConnectedOnly,
                },
            )
            .expect("valid tray selection")
            .recipients
        };
        let mut targets = vec![
            item(
                "All Connected Devices…",
                Command::SendFiles {
                    recipients: recipients(None),
                    targeted: false,
                },
            ),
            Separator,
        ];
        for peer in &peers {
            if let Ok(device) = pneu_proto::Id::new(&peer.id) {
                let label = if peer.name.chars().count() > 40 {
                    format!(
                        "{}…",
                        peer.name.chars().take(39).collect::<String>().trim_end()
                    )
                } else {
                    peer.name.clone()
                };
                targets.push(item(
                    format!("{label}…"),
                    Command::SendFiles {
                        recipients: recipients(Some(&device)),
                        targeted: true,
                    },
                ));
            }
        }
        if let [one] = peers.as_slice() {
            if let Ok(device) = pneu_proto::Id::new(&one.id) {
                items.push(item(
                    "Send Files…",
                    Command::SendFiles {
                        recipients: recipients(Some(&device)),
                        targeted: true,
                    },
                ));
            }
        } else {
            items.push(Submenu {
                label: "Send Files To".into(),
                items: targets,
            });
        }
        let value = serde_json::to_value(config).unwrap_or_default();
        let mode = ClipboardMode::from_config(
            Some(&value["clipboard_mode"]).filter(|v| !v.is_null()),
            config.clipboard_sync_to_phone,
        );
        if mode == ClipboardMode::Manual {
            items.push(item(
                "Send Clipboard",
                Command::SendClipboard(super::captured_authorities(snapshot)),
            ));
        }
        if config.clipboard_history {
            items.push(history(snapshot));
        }
        items.extend([item("Open Received Files", Command::OpenFolder), Separator]);
        let mut modes = [
            ("Share Automatically", ClipboardMode::Automatic),
            ("Share Manually", ClipboardMode::Manual),
            ("Off", ClipboardMode::Off),
        ]
        .into_iter()
        .map(|(label, choice)| Item {
            label: label.into(),
            enabled: true,
            checked: Some(choice == mode),
            command: Some(Command::ClipboardMode(choice.key())),
        })
        .collect::<Vec<_>>();
        modes.extend([
            Separator,
            toggle(Toggle::IncludeImages, config, mode != ClipboardMode::Off),
        ]);
        items.push(Submenu {
            label: "Clipboard".into(),
            items: modes,
        });
        items.push(Submenu {
            label: "When Files Arrive".into(),
            items: [
                Toggle::Save,
                Toggle::Notifications,
                Toggle::CopyImages,
                Toggle::Viewer,
            ]
            .into_iter()
            .map(|t| toggle(t, config, true))
            .collect(),
        });
        items.push(Separator);
    }
    if peers.is_empty() && config.clipboard_history {
        items.push(history(snapshot));
    }
    items.extend([
        item(
            "Add Device…",
            Command::AddDevice {
                activation_token: None,
            },
        ),
        Separator,
        item("Quit Pneu", Command::Quit),
    ]);
    items
}

#[cfg(test)]
mod tests {
    use super::super::{Command, Toggle, window::peers};
    use super::*;

    #[test]
    fn one_device_tooltip_has_one_line_and_multiple_devices_keep_each_line() {
        let mut snap = Snapshot {
            devices: (1..=2)
                .map(|n| crate::core::mesh::DeviceSnapshot {
                    device: pneu_proto::Id::from_bytes([n; 16]),
                    name: format!("Device {n}"),
                    role: "computer".into(),
                    state: "connected".into(),
                    last_seen: 0,
                    dialer: None,
                    route: Some("Local network".into()),
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(tooltip_lines(&snap, &peers(&snap, &[]), &[]).len(), 2);
        snap.devices.pop();
        assert_eq!(
            tooltip_lines(&snap, &peers(&snap, &[]), &[]),
            ["Device 1 · Local network"]
        );
        snap.error = Some(super::super::ReceiveError::Port(53317));
        assert_eq!(
            tooltip_lines(&snap, &peers(&snap, &[]), &[]),
            [snap.error.as_ref().unwrap().tooltip()]
        );
    }
    #[test]
    fn menu_projects_unpaired_and_paired_settings_and_captures_authority() {
        let mut snapshot = Snapshot::default();
        let mut config = crate::Config::default();
        let entries = menu(&snapshot, &config);
        assert!(
            matches!(&entries[0], MenuEntry::Item { label, enabled: false, checked: None, command: None } if label == "No devices paired")
        );
        assert!(matches!(
            entries.last(),
            Some(MenuEntry::Item {
                command: Some(Command::Quit),
                ..
            })
        ));
        assert!(
            !entries
                .iter()
                .any(|item| matches!(item, MenuEntry::Submenu { .. }))
        );
        fn labels(entries: &[MenuEntry]) -> Vec<&str> {
            entries
                .iter()
                .map(|entry| match entry {
                    MenuEntry::Separator => "---",
                    MenuEntry::Item { label, .. } | MenuEntry::Submenu { label, .. } => {
                        label.as_str()
                    }
                })
                .collect()
        }
        assert_eq!(
            labels(&entries),
            [
                "No devices paired",
                "---",
                "Open Pneu",
                "Add Device…",
                "---",
                "Quit Pneu"
            ]
        );
        assert!(matches!(
            entries[2],
            MenuEntry::Item {
                command: Some(Command::Window { .. }),
                ..
            }
        ));
        let device = pneu_proto::Id::from_bytes([1; 16]);
        snapshot.devices.push(crate::core::mesh::DeviceSnapshot {
            device: device.clone(),
            name: "Phone & tablet".into(),
            role: "phone".into(),
            state: "connected".into(),
            last_seen: 0,
            dialer: None,
            route: None,
        });
        snapshot.send_targets.push(crate::outbox::SendTarget {
            device: device.clone(),
            name: "Phone & tablet".into(),
            generation: 7,
            fingerprint: Some(pneu_proto::Hash::new("ab".repeat(32)).unwrap()),
            eligible: true,
            would_queue: false,
            reason: None,
        });
        config.clipboard_mode = crate::config::ClipboardMode::Off;
        let entries = menu(&snapshot, &config);
        assert_eq!(
            &labels(&entries)[1..],
            [
                "---",
                "Open Pneu",
                "Send Files…",
                "Open Received Files",
                "---",
                "Clipboard",
                "When Files Arrive",
                "---",
                "Add Device…",
                "---",
                "Quit Pneu"
            ]
        );
        config.clipboard_mode = crate::config::ClipboardMode::Manual;
        assert_eq!(
            &labels(&menu(&snapshot, &config))[1..],
            [
                "---",
                "Open Pneu",
                "Send Files…",
                "Send Clipboard",
                "Open Received Files",
                "---",
                "Clipboard",
                "When Files Arrive",
                "---",
                "Add Device…",
                "---",
                "Quit Pneu"
            ]
        );
        let files = entries
            .iter()
            .find(|entry| matches!(entry, MenuEntry::Item {label,..} if label == "Send Files…"))
            .unwrap();
        assert!(
            matches!(files, MenuEntry::Item { command: Some(Command::SendFiles { recipients, targeted: true }), .. }
            if recipients.len() == 1 && recipients[0].device == device && recipients[0].generation == 7)
        );
        let clipboard = entries
            .iter()
            .find_map(|item| match item {
                MenuEntry::Submenu { label, items } if label == "Clipboard" => Some(items),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(&clipboard[2], MenuEntry::Item { label, checked: Some(true), command: Some(Command::ClipboardMode("off")), .. } if label == "Off")
        );
        assert!(matches!(
            clipboard.last(),
            Some(MenuEntry::Item {
                enabled: false,
                command: Some(Command::Toggle(Toggle::IncludeImages)),
                ..
            })
        ));
        // Menu commands retain the generation they were built for; Core checks
        // it even after a peer is removed or paired again before activation.
        snapshot.send_targets[0].generation = 8;
        assert!(
            matches!(files, MenuEntry::Item { command: Some(Command::SendFiles { recipients, .. }), .. } if recipients[0].generation == 7)
        );
        snapshot.devices[0].state = "disconnected".into();
        snapshot.send_targets[0].would_queue = true;
        assert!(menu(&snapshot, &config).iter().any(|entry| matches!(entry, MenuEntry::Item { label, command: Some(Command::SendFiles {recipients,targeted:true}),.. } if label == "Send Files…" && recipients.len() == 1)));
        let mut second = snapshot.devices[0].clone();
        second.device = pneu_proto::Id::from_bytes([2; 16]);
        second.name = "Second computer".into();
        snapshot.devices.push(second);
        assert!(menu(&snapshot, &config).iter().any(
            |entry| matches!(entry, MenuEntry::Submenu {label,..} if label == "Send Files To")
        ));
    }

    #[test]
    fn shared_icon_precedence_and_rgba_sizes() -> Result<()> {
        let mut snapshot = Snapshot::default();
        assert_eq!(icon(&snapshot, &[]), "pneu-tray-disconnected");
        snapshot.receiving = true;
        snapshot.connected = true;
        assert_eq!(icon(&snapshot, &[]), "pneu-tray-receiving");
        snapshot.error = Some(super::super::ReceiveError::Port(53317));
        assert_eq!(icon(&snapshot, &peers(&snapshot, &[])), "pneu-tray-error");
        for (_, svg) in ICONS {
            for foreground in ["#232629", "#eff0f1"] {
                let svg = recolor_svg(svg, foreground, "#da4453", "#f67400");
                for size in [16, 20, 24, 32] {
                    let rgba = render_svg(svg.as_bytes(), size)?;
                    assert_eq!(rgba.len(), (size * size * 4) as usize);
                    assert!(rgba.chunks_exact(4).any(|p| p[3] != 0));
                    assert!(rgba.chunks_exact(4).any(|p| p[3] == 0));
                }
            }
        }
        assert!(render_svg(ICONS[0].1.as_bytes(), 0).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    fn entry(text: &str) -> crate::store::history::Item {
        crate::store::history::Item {
            id: "1".repeat(32),
            title: crate::store::history::title(text),
            url: crate::store::history::is_url(text),
            image: false,
            origin: None,
            at: 1,
            thumbnail: None,
        }
    }
    #[test]
    fn history_labels_collapse_whitespace_trim_urls_limit_unicode_and_escape_kde_mnemonics() {
        assert_eq!(
            history_label(&entry(" \n https://example.org/a_b \t"), false),
            "example.org/a_b"
        );
        assert_eq!(
            history_label(&entry("line_1\n\tline_2"), true),
            "line__1 line__2"
        );
        assert_eq!(
            history_label(&entry(&"é".repeat(40)), false),
            "é".repeat(40)
        );
        assert_eq!(
            history_label(&entry(&"é".repeat(41)), false),
            format!("{}…", "é".repeat(39))
        );
        assert!(
            history_label(
                &crate::store::history::Item {
                    image: true,
                    ..entry("")
                },
                false
            )
            .starts_with("Image · ")
        );
    }
    #[test]
    fn history_menu_is_hidden_when_off_disabled_when_empty_and_has_ten_latest_plus_show_all() {
        let mut snapshot = Snapshot::default();
        let mut config = crate::Config::default();
        fn history(items: Vec<MenuEntry>) -> Option<Vec<MenuEntry>> {
            items.into_iter().find_map(|i| match i {
                MenuEntry::Submenu { label, items } if label == "Clipboard History" => Some(items),
                _ => None,
            })
        }
        assert!(history(menu(&snapshot, &config)).is_none());
        config.clipboard_history = true;
        let empty = history(menu(&snapshot, &config)).unwrap();
        assert_eq!(empty.len(), 1);
        assert!(
            matches!(&empty[0],MenuEntry::Item {label,enabled:false,command:None,..} if label=="Nothing copied yet")
        );
        snapshot.clipboard_history = (0..11).map(|i| entry(&format!("text {i}"))).collect();
        let items = history(menu(&snapshot, &config)).unwrap();
        assert_eq!(items.len(), 12);
        assert!(matches!(&items[9],MenuEntry::Item {label,..} if label=="text 9"));
        assert!(
            matches!(&items[11],MenuEntry::Item {label,command:Some(super::super::Command::ShowClipboardHistory),..} if label=="Show All")
        );
    }
}
