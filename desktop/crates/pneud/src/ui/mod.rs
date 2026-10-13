//! Desktop presentation state. The protocol core remains usable without a tray host.
pub(crate) mod activation;
pub mod clipboard;
pub mod notifications;
pub mod palette;
pub(crate) mod portal;
pub mod runtime;
pub mod tray;
pub mod window;

use crate::Config;
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, path::Path, time::Instant};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Phone {
    /// Metadata is usable only for this pairing identity, including after restart.
    #[serde(default)]
    pub device: Option<pneu_proto::Id>,
    pub name: String,
    pub last_seen: u64,
    #[serde(default)]
    pub route: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub device: Option<pneu_proto::Id>,
    /// Connection generation which supplied the display metadata, if known.
    #[serde(default)]
    pub generation: Option<u64>,
    pub phone: Phone,
    pub paired: bool,
    pub connected: bool,
    pub paused: bool,
    pub receiving: bool,
    pub pairing: Option<String>,
    pub error: Option<ReceiveError>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReceiveDestination {
    SaveFolder,
    Cache,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReceiveError {
    Port(u16),
    Storage {
        destination: ReceiveDestination,
        path: String,
        disk_full: bool,
    },
}
impl ReceiveError {
    pub fn storage(
        destination: ReceiveDestination,
        path: &Path,
        code: pneu_proto::NackCode,
    ) -> Self {
        Self::Storage {
            destination,
            path: display_path(path),
            disk_full: code == pneu_proto::NackCode::DiskFull,
        }
    }
    fn is_destination(&self, kind: ReceiveDestination, path: Option<&str>) -> bool {
        matches!(self, Self::Storage { destination, path: actual, .. } if *destination == kind && path.is_none_or(|p| p == actual))
    }
    pub fn tooltip(&self) -> String {
        match self {
            Self::Port(port) => format!("Can't receive: port {port} is in use"),
            Self::Storage {
                path,
                disk_full: true,
                ..
            } => format!("The disk with {path} is full"),
            Self::Storage { path, .. } => format!("Can't save to {path}"),
        }
    }
    pub fn banner(&self) -> String {
        match self {
            Self::Port(port) => format!("Port {port} is in use by another program."),
            Self::Storage {
                path,
                disk_full: true,
                ..
            } => format!("The disk with {path} is full."),
            Self::Storage { path, .. } => format!("Can't write to {path}."),
        }
    }
}
impl Snapshot {
    // One destination-keyed state: success resolves only the same destination;
    // storage errors cannot hide a listener error.
    pub(crate) fn storage_result(
        &mut self,
        destination: ReceiveDestination,
        path: &Path,
        code: Option<pneu_proto::NackCode>,
    ) {
        if matches!(self.error, Some(ReceiveError::Port(_))) {
            return;
        }
        match code {
            Some(code) => self.error = Some(ReceiveError::storage(destination, path, code)),
            None => self.clear_destination(destination, Some(&display_path(path))),
        }
    }
    pub(crate) fn clear_destination(&mut self, kind: ReceiveDestination, path: Option<&str>) {
        if self
            .error
            .as_ref()
            .is_some_and(|e| e.is_destination(kind, path))
        {
            self.error = None;
        }
    }
}
#[derive(Default)]
pub(crate) struct State {
    pub snapshot: Snapshot,
    pub receiving_until: Option<Instant>,
    pub phone_generation: Option<u64>,
    pub active_transfers: std::collections::HashSet<pneu_proto::Id>,
}
impl State {
    pub fn clear_transfers(&mut self) {
        self.active_transfers.clear();
        self.receiving_until = None;
        self.snapshot.receiving = false;
    }
}
impl Snapshot {
    pub fn icon(&self) -> &'static str {
        if self.error.is_some() {
            "pneu-tray-error"
        } else if self.paused && self.connected {
            "pneu-tray-paused"
        } else if self.receiving && self.connected {
            "pneu-tray-receiving"
        } else if self.connected {
            "pneu-tray-connected"
        } else {
            "pneu-tray-disconnected"
        }
    }
    pub fn status(&self) -> String {
        let phone = &self.phone.name;
        if let Some(error) = &self.error {
            error.tooltip()
        } else if let Some(name) = &self.pairing {
            format!("Pairing with {name}…")
        } else if !self.paired {
            "No phone paired".into()
        } else if self.paused && self.connected {
            format!("{phone} paused sending")
        } else if self.receiving && self.connected {
            format!("Receiving from {phone}…")
        } else if self.connected {
            format!("{phone} connected over {}", self.phone.route)
        } else {
            format!("{phone} isn't connected")
        }
    }
    pub fn tooltip(&self) -> String {
        let mut text = self.status();
        if self.paired && !self.connected && self.error.is_none() && self.phone.last_seen != 0 {
            text.push_str(&format!("\nLast seen {}", last_seen(self.phone.last_seen)));
        }
        text
    }
    pub fn window_status(&self) -> String {
        if self.paused && self.connected {
            "Paused on phone".into()
        } else if self.receiving && self.connected {
            "Receiving…".into()
        } else if self.connected {
            format!("Connected over {}", self.phone.route)
        } else if self.phone.last_seen == 0 {
            "Not connected".into()
        } else {
            format!(
                "Not connected · last seen {}",
                last_seen(self.phone.last_seen)
            )
        }
    }
}
pub fn last_seen(timestamp: u64) -> String {
    use chrono::{DateTime, Local};
    let Some(time) = DateTime::from_timestamp_millis(timestamp as i64) else {
        return String::new();
    };
    let time = time.with_timezone(&Local);
    let today = Local::now().date_naive();
    if time.date_naive() == today {
        time.format(if twelve_hour_clock() {
            "%-I:%M %p"
        } else {
            "%H:%M"
        })
        .to_string()
    } else if Some(time.date_naive()) == today.pred_opt() {
        "Yesterday".into()
    } else {
        time.format("%-d %b").to_string()
    }
}
// Plasma writes regional overrides here; fall back to the process locale.
fn twelve_hour_clock() -> bool {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    let ini = std::fs::read_to_string(config.join("plasma-localerc")).unwrap_or_default();
    let locale = std::env::var("LC_ALL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ini.lines()
                .find_map(|line| line.strip_prefix("LC_TIME="))
                .map(str::to_owned)
        })
        .or_else(|| std::env::var("LC_TIME").ok())
        .or_else(|| std::env::var("LANG").ok())
        .unwrap_or_else(|| "C".into());
    let Ok(locale) = std::ffi::CString::new(locale) else {
        return false;
    };
    // The locale object is private to this call; no process-global setlocale.
    unsafe {
        let handle = libc::newlocale(libc::LC_TIME_MASK, locale.as_ptr(), std::ptr::null_mut());
        if handle.is_null() {
            return false;
        }
        let format =
            std::ffi::CStr::from_ptr(libc::nl_langinfo_l(libc::T_FMT, handle)).to_string_lossy();
        let twelve = format.contains("%I") || format.contains("%l") || format.contains("%r");
        libc::freelocale(handle);
        twelve
    }
}
pub fn route(address: IpAddr) -> &'static str {
    match address {
        IpAddr::V4(ip) if u32::from(ip) & 0xffc00000 == 0x64400000 => "Tailscale",
        IpAddr::V6(ip) if ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0] => "Tailscale",
        IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some() => {
            route(IpAddr::V4(ip.to_ipv4_mapped().unwrap()))
        }
        _ => "Wi-Fi",
    }
}
pub fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(relative) = path.strip_prefix(home)
    {
        if relative.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toggle {
    Save,
    Notifications,
    CopyImages,
    Viewer,
    Clipboard,
    IncludeImages,
}
impl Toggle {
    pub fn all() -> [Self; 6] {
        [
            Self::Save,
            Self::Notifications,
            Self::CopyImages,
            Self::Viewer,
            Self::Clipboard,
            Self::IncludeImages,
        ]
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Save => "Save to Folder",
            Self::Notifications => "Show Notifications",
            Self::CopyImages => "Copy Images to Clipboard",
            Self::Viewer => "Open in Image Viewer",
            Self::Clipboard => "Send Clipboard to Phone",
            Self::IncludeImages => "Include Images",
        }
    }
    pub fn checked(self, c: &Config) -> bool {
        match self {
            Self::Save => c.save_to_folder,
            Self::Notifications => c.notification_preview,
            Self::CopyImages => c.copy_image_to_clipboard,
            Self::Viewer => c.open_in_viewer,
            Self::Clipboard => c.clipboard_sync_to_phone,
            Self::IncludeImages => c.clipboard_include_images,
        }
    }
    pub fn enabled(self, c: &Config) -> bool {
        self != Self::IncludeImages || c.clipboard_sync_to_phone
    }
    pub fn flip(self, c: &mut Config) {
        match self {
            Self::Save => c.save_to_folder = !c.save_to_folder,
            Self::Notifications => c.notification_preview = !c.notification_preview,
            Self::CopyImages => c.copy_image_to_clipboard = !c.copy_image_to_clipboard,
            Self::Viewer => c.open_in_viewer = !c.open_in_viewer,
            Self::Clipboard => c.clipboard_sync_to_phone = !c.clipboard_sync_to_phone,
            Self::IncludeImages => c.clipboard_include_images = !c.clipboard_include_images,
        }
    }
}
#[derive(Clone, Debug)]
pub enum Command {
    Window { activation_token: Option<String> },
    ChooseFolder,
    OpenFolder,
    Toggle(Toggle),
    Quit,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn icons_and_status_precedence() {
        let mut s = Snapshot::default();
        assert_eq!(s.icon(), "pneu-tray-disconnected");
        assert_eq!(s.status(), "No phone paired");
        s.paired = true;
        s.phone.name = "Phone".into();
        s.connected = true;
        s.phone.route = "Wi-Fi".into();
        assert_eq!(s.status(), "Phone connected over Wi-Fi");
        assert_eq!(s.icon(), "pneu-tray-connected");
        s.receiving = true;
        assert_eq!(s.icon(), "pneu-tray-receiving");
        s.paused = true;
        assert_eq!(s.icon(), "pneu-tray-paused");
        s.error = Some(ReceiveError::Port(47821));
        assert_eq!(s.icon(), "pneu-tray-error");
        assert_eq!(s.status(), "Can't receive: port 47821 is in use");
    }
    #[test]
    fn menu_defaults_and_dependency() {
        let mut c = Config::default();
        assert_eq!(
            Toggle::all().map(|t| t.checked(&c)),
            [true, true, true, false, true, false]
        );
        Toggle::Clipboard.flip(&mut c);
        assert!(!Toggle::IncludeImages.enabled(&c));
        Toggle::IncludeImages.flip(&mut c);
        assert!(c.clipboard_include_images);
    }
    #[test]
    fn routes_include_both_tailscale_ranges() {
        for address in [
            "100.64.0.1",
            "100.127.255.254",
            "fd7a:115c:a1e0::1",
            "::ffff:100.100.1.1",
        ] {
            assert_eq!(route(address.parse().unwrap()), "Tailscale");
        }
        for address in ["100.63.255.255", "100.128.0.1", "192.168.1.1", "::1"] {
            assert_eq!(route(address.parse().unwrap()), "Wi-Fi");
        }
    }
}
