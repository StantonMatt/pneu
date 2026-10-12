use super::{Command, Snapshot, Toggle};
use crate::Config;
use anyhow::Result;
use ksni::{TrayMethods, menu::*};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

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
pub struct Tray {
    pub snapshot: Snapshot,
    pub config: Config,
    pub icons: PathBuf,
    pub pixmaps: Vec<(String, ksni::Icon)>,
    pub named_icons: bool,
    pub commands: mpsc::Sender<Command>,
}
impl Tray {
    fn send(&self, command: Command) {
        let _ = self.commands.try_send(command);
    }
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
            self.snapshot.icon().into()
        } else {
            String::new()
        }
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.pixmaps
            .iter()
            .filter(|(name, _)| name == self.snapshot.icon())
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
        ksni::ToolTip {
            icon_name: "pneu".into(),
            title: "Pneu".into(),
            description: super::notifications::escape(&self.snapshot.tooltip())
                .replace('\n', "<br/>"),
            ..Default::default()
        }
    }
    fn activate(&mut self, _: i32, _: i32) {
        self.send(Command::Window);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let mut items = vec![
            StandardItem {
                label: self.snapshot.status(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Open Folder".into(),
                icon_name: "folder-open".into(),
                activate: Box::new(|s: &mut Self| s.send(Command::OpenFolder)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
        ];
        for toggle in Toggle::all() {
            if toggle == Toggle::Clipboard {
                items.push(ksni::MenuItem::Separator);
            }
            items.push(
                CheckmarkItem {
                    label: toggle.label().into(),
                    checked: toggle.checked(&self.config),
                    enabled: toggle.enabled(&self.config),
                    activate: Box::new(move |s: &mut Self| s.send(Command::Toggle(toggle))),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(ksni::MenuItem::Separator);
        items.push(
            StandardItem {
                label: if self.snapshot.paired {
                    "Configure Pneu…"
                } else {
                    "Pair Phone…"
                }
                .into(),
                icon_name: if self.snapshot.paired {
                    "configure"
                } else {
                    "smartphone"
                }
                .into(),
                activate: Box::new(|s: &mut Self| s.send(Command::Window)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|s: &mut Self| s.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
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
    let foreground = palette.foreground;
    let negative = palette.negative;
    ICONS
        .into_iter()
        .map(|(name, svg)| {
            let svg = svg
                .replace("#232629", &foreground)
                .replace("#da4453", &negative);
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

#[cfg(test)]
mod tests {
    use super::*;
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
    fn every_tray_state_renders_in_light_and_custom_dark_foreground() -> Result<()> {
        for (foreground, rgb) in [("#232629", [35, 38, 41]), ("#eff0f1", [239, 240, 241])] {
            let icons = pixmaps_for(super::super::palette::Colors {
                foreground: foreground.into(),
                negative: "#da4453".into(),
                background: "#ffffff".into(),
            })?;
            assert_eq!(icons.len(), 5);
            for (_, icon) in icons {
                assert!(
                    icon.data
                        .chunks_exact(4)
                        .any(|p| p[0] > 240 && p[1..] == rgb)
                );
            }
        }
        Ok(())
    }
}
