use super::{
    Command, Snapshot, Toggle,
    window::{ClipboardMode, Peer, contract::Transfer, peers},
};
use crate::Config;
use anyhow::Result;
use ksni::{TrayMethods, menu::*};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};
use tokio::sync::mpsc;

pub use super::tray_model::{ICONS, render_svg};
pub struct Tray {
    pub snapshot: Snapshot,
    pub config: Config,
    pub icons: PathBuf,
    pub pixmaps: Vec<(String, ksni::Icon)>,
    pub named_icons: bool,
    pub commands: mpsc::Sender<Command>,
    device_slots: Mutex<Vec<String>>,
}
impl Tray {
    fn send(&self, command: Command) {
        let _ = self.commands.try_send(command);
    }
    fn transfers(&self) -> Vec<Transfer> {
        self.snapshot.transfers.iter().map(Transfer::from).collect()
    }
    fn clipboard_mode(&self) -> ClipboardMode {
        let config = serde_json::to_value(&self.config).unwrap_or_default();
        ClipboardMode::from_config(
            Some(&config["clipboard_mode"]).filter(|v| !v.is_null()),
            self.config.clipboard_sync_to_phone,
        )
    }
}
pub(crate) use super::tray_model::{icon, status};
fn item<T>(
    label: impl Into<String>,
    icon: &str,
    activate: impl Fn(&mut T) + Send + 'static,
) -> ksni::MenuItem<T> {
    StandardItem {
        label: label.into(),
        icon_name: icon.into(),
        activate: Box::new(activate),
        ..Default::default()
    }
    .into()
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "pneu".into()
    }
    fn title(&self) -> String {
        "Pneu".into()
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }
    fn status(&self) -> ksni::Status {
        if self.snapshot.error.is_some() {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }
    fn icon_theme_path(&self) -> String {
        self.icons.display().to_string()
    }
    fn icon_name(&self) -> String {
        if self.named_icons {
            icon(&self.snapshot, &peers(&self.snapshot, &self.transfers())).into()
        } else {
            String::new()
        }
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let name = icon(&self.snapshot, &peers(&self.snapshot, &self.transfers()));
        self.pixmaps
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, icon)| icon.clone())
            .collect()
    }
    fn attention_icon_name(&self) -> String {
        self.icon_name()
    }
    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon_pixmap()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let transfers = self.transfers();
        let peers = peers(&self.snapshot, &transfers);
        // One line per device; the status line leads only when it adds something.
        let mut lines: Vec<String> = peers.iter().map(Peer::short).collect();
        if self.snapshot.error.is_some()
            || self.snapshot.pairing.is_some()
            || lines.is_empty()
            || peers.iter().any(|p| p.sending() || p.receiving())
        {
            lines.insert(0, status(&self.snapshot, &peers, &transfers));
        }
        ksni::ToolTip {
            icon_name: "pneu".into(),
            title: "Pneu".into(),
            description: super::notifications::escape(&lines.join("\n")).replace('\n', "<br/>"),
            ..Default::default()
        }
    }
    fn activate(&mut self, _: i32, _: i32) {
        self.send(Command::Window {
            activation_token: None,
        });
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let transfers = self.transfers();
        let peers = peers(&self.snapshot, &transfers);
        let mut items = vec![
            StandardItem {
                label: status(&self.snapshot, &peers, &transfers),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
        ];
        if !peers.is_empty() {
            let connected =
                super::runtime::file_recipients(self.snapshot.send_targets.clone(), None);
            let mut targets = vec![
                item(
                    "All Connected Devices…",
                    "network-workgroup",
                    move |s: &mut Self| {
                        s.send(Command::SendFiles {
                            recipients: connected.clone(),
                            targeted: false,
                        })
                    },
                ),
                ksni::MenuItem::Separator,
            ];
            // ksni IDs are positional until the menu's structure changes.
            // Never reassign a position: keep invisible tombstones for removed
            // identities and append new bindings (which invalidates old IDs).
            let identities = tray_identities(&self.snapshot, &peers);
            let slots = stable_slots(
                &mut self.device_slots.lock().expect("tray device slots"),
                &identities,
            );
            for identity in slots {
                let peer = peers.iter().find(|p| {
                    identities
                        .iter()
                        .any(|(key, id)| key == &identity && id == &p.id)
                });
                let Some(peer) = peer else {
                    targets.push(
                        StandardItem {
                            visible: false,
                            enabled: false,
                            ..Default::default()
                        }
                        .into(),
                    );
                    continue;
                };
                let Ok(device) = pneu_proto::Id::new(peer.id.as_str()) else {
                    continue;
                };
                let captured = super::runtime::file_recipients(
                    self.snapshot.send_targets.clone(),
                    Some(&device),
                );
                targets.push(item(
                    format!("{}…", menu_label(&peer.name)),
                    if peer.phone { "smartphone" } else { "computer" },
                    move |s: &mut Self| {
                        let current = tray_identities(&s.snapshot, &peers_for_menu(s));
                        if current.iter().any(|(key, _)| key == &identity) {
                            s.send(Command::SendFiles {
                                recipients: captured.clone(),
                                targeted: true,
                            });
                        }
                    },
                ));
            }
            items.push(
                SubMenu {
                    label: "Send Files To".into(),
                    icon_name: "document-send".into(),
                    submenu: targets,
                    ..Default::default()
                }
                .into(),
            );
            let mode = self.clipboard_mode();
            if mode == ClipboardMode::Manual {
                let captured = super::captured_authorities(&self.snapshot);
                items.push(item("Send Clipboard", "edit-paste", move |s: &mut Self| {
                    s.send(Command::SendClipboard(captured.clone()))
                }));
            }
            items.push(item("Open Folder", "folder-open", |s: &mut Self| {
                s.send(Command::OpenFolder)
            }));
            items.push(ksni::MenuItem::Separator);
            let modes = [
                ClipboardMode::Automatic,
                ClipboardMode::Manual,
                ClipboardMode::Off,
            ];
            items.push(
                SubMenu {
                    label: "Clipboard".into(),
                    icon_name: "edit-paste".into(),
                    submenu: vec![
                        RadioGroup {
                            selected: modes.iter().position(|m| *m == mode).unwrap_or(0),
                            select: Box::new(move |s: &mut Self, index| {
                                s.send(Command::ClipboardMode(modes[index].key()));
                            }),
                            options: ["Share Automatically", "Share Manually", "Off"]
                                .into_iter()
                                .map(|label| RadioItem {
                                    label: label.into(),
                                    ..Default::default()
                                })
                                .collect(),
                        }
                        .into(),
                        ksni::MenuItem::Separator,
                        CheckmarkItem {
                            label: Toggle::IncludeImages.label().into(),
                            checked: Toggle::IncludeImages.checked(&self.config),
                            enabled: mode != ClipboardMode::Off,
                            activate: Box::new(|s: &mut Self| {
                                s.send(Command::Toggle(Toggle::IncludeImages))
                            }),
                            ..Default::default()
                        }
                        .into(),
                    ],
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                SubMenu {
                    label: "When Files Arrive".into(),
                    icon_name: "download".into(),
                    submenu: [
                        Toggle::Save,
                        Toggle::Notifications,
                        Toggle::CopyImages,
                        Toggle::Viewer,
                    ]
                    .into_iter()
                    .map(|toggle| {
                        CheckmarkItem {
                            label: toggle.label().into(),
                            checked: toggle.checked(&self.config),
                            activate: Box::new(move |s: &mut Self| s.send(Command::Toggle(toggle))),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect(),
                    ..Default::default()
                }
                .into(),
            );
            items.push(ksni::MenuItem::Separator);
        }
        items.push(item("Add Device…", "list-add", |s: &mut Self| {
            s.send(Command::AddDevice {
                activation_token: None,
            })
        }));
        items.push(item("Configure Pneu…", "configure", |s: &mut Self| {
            s.send(Command::Window {
                activation_token: None,
            })
        }));
        items.push(item("Quit", "application-exit", |s: &mut Self| {
            s.send(Command::Quit)
        }));
        items
    }
}
fn peers_for_menu(tray: &Tray) -> Vec<Peer> {
    peers(&tray.snapshot, &tray.transfers())
}
fn tray_identities(snapshot: &Snapshot, peers: &[Peer]) -> Vec<(String, String)> {
    let targets = serde_json::to_value(&snapshot.send_targets).unwrap_or_default();
    peers
        .iter()
        .map(|peer| {
            let binding = targets.as_array().and_then(|targets| {
                targets
                    .iter()
                    .find(|t| t["device"].as_str() == Some(&peer.id))
            });
            let generation = binding.and_then(|t| t["generation"].as_u64()).unwrap_or(0);
            let fingerprint = binding
                .and_then(|t| t["fingerprint"].as_str())
                .unwrap_or("");
            (
                format!("{}/{generation}/{fingerprint}", peer.id),
                peer.id.clone(),
            )
        })
        .collect()
}
fn stable_slots(slots: &mut Vec<String>, current: &[(String, String)]) -> Vec<String> {
    for (identity, _) in current {
        if !slots.contains(identity) {
            slots.push(identity.clone());
        }
    }
    slots.clone()
}

/// Menu labels treat "_" as an access-key marker; long names end in an ellipsis.
fn menu_label(name: &str) -> String {
    let name = name.replace('_', "__");
    if name.chars().count() > 40 {
        format!("{}…", name.chars().take(39).collect::<String>().trim_end())
    } else {
        name
    }
}
pub async fn start(
    snapshot: Snapshot,
    config: Config,
    data: &Path,
    commands: mpsc::Sender<Command>,
) -> Result<ksni::Handle<Tray>> {
    let icons = prepare_icons(data)?;
    let named_icons = std::env::var_os("PNEU_TRAY_NAMED_ICONS").is_some();
    Ok(Tray {
        snapshot,
        config,
        icons,
        pixmaps: pixmaps()?,
        named_icons,
        commands,
        device_slots: Mutex::new(Vec::new()),
    }
    .assume_sni_available(true)
    .spawn()
    .await?)
}
fn prepare_icons(data: &Path) -> Result<PathBuf> {
    let folder = crate::storage::Folder::open(&data.join("icons/hicolor/scalable/status"), true)?;
    for (name, svg) in ICONS {
        crate::config::atomic_private_in(
            &folder,
            std::ffi::OsStr::new(&format!("{name}.svg")),
            svg.as_bytes(),
        )?;
    }
    folder.path()
}
// Use an explicit panel-colored pixmap until named-icon recoloring is verified.
// ARGB fallback follows the active Plasma panel theme or adaptive window palette.
pub fn pixmaps() -> Result<Vec<(String, ksni::Icon)>> {
    pixmaps_for(super::palette::panel_colors())
}
fn pixmaps_for(palette: super::palette::Colors) -> Result<Vec<(String, ksni::Icon)>> {
    ICONS
        .into_iter()
        .map(|(name, svg)| {
            let svg = recolor_svg(svg, &palette);
            let rgba = render_svg(svg.as_bytes(), 44)?;
            let mut argb = Vec::with_capacity(rgba.len());
            for pixel in rgba.chunks_exact(4) {
                argb.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
            }
            Ok((
                name.into(),
                ksni::Icon {
                    width: 44,
                    height: 44,
                    data: argb,
                },
            ))
        })
        .collect()
}
fn recolor_svg(svg: &str, palette: &super::palette::Colors) -> String {
    super::tray_model::recolor_svg(
        svg,
        &palette.foreground,
        &palette.negative,
        &palette.neutral,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn devices(states: &[(&str, bool, &str)]) -> Snapshot {
        Snapshot {
            devices: states
                .iter()
                .enumerate()
                .map(
                    |(i, (name, phone, state))| crate::core::mesh::DeviceSnapshot {
                        device: pneu_proto::Id::from_bytes([i as u8 + 1; 16]),
                        name: (*name).into(),
                        role: if *phone { "phone" } else { "computer" }.into(),
                        state: (*state).into(),
                        last_seen: 0,
                        dialer: None,
                        route: (*state == "connected").then(|| "Wi-Fi".into()),
                    },
                )
                .collect(),
            ..Default::default()
        }
    }
    #[test]
    fn device_menu_slots_never_reassign_stale_ids() {
        let mut slots = Vec::new();
        let first = vec![
            ("A/1/fp-a".into(), "A".into()),
            ("B/1/fp-b".into(), "B".into()),
        ];
        assert_eq!(stable_slots(&mut slots, &first), ["A/1/fp-a", "B/1/fp-b"]);
        let reordered = vec![first[1].clone(), first[0].clone()];
        assert_eq!(
            stable_slots(&mut slots, &reordered),
            ["A/1/fp-a", "B/1/fp-b"]
        );
        let replacement = vec![
            ("C/1/fp-c".into(), "C".into()),
            ("A/2/new-fp".into(), "A".into()),
        ];
        assert_eq!(
            stable_slots(&mut slots, &replacement),
            ["A/1/fp-a", "B/1/fp-b", "C/1/fp-c", "A/2/new-fp"]
        );
    }

    #[test]
    fn stale_device_menu_positions_keep_their_original_identity_or_become_inert() {
        use ksni::Tray as _;
        let (commands, mut rx) = mpsc::channel(16);
        let mut tray = Tray {
            snapshot: devices(&[("A", false, "connected"), ("B", false, "connected")]),
            config: Config::default(),
            icons: PathBuf::new(),
            pixmaps: Vec::new(),
            named_icons: false,
            commands,
            device_slots: Mutex::new(Vec::new()),
        };
        tray.snapshot.send_targets = tray
            .snapshot
            .devices
            .iter()
            .map(|d| crate::outbox::SendTarget {
                device: d.device.clone(),
                name: d.name.clone(),
                generation: 1,
                fingerprint: Some(pneu_proto::Hash::new("ab".repeat(32)).unwrap()),
                eligible: true,
                would_queue: false,
                reason: None,
            })
            .collect();
        let original_a = tray.snapshot.devices[0].device.clone();
        let activate_first_device = |tray: &mut Tray| {
            let menu = tray.menu();
            let ksni::MenuItem::SubMenu(send) = &menu[2] else {
                panic!("send submenu");
            };
            let ksni::MenuItem::Standard(first) = &send.submenu[2] else {
                panic!("device item");
            };
            (first.activate)(tray);
        };
        activate_first_device(&mut tray);
        assert!(
            matches!(rx.try_recv().unwrap(), Command::SendFiles { recipients: targets, .. } if targets.len() == 1 && targets[0].device == original_a)
        );
        tray.snapshot.devices.reverse();
        activate_first_device(&mut tray);
        assert!(
            matches!(rx.try_recv().unwrap(), Command::SendFiles { recipients: targets, .. } if targets.len() == 1 && targets[0].device == original_a)
        );
        tray.snapshot.devices.retain(|d| d.device != original_a);
        tray.snapshot
            .devices
            .push(crate::core::mesh::DeviceSnapshot {
                device: pneu_proto::Id::from_bytes([8; 16]),
                name: "C".into(),
                role: "computer".into(),
                state: "connected".into(),
                last_seen: 0,
                dialer: None,
                route: None,
            });
        // Even an accessibility/DBus action on an invisible tombstone is inert.
        activate_first_device(&mut tray);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn queued_tray_sends_keep_the_menu_binding_and_broadcast_recipient_set() {
        use ksni::Tray as _;
        let (commands, mut rx) = mpsc::channel(16);
        let mut tray = Tray {
            snapshot: devices(&[("A", false, "connected")]),
            config: Config {
                clipboard_mode: crate::config::ClipboardMode::Manual,
                ..Config::default()
            },
            icons: PathBuf::new(),
            pixmaps: Vec::new(),
            named_icons: false,
            commands,
            device_slots: Mutex::new(Vec::new()),
        };
        let authority = crate::outbox::Authority {
            device: tray.snapshot.devices[0].device.clone(),
            generation: 7,
            fingerprint: pneu_proto::Hash::new("ab".repeat(32)).unwrap(),
        };
        tray.snapshot.send_targets = vec![crate::outbox::SendTarget {
            device: authority.device.clone(),
            name: "A".into(),
            generation: authority.generation,
            fingerprint: Some(authority.fingerprint.clone()),
            eligible: true,
            would_queue: false,
            reason: None,
        }];
        let menu = tray.menu();
        let ksni::MenuItem::SubMenu(files) = &menu[2] else {
            panic!("files submenu")
        };
        let ksni::MenuItem::Standard(one) = &files.submenu[2] else {
            panic!("named recipient")
        };
        (one.activate)(&mut tray);
        // The command is already queued when the menu learns of replacement.
        tray.snapshot.send_targets[0].generation = 99;
        tray.snapshot.send_targets[0].fingerprint =
            Some(pneu_proto::Hash::new("cd".repeat(32)).unwrap());
        assert!(
            matches!(rx.try_recv().unwrap(), Command::SendFiles { recipients: targets, .. } if targets == vec![authority.clone()])
        );
        // Old named menu callbacks become inert once the snapshot is current.
        (one.activate)(&mut tray);
        assert!(rx.try_recv().is_err());
        // The fixed-position broadcast and clipboard callbacks also keep their
        // original recipients instead of adopting a new pairing when invoked.
        let ksni::MenuItem::Standard(all) = &files.submenu[0] else {
            panic!("connected recipients")
        };
        (all.activate)(&mut tray);
        assert!(
            matches!(rx.try_recv().unwrap(), Command::SendFiles { recipients: targets, .. } if targets == vec![authority.clone()])
        );
        let clipboard = menu
            .iter()
            .find_map(|item| match item {
                ksni::MenuItem::Standard(item) if item.label == "Send Clipboard" => Some(item),
                _ => None,
            })
            .unwrap();
        (clipboard.activate)(&mut tray);
        assert!(
            matches!(rx.try_recv().unwrap(), Command::SendClipboard(targets) if targets == vec![authority])
        );
    }

    #[test]
    fn status_line_and_icon_cover_one_and_several_devices() {
        let line = |s: &Snapshot| status(s, &peers(s, &[]), &[]);
        let icon_of = |s: &Snapshot| icon(s, &peers(s, &[]));
        let none = devices(&[]);
        assert_eq!(line(&none), "No devices paired");
        assert_eq!(icon_of(&none), "pneu-tray-disconnected");
        let mut one = devices(&[("Galaxy S25+", true, "connected")]);
        one.device = Some(pneu_proto::Id::from_bytes([1; 16]));
        one.phone.route = "Wi-Fi".into();
        assert_eq!(line(&one), "Galaxy S25+ connected over Wi-Fi");
        assert_eq!(icon_of(&one), "pneu-tray-connected");
        one.paused_devices = vec![pneu_proto::Id::from_bytes([1; 16])];
        assert_eq!(line(&one), "Galaxy S25+ paused sending");
        assert_eq!(icon_of(&one), "pneu-tray-paused");
        let two = devices(&[
            ("Galaxy S25+", true, "connected"),
            ("MacBook Air", false, "connected"),
        ]);
        assert_eq!(line(&two), "Galaxy S25+ and MacBook Air connected");
        let some = devices(&[
            ("A", false, "connected"),
            ("B", false, "disconnected"),
            ("C", false, "disconnected"),
        ]);
        assert_eq!(line(&some), "1 of 3 devices connected");
        let off = devices(&[("A", false, "disconnected"), ("B", false, "connecting")]);
        assert_eq!(line(&off), "No devices connected");
        assert_eq!(icon_of(&off), "pneu-tray-disconnected");
        let sending = [Transfer {
            operation: "o".into(),
            device: pneu_proto::Id::from_bytes([1; 16]).as_str().into(),
            state: "sending".into(),
            ..Default::default()
        }];
        assert_eq!(
            status(&two, &peers(&two, &sending), &sending),
            "Sending to Galaxy S25+…"
        );
        assert_eq!(icon(&two, &peers(&two, &sending)), "pneu-tray-receiving");
        assert_eq!(menu_label("a_b"), "a__b");
        assert_eq!(menu_label(&"x".repeat(50)).chars().count(), 40);
    }
    #[test]
    fn icon_preparation_preserves_symlink_targets_and_refuses_redirected_directories() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let icons = prepare_icons(dir.path())?;
        let external = dir.path().join("external");
        std::fs::create_dir(&external)?;
        let keep = external.join("keep");
        std::fs::write(&keep, b"foreign")?;
        let entry = icons.join(format!("{}.svg", ICONS[0].0));
        std::fs::remove_file(&entry)?;
        std::os::unix::fs::symlink(&keep, &entry)?;
        prepare_icons(dir.path())?;
        assert_eq!(std::fs::read(&keep)?, b"foreign");
        assert!(!std::fs::symlink_metadata(&entry)?.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(entry)?, ICONS[0].1);
        for component in [
            "icons/hicolor/scalable/status",
            "icons/hicolor/scalable",
            "icons/hicolor",
            "icons",
        ] {
            let path = dir.path().join(component);
            let moved = dir.path().join("moved");
            std::fs::rename(&path, &moved)?;
            std::os::unix::fs::symlink(&external, &path)?;
            assert!(prepare_icons(dir.path()).is_err());
            assert_eq!(std::fs::read(&keep)?, b"foreign");
            assert_eq!(std::fs::read_dir(&external)?.count(), 1);
            std::fs::remove_file(&path)?;
            std::fs::rename(&moved, &path)?;
        }
        Ok(())
    }
    #[test]
    fn coincident_palette_colors_keep_glyph_badge_and_receiving_colors() -> Result<()> {
        // All source-token colors plus an unrelated custom color. The Cartesian
        // product covers cascades and palettes whose semantic colors coincide.
        let colors = ["#232629", "#da4453", "#f67400", "#1d2e3f"];
        let rgb =
            |color: &str| -> [u8; 3] { hex::decode(&color[1..]).unwrap().try_into().unwrap() };
        for foreground in colors {
            for negative in colors {
                for neutral in colors {
                    let icons = pixmaps_for(super::super::palette::Colors {
                        foreground: foreground.into(),
                        negative: negative.into(),
                        neutral: neutral.into(),
                        background: "#ffffff".into(),
                    })?;
                    for (name, icon) in icons {
                        // Interior pixels of the landed half and top-right badge,
                        // away from antialiased edges and knocked-out bars.
                        let pixel = |x: usize, y: usize| &icon.data[(y * 44 + x) * 4..][..4];
                        let glyph = pixel(25, 28);
                        let receiving = name == "pneu-tray-receiving";
                        let disconnected = name == "pneu-tray-disconnected";
                        assert_eq!(glyph[0], if disconnected { 128 } else { 255 }, "{name}");
                        let expected = if receiving {
                            [99, 112, 255]
                        } else {
                            rgb(foreground)
                        };
                        assert!(
                            glyph[1..].iter().zip(expected).all(|(actual, expected)| {
                                actual.abs_diff(expected) <= if disconnected { 2 } else { 0 }
                            }),
                            "{name}: foreground={foreground}, negative={negative}, neutral={neutral}"
                        );
                        let badge = match name.as_str() {
                            "pneu-tray-error" => Some(negative),
                            "pneu-tray-paused" => Some(neutral),
                            _ => None,
                        };
                        if let Some(color) = badge {
                            let actual = pixel(35, 16);
                            assert_eq!(actual[0], 255, "{name}");
                            assert_eq!(
                                &actual[1..],
                                &rgb(color),
                                "{name}: foreground={foreground}, negative={negative}, neutral={neutral}"
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
    #[test]
    fn every_tray_state_renders_in_light_and_custom_dark_foreground() -> Result<()> {
        for (foreground, rgb) in [("#232629", [35, 38, 41]), ("#eff0f1", [239, 240, 241])] {
            let icons = pixmaps_for(super::super::palette::Colors {
                foreground: foreground.into(),
                negative: "#da4453".into(),
                neutral: "#9a7b3c".into(),
                background: "#ffffff".into(),
            })?;
            assert_eq!(icons.len(), 5);
            for (name, icon) in icons {
                let expected = if name == "pneu-tray-receiving" {
                    [99, 112, 255]
                } else {
                    rgb
                };
                let alpha = if name == "pneu-tray-disconnected" {
                    120
                } else {
                    240
                };
                assert!(
                    icon.data.chunks_exact(4).any(|p| p[0] > alpha
                        && p[1..].iter().zip(expected).all(|(actual, expected)| actual
                            .abs_diff(expected)
                            <= if name == "pneu-tray-disconnected" {
                                2
                            } else {
                                0
                            })),
                    "{name}"
                );
                if name == "pneu-tray-disconnected" {
                    assert!(icon.data.chunks_exact(4).all(|p| p[0] <= 128));
                }
                if name == "pneu-tray-paused" {
                    assert!(
                        icon.data
                            .chunks_exact(4)
                            .any(|p| p[0] > 240 && p[1..] == [154, 123, 60])
                    );
                }
            }
        }
        Ok(())
    }
}
