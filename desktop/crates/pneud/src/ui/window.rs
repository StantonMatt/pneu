//! One supervised window process. Qt has a main-thread event loop, independent
//! from the daemon runtime; all settings and trust changes go through Core IPC.
use crate::{Config, Paths, ipc};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::collections::HashMap;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

slint::slint! {
    export {
        PneuWindow, Ui, WindowFocus, Page, Dialog, CodeState, ScanState, DeviceItem, RecentItem, HistoryItem, SettingItem,
        PickItem, RecipientItem, Message, Summary, Tone, Tube, LinkState, LinkTile, DropItem, DropGeometry,
        TopKind, TopCard, LinkRow, DeviceLink
    } from "../../ui/window.slint";
    export { Palette } from "std-widgets.slint";
    export { Pneu } from "../../ui/theme.slint";
}

pub(crate) use super::send_request::SendRequest;
use super::window_setup::{self as setup, Remembered, Removal, Row, Setup, Top};

/// The download page, shown as a code on Add Device → Phone (window.slint
/// copies it too, and prints it without the scheme).
const DOWNLOAD_PAGE: &str = "https://stantonmatt.github.io/pneu/";
const TAILSCALE_DOWNLOAD: &str = "https://tailscale.com/download";

fn clipboard_send_error(error: &str, from_menu: bool) -> Option<&'static str> {
    if error.contains("clipboard image too large") {
        (!from_menu).then_some("Couldn't send the clipboard. The image is over 20 MB")
    } else {
        Some("Couldn't send the clipboard")
    }
}

fn is_pairing_page(page: Page) -> bool {
    matches!(page, Page::Add | Page::Code | Page::Scan)
}

/// Keep Scan active until the worker can navigate and preview the decoded link
/// in one action. Native capture is already stopped by the caller.
#[cfg(any(target_os = "macos", all(test, unix)))]
pub(super) fn scanned_pairing(window: &PneuWindow, payload: String) {
    if window.global::<Ui>().get_page() != Page::Scan {
        return;
    }
    window.global::<Ui>().invoke_scanned_link(payload.into());
}

/// Presentation adapters for the typed daemon snapshot.
pub(crate) mod contract {
    use super::*;
    #[derive(Clone, Debug, Default, Deserialize, PartialEq)]
    #[serde(default)]
    pub struct Transfer {
        pub operation: String,
        pub transfer_id: String,
        pub device: String,
        pub file_name: String,
        pub size: u64,
        pub sent: u64,
        pub state: String,
        pub error: Option<String>,
        pub direction: Option<String>,
        pub finished_at: Option<u64>,
        pub path: Option<PathBuf>,
    }
    impl Transfer {
        pub fn incoming(&self) -> bool {
            self.direction.as_deref() == Some("incoming")
        }
    }
    #[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
    #[serde(default)]
    pub struct SendTarget {
        pub device: String,
        pub name: String,
        pub eligible: bool,
        pub would_queue: bool,
        pub generation: u64,
        pub fingerprint: Option<String>,
        pub reason: Option<String>,
    }
    /// Global settings live in the config or the snapshot, depending on where
    /// the backend lands them; read either.
    pub fn setting<'a>(status: &'a Value, key: &str) -> Option<&'a Value> {
        [
            &status["config"],
            &status["snapshot"],
            &status["snapshot"]["settings"],
        ]
        .into_iter()
        .map(|v| &v[key])
        .find(|v| !v.is_null())
    }
    pub fn list<T: for<'de> Deserialize<'de>>(status: &Value, key: &str) -> Vec<T> {
        serde_json::from_value(status["snapshot"][key].clone()).unwrap_or_default()
    }
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    pub struct CapturedTarget {
        pub device: String,
        pub generation: u64,
        pub fingerprint: String,
    }
    pub fn resolve_files(
        targets: &[SendTarget],
        selection: crate::file_recipients::Selection<'_>,
    ) -> Result<Vec<CapturedTarget>> {
        let targets: Vec<crate::outbox::SendTarget> =
            serde_json::from_value(serde_json::to_value(targets)?)?;
        let resolution = crate::file_recipients::resolve(&targets, selection)?;
        Ok(serde_json::from_value(serde_json::to_value(
            resolution.recipients,
        )?)?)
    }
    pub fn send_command(
        paths: Vec<PathBuf>,
        targets: &[CapturedTarget],
        request_id: &str,
    ) -> Result<ipc::Command> {
        // Deserialize the public wire contract so the UI does not depend on
        // the backend's internal representation of captured authority.
        Ok(serde_json::from_value(
            json!({"command":"send_files", "paths": paths,
            "targets": targets, "request_id": request_id}),
        )?)
    }
    impl From<&crate::outbox::Transfer> for Transfer {
        fn from(t: &crate::outbox::Transfer) -> Self {
            Self {
                operation: t.operation.as_str().into(),
                transfer_id: t.transfer_id.as_str().into(),
                device: t.device.as_str().into(),
                file_name: t.file_name.clone(),
                size: t.size,
                sent: t.sent,
                state: t.state.clone(),
                error: t.error.clone(),
                direction: Some(t.direction.clone()),
                finished_at: t.finished_at,
                path: t.path.clone(),
            }
        }
    }
}
use contract::{SendTarget, Transfer};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ClipboardMode {
    #[default]
    Automatic,
    Manual,
    Off,
}
impl ClipboardMode {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
            Self::Off => "off",
        }
    }
    /// Before the backend reports `clipboard_mode`, the v2 switch decides.
    pub(crate) fn from_config(value: Option<&Value>, sync_to_phone: bool) -> Self {
        match value.and_then(Value::as_str) {
            Some("manual") => Self::Manual,
            Some("off") => Self::Off,
            Some(_) => Self::Automatic,
            None if sync_to_phone => Self::Automatic,
            None => Self::Off,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct View {
    revision: i32,
    page: Page,
    device: String,
    add_tab: i32,
    // The bindings displayed by this UI frame, including queued callbacks.
    targets: Option<Vec<SendTarget>>,
    // State installed with the displayed frame, carried through queued actions.
    setup: Arc<Setup>,
    top: Arc<Top>,
}

// Status updates may change connectivity while a picker is open. They do not
// change navigation scope: captured bindings travel with the action and Core
// rejects a replacement pairing instead of silently discarding the selection.
impl PartialEq for View {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision
            && self.page == other.page
            && self.device == other.device
            && self.add_tab == other.add_tab
    }
}

enum Action {
    Scoped(View, Box<Action>),
    Close,
    #[cfg(target_os = "macos")]
    Native(super::macos::native::MenuAction),
    #[cfg(any(target_os = "macos", all(target_os = "linux", pneu_qt_drag), windows))]
    DropFiles {
        files: Vec<PathBuf>,
        targets: Vec<contract::CapturedTarget>,
        origin: View,
        targeted: bool,
    },
    Navigated,
    Folder,
    OpenFolder,
    Unpair(String),
    SendFiles(String),
    PickerSend(Vec<String>),
    PickerCancel,
    SendClipboard,
    CopyHistory(String),
    DisableEmptyHistory,
    DeleteHistory(String),
    PinHistory(String, bool),
    ClearHistory,
    Cancel(String),
    Retry(String),
    Dismiss(String),
    Details(String),
    Recipient(String, String, bool),
    Open(String),
    SetPeer(String, bool),
    SetMode(i32),
    SetFlag(String, bool),
    Rename(String),
    Login(bool),
    SystemSettings(String),
    ShowCode,
    NewCode,
    PasteLink(String),
    ScannedLink(String),
    Probed(String),
    Focused,
    OfferPair(bool),
    OfferDismiss,
    ConfirmJoin(bool),
    TopCard(bool),
    TopCardClose,
    LinkDevice(String),
    Answer(String, bool),
    Removing(String),
    Remove(String),
    Message(String, bool),
    GetTailscale,
}

/// IPC currently exposes daemon rejections as this prefix; socket/timeout/
/// decoding failures do not have it and cannot establish an attempt's fate.
fn daemon_rejection(error: &anyhow::Error) -> bool {
    error.to_string().starts_with("daemon command failed:")
}

// ---------- What the window shows, computed from daemon state ----------

#[derive(Clone, Debug, Default, PartialEq)]
struct LinkData {
    self_tone: Tone,
    tiles: Vec<LinkTile>,
    slots: i32,
    tube: Tube,
    capsule: i32,
}
impl LinkData {
    fn state(&self) -> LinkState {
        LinkState {
            self_tone: self.self_tone,
            tiles: ModelRc::new(VecModel::from(self.tiles.clone())),
            slots: self.slots,
            tube: self.tube,
            capsule: self.capsule,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
struct SummaryData {
    title: String,
    detail: String,
    link: LinkData,
}
impl SummaryData {
    fn summary(&self) -> Summary {
        Summary {
            title: self.title.as_str().into(),
            detail: self.detail.as_str().into(),
            link: self.link.state(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Connected,
    Paused,
    Connecting,
    Offline,
    Update,
    Lost,
    Mismatch,
}
impl Health {
    fn of(state: &str, paused: bool) -> Self {
        match state {
            "connected" if paused => Self::Paused,
            "connected" => Self::Connected,
            "connecting" => Self::Connecting,
            "update_required" => Self::Update,
            "unauthorized" | "pairing_lost" => Self::Lost,
            "fingerprint_mismatch" | "binding_mismatch" => Self::Mismatch,
            _ => Self::Offline,
        }
    }
    fn connected(self) -> bool {
        matches!(self, Self::Connected | Self::Paused)
    }
    fn lost(self) -> bool {
        matches!(self, Self::Lost | Self::Mismatch)
    }
    fn tone(self) -> Tone {
        match self {
            Self::Connected => Tone::Accent,
            Self::Paused => Tone::Warn,
            Self::Lost | Self::Mismatch => Tone::Danger,
            _ => Tone::Neutral,
        }
    }
}

/// One paired device as the window and tray describe it.
#[derive(Clone, Debug)]
pub(crate) struct Peer {
    pub id: String,
    pub name: String,
    pub phone: bool,
    health: Health,
    pub last_seen: u64,
    pub route: Option<String>,
    sending: Option<Transfer>,
    receiving: Option<Transfer>,
}
impl Peer {
    pub(crate) fn connected(&self) -> bool {
        self.health.connected()
    }
    /// v2's per-device line, without transfers ("Connected over local network").
    pub(crate) fn connection(&self) -> String {
        match self.health {
            Health::Connected => match &self.route {
                Some(route) => format!("Connected over {}", super::route_in_sentence(route)),
                None => "Connected".into(),
            },
            Health::Paused => "Paused on phone".into(),
            Health::Connecting => "Connecting…".into(),
            Health::Update => "Update required".into(),
            Health::Lost | Health::Mismatch => "Pairing lost".into(),
            Health::Offline if self.last_seen == 0 => "Not connected yet".into(),
            Health::Offline => format!("Not connected · last seen {}", seen(self.last_seen)),
        }
    }
    pub(crate) fn paused(&self) -> bool {
        self.health == Health::Paused
    }
    pub(crate) fn lost(&self) -> bool {
        self.health.lost()
    }
    pub(crate) fn sending(&self) -> bool {
        self.sending.is_some()
    }
    pub(crate) fn receiving(&self) -> bool {
        self.receiving.is_some()
    }
    /// Tooltip line: "Galaxy S25+ · Local network", "Office-PC · last seen 9:02 AM".
    pub(crate) fn short(&self) -> String {
        let state = match self.health {
            Health::Connected => self.route.clone().unwrap_or_else(|| "Connected".into()),
            Health::Paused => "Paused".into(),
            Health::Offline if self.last_seen != 0 => format!("last seen {}", seen(self.last_seen)),
            _ => self.connection(),
        };
        format!("{} · {state}", self.name)
    }
    fn item(&self) -> DeviceItem {
        let (status, progress) = if let Some(t) = &self.sending {
            (
                format!("Sending {} · {}", t.file_name, size_pair(t.sent, t.size)),
                fraction(t.sent, t.size),
            )
        } else if let Some(t) = &self.receiving {
            (
                format!("Receiving {} · {}%", t.file_name, percent(t.sent, t.size)),
                fraction(t.sent, t.size),
            )
        } else {
            (self.connection(), -1.0)
        };
        let busy = self.sending.is_some() || self.receiving.is_some();
        DeviceItem {
            id: self.id.as_str().into(),
            name: self.name.as_str().into(),
            phone: self.phone,
            tone: self.health.tone(),
            status: status.into(),
            status_tone: match self.health {
                _ if busy => Tone::Neutral,
                Health::Paused | Health::Update => Tone::Warn,
                Health::Lost | Health::Mismatch => Tone::Danger,
                _ => Tone::Neutral,
            },
            progress,
            connected: self.connected(),
        }
    }
    fn tile(&self, slot: i32) -> LinkTile {
        LinkTile {
            phone: self.phone,
            tone: self.health.tone(),
            more: 0,
            slot,
        }
    }
}

/// Devices in the daemon's order (connected first, then most recently seen).
pub(crate) fn peers(snapshot: &super::Snapshot, transfers: &[Transfer]) -> Vec<Peer> {
    snapshot
        .devices
        .iter()
        .map(|d| {
            let id = d.device.as_str().to_owned();
            let paused = d.role == "phone" && snapshot.paused_devices.contains(&d.device);
            let mut health = Health::of(&d.state, paused);
            if snapshot.refreshing_devices.contains(&d.device)
                && matches!(health, Health::Offline | Health::Connecting)
            {
                health = if paused {
                    Health::Paused
                } else {
                    Health::Connected
                };
            }
            let active = |incoming: bool| {
                transfers
                    .iter()
                    .find(|t| t.device == id && t.incoming() == incoming && t.state == "sending")
                    .cloned()
            };
            let route = health.connected().then(|| d.route.clone()).flatten();
            Peer {
                name: display_peer_text(&d.name),
                phone: d.role == "phone",
                health,
                last_seen: d.last_seen,
                route,
                sending: active(false),
                receiving: active(true).filter(|_| health.connected()),
                id,
            }
        })
        .collect()
}

fn stack(peers: &[Peer]) -> (Vec<LinkTile>, i32) {
    let mut tiles: Vec<LinkTile> = if peers.len() > 3 {
        let mut tiles: Vec<_> = peers[..2]
            .iter()
            .enumerate()
            .map(|(i, p)| p.tile(i as i32))
            .collect();
        tiles.push(LinkTile {
            phone: false,
            tone: Tone::Neutral,
            more: peers.len() as i32 - 2,
            slot: 2,
        });
        tiles
    } else {
        peers
            .iter()
            .enumerate()
            .map(|(i, p)| p.tile(i as i32))
            .collect()
    };
    let slots = tiles.len() as i32;
    // Drawn back to front: the first device sits in front.
    tiles.reverse();
    (tiles, slots)
}

fn summary(peers: &[Peer], transfers: &[Transfer]) -> SummaryData {
    let total = peers.len();
    let connected: Vec<_> = peers.iter().filter(|p| p.connected()).collect();
    let paused = !connected.is_empty() && connected.iter().all(|p| p.health == Health::Paused);
    let lost = total > 0 && peers.iter().all(|p| p.health.lost());
    let sending = transfers
        .iter()
        .find(|t| !t.incoming() && t.state == "sending");
    let receiving = peers.iter().find(|p| p.receiving.is_some());
    let (tiles, slots) = stack(peers);
    let tube = if paused {
        Tube::Paused
    } else if !connected.is_empty() {
        Tube::Solid
    } else if lost {
        Tube::Broken
    } else if peers.iter().any(|p| p.health == Health::Connecting) {
        Tube::March
    } else {
        Tube::Dashed
    };
    let capsule = if tube != Tube::Solid {
        0
    } else if sending.is_some() {
        1
    } else if receiving.is_some() {
        2
    } else {
        0
    };
    let link = LinkData {
        self_tone: if connected.is_empty() {
            Tone::Neutral
        } else {
            Tone::Accent
        },
        tiles,
        slots,
        tube,
        capsule,
    };
    let last = peers.iter().map(|p| p.last_seen).max().unwrap_or(0);
    let (title, detail) = if lost {
        (
            "Pairing lost".to_owned(),
            match peers {
                [one] => format!("{} doesn't recognize {}", one.name, this_computer()),
                _ => format!("{total} devices don't recognize {}", this_computer()),
            },
        )
    } else if let [one] = peers {
        let device = device_summary(one).0;
        (device.title, device.detail)
    } else if paused {
        ("Paused".into(), "Automatic sending is off".into())
    } else if !connected.is_empty() {
        let detail = if let Some(t) = sending {
            format!("Sending {}…", t.file_name)
        } else if let Some(p) = receiving {
            format!("Receiving from {}…", p.name)
        } else if connected.len() == total && total == 2 {
            format!("{} and {}", connected[0].name, connected[1].name)
        } else if connected.len() == total {
            format!("{total} devices")
        } else {
            format!("{} of {total} devices", connected.len())
        };
        ("Connected".into(), detail)
    } else if tube == Tube::March {
        ("Connecting…".into(), format!("{total} devices"))
    } else if last != 0 {
        (
            "Not connected".into(),
            format!("Last connected {}", seen(last)),
        )
    } else {
        ("Not connected".into(), String::new())
    };
    SummaryData {
        title,
        detail,
        link,
    }
}

/// No paired device can be reached, so the main page offers Can't connect?.
/// A device that answered (even to refuse the pairing) counts as reachable.
fn unreachable(peers: &[Peer]) -> bool {
    !peers.is_empty()
        && peers
            .iter()
            .all(|p| matches!(p.health, Health::Offline | Health::Connecting))
}

/// Tailscale runs here when one of this computer's addresses is in its ranges.
fn tailscale_running() -> bool {
    crate::discovery::candidates()
        .is_ok_and(|addresses| addresses.iter().any(|a| super::route(a.ip) == "Tailscale"))
}

/// The device page's status card: one-to-one Link, title, detail and action.
fn device_summary(peer: &Peer) -> (SummaryData, &'static str) {
    let h = peer.health;
    let tube = match h {
        Health::Connected => Tube::Solid,
        Health::Paused => Tube::Paused,
        Health::Connecting => Tube::March,
        Health::Lost | Health::Mismatch => Tube::Broken,
        Health::Offline | Health::Update => Tube::Dashed,
    };
    let (title, detail) = match h {
        Health::Connected => (
            "Connected".to_owned(),
            if let Some(t) = &peer.sending {
                format!("Sending {} · {}", t.file_name, size_pair(t.sent, t.size))
            } else if let Some(t) = &peer.receiving {
                format!("Receiving {} · {}%", t.file_name, percent(t.sent, t.size))
            } else {
                peer.route.clone().unwrap_or_default()
            },
        ),
        Health::Paused => ("Paused".into(), "Automatic sending is off".into()),
        Health::Connecting => ("Connecting…".into(), String::new()),
        Health::Update => (
            "Update required".into(),
            format!("Update Pneu on {}", peer.name),
        ),
        Health::Lost => (
            "Pairing lost".into(),
            format!("{} doesn't recognize {}", peer.name, this_computer()),
        ),
        Health::Mismatch => (
            "Pairing lost".into(),
            "A different device answered at its address".into(),
        ),
        Health::Offline if peer.last_seen != 0 => (
            "Not connected".into(),
            format!("Last connected {}", seen(peer.last_seen)),
        ),
        Health::Offline => ("Not connected yet".into(), String::new()),
    };
    let capsule = match (&peer.sending, &peer.receiving) {
        _ if tube != Tube::Solid => 0,
        (Some(_), _) => 1,
        (_, Some(_)) => 2,
        _ => 0,
    };
    (
        SummaryData {
            title,
            detail,
            link: LinkData {
                self_tone: if h.connected() {
                    Tone::Accent
                } else {
                    Tone::Neutral
                },
                tiles: vec![peer.tile(0)],
                slots: 1,
                tube,
                capsule,
            },
        },
        if h.lost() { "pair" } else { "send" },
    )
}

/// Switch labels describe the pair from this computer's side.
fn device_settings(peer: &Peer, reply: &Value, mode: ClipboardMode) -> (Vec<SettingItem>, String) {
    if peer.health.lost() {
        return (Vec::new(), String::new());
    }
    if reply["can_sync"] == json!(false) {
        if peer.last_seen == 0 && !peer.connected() {
            return (
                Vec::new(),
                format!("Switches sync when {} connects.", peer.name),
            );
        }
        return (
            Vec::new(),
            format!("Update Pneu on {} to sync these switches.", peer.name),
        );
    }
    let mut items: Vec<(u8, SettingItem)> = reply["settings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let key = s["key"].as_str()?;
            let mut parts = key.split('/');
            let (from, to, feature) = (parts.next()?, parts.next()?, parts.next()?);
            let (order, label) = match feature {
                "clipboard" if to == peer.id => (0, "Send clipboard"),
                "clipboard" if from == peer.id => (1, "Receive clipboard"),
                "automatic_media" if from == peer.id => (2, "Receive automatic photos"),
                _ => return None,
            };
            Some((
                order,
                SettingItem {
                    key: key.into(),
                    label: label.into(),
                    enabled: order != 0 || mode != ClipboardMode::Off,
                    sub: if order == 0 && mode == ClipboardMode::Off {
                        "Off for all devices in Settings".into()
                    } else if s["pending"] == json!(true) && !peer.connected() {
                        format!("Applies when {} connects", peer.name).into()
                    } else {
                        SharedString::new()
                    },
                    on: !(order == 0 && mode == ClipboardMode::Off)
                        && s["enabled"].as_bool().unwrap_or(true),
                },
            ))
        })
        .collect();
    items.sort_by_key(|(order, _)| *order);
    (
        items.into_iter().map(|(_, item)| item).collect(),
        String::new(),
    )
}

/// An artifact keeps its own row even when several files share an operation.
fn artifact_key(t: &Transfer) -> String {
    if !t.transfer_id.is_empty() && t.transfer_id == t.operation {
        return format!(
            "{}/admission/{}",
            t.operation,
            t.path
                .as_ref()
                .map(|p| p.to_string_lossy())
                .unwrap_or_default()
        );
    }
    format!(
        "{}/{}",
        t.operation,
        if t.transfer_id.is_empty() {
            &t.file_name
        } else {
            &t.transfer_id
        }
    )
}
fn artifact_id(t: &Transfer) -> Option<&str> {
    (!t.transfer_id.is_empty() && t.transfer_id != t.operation).then_some(t.transfer_id.as_str())
}
fn has_recipient(t: &Transfer) -> bool {
    !(t.transfer_id == t.operation && t.device == "00000000000000000000000000000000")
}
fn distinct_recipients(list: &[&Transfer]) -> usize {
    list.iter()
        .filter(|t| has_recipient(t))
        .map(|t| &t.device)
        .collect::<HashSet<_>>()
        .len()
}
fn recipient_details(peers: &[Peer], transfers: &[Transfer], key: &str) -> Vec<RecipientItem> {
    let mut seen = HashSet::new();
    transfers
        .iter()
        .filter(|t| artifact_key(t) == key && has_recipient(t) && seen.insert(t.device.clone()))
        .map(|t| {
            let peer = peers.iter().find(|p| p.id == t.device);
            let active = matches!(
                t.state.as_str(),
                "sending" | "queued" | "waiting_for_device" | "staging"
            );
            let detail = match t.state.as_str() {
                "sending" => format!("Sending · {}", size_pair(t.sent, t.size)),
                "staging" => "Preparing…".into(),
                "queued" | "waiting_for_device" => "Waiting · not connected".into(),
                "failed" => transfer_failure(
                    t,
                    peer.map(|p| p.name.as_str()).unwrap_or("a removed device"),
                ),
                "sent" => t
                    .finished_at
                    .map(|at| format!("Sent · {}", super::last_seen(at)))
                    .unwrap_or_else(|| "Sent".into()),
                "cancelled" => "Stopped".into(),
                other => other.into(),
            };
            RecipientItem {
                item: artifact_key(t).into(),
                device: t.device.as_str().into(),
                name: peer
                    .map(|p| p.name.as_str())
                    .unwrap_or("a removed device")
                    .into(),
                phone: peer.is_some_and(|p| p.phone),
                tone: if t.state == "failed" {
                    Tone::Danger
                } else {
                    peer.map(|p| p.health.tone()).unwrap_or_default()
                },
                detail: detail.into(),
                progress: if t.state == "sending" {
                    fraction(t.sent, t.size)
                } else {
                    -1.0
                },
                can_stop: active,
                can_retry: t.state == "failed" && artifact_id(t).is_some(),
            }
        })
        .collect()
}

fn transfer_failure(t: &Transfer, name: &str) -> String {
    if matches!(
        t.error.as_deref(),
        Some("Connection lost" | "Connection interrupted")
    ) {
        if t.sent > 0 {
            format!("Connection lost · {} sent", size_pair(t.sent, t.size))
        } else {
            "Connection lost".into()
        }
    } else {
        super::notifications::send_failure_reason(name, t.error.as_deref().unwrap_or_default())
    }
}

/// Recent: active, waiting and failed items first, then the last three done.
fn recent(peers: &[Peer], transfers: &[Transfer]) -> Vec<RecentItem> {
    let name = |id: &str| {
        let name = peers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "a removed device".into());
        if name.chars().count() > 24 {
            format!("{}…", name.chars().take(23).collect::<String>())
        } else {
            name
        }
    };
    let mut operations: Vec<(String, Vec<&Transfer>)> = Vec::new();
    for t in transfers {
        let key = artifact_key(t);
        match operations.iter_mut().find(|(op, _)| *op == key) {
            Some((_, list)) => list.push(t),
            None => operations.push((key, vec![t])),
        }
    }
    operations.sort_by_key(|(_, list)| {
        std::cmp::Reverse(list.iter().filter_map(|t| t.finished_at).max().unwrap_or(0))
    });
    let (mut open, mut done) = (Vec::new(), Vec::new());
    for (operation, list) in operations {
        let first = list[0];
        let live: Vec<_> = list
            .iter()
            .copied()
            .filter(|t| t.state != "cancelled")
            .collect();
        if live.is_empty() {
            continue;
        }
        let picture = is_picture(&first.file_name);
        let mut item = RecentItem {
            id: operation.into(),
            can_details: !first.incoming() && list.iter().any(|t| has_recipient(t)),
            title: first.file_name.as_str().into(),
            batch: artifact_id(first).is_none(),
            picture,
            video: is_video(&first.file_name),
            progress: -1.0,
            ..Default::default()
        };
        let when = live
            .iter()
            .filter_map(|t| t.finished_at)
            .max()
            .map(super::last_seen);
        let with_time = |text: String| match &when {
            Some(time) if text.is_empty() => time.clone(),
            Some(time) => format!("{text} · {time}"),
            None if text.is_empty() => "Sent".into(),
            None => text,
        };
        let sending: Vec<_> = live
            .iter()
            .filter(|t| matches!(t.state.as_str(), "sending" | "staging"))
            .copied()
            .collect();
        let waiting: Vec<_> = live
            .iter()
            .filter(|t| matches!(t.state.as_str(), "queued" | "waiting_for_device"))
            .copied()
            .collect();
        let failed: Vec<_> = live
            .iter()
            .filter(|t| t.state == "failed")
            .copied()
            .collect();
        if first.incoming() {
            if !sending.is_empty() {
                continue; // The sender's device row shows incoming progress.
            }
            item.detail = with_time(format!("From {}", name(&first.device))).into();
            item.can_open = first.path.is_some();
            done.push(item);
            continue;
        }
        if !sending.is_empty() {
            let (sent, size) = live
                .iter()
                .fold((0, 0), |(s, z), t| (s + t.sent, z + t.size));
            item.detail = if sending.iter().all(|t| t.state == "staging") {
                "Preparing…".into()
            } else {
                match distinct_recipients(&live) {
                    1 => format!("Sending · {}", size_pair(sent, size)),
                    n => format!("Sending to {n} devices · {}", size_pair(sent, size)),
                }
            }
            .into();
            item.progress = if sending.iter().all(|t| t.state == "staging") {
                -1.0
            } else {
                fraction(sent, size)
            };
            item.can_cancel = true;
            open.push(item);
        } else if !waiting.is_empty() {
            item.detail = if waiting.len() == live.len() && live.len() > 1 {
                "Waiting".into()
            } else if let [one] = waiting.as_slice() {
                format!("Waiting for {}", name(&one.device))
            } else {
                format!("Waiting for {} devices", distinct_recipients(&waiting))
            }
            .into();
            item.can_cancel = true;
            open.push(item);
        } else if !failed.is_empty() {
            item.detail = match failed.as_slice() {
                [one] if !has_recipient(one) => one
                    .error
                    .clone()
                    .unwrap_or_else(|| "Couldn't send this item".into()),
                [one] => transfer_failure(one, &name(&one.device)),
                many => format!("Failed on {} devices", distinct_recipients(many)),
            }
            .into();
            item.detail_tone = Tone::Danger;
            item.can_retry = failed.iter().any(|t| artifact_id(t).is_some());
            item.can_dismiss = !transfers.iter().any(|t| {
                (if artifact_id(first).is_some() {
                    artifact_key(t) == artifact_key(first)
                } else {
                    t.operation == first.operation
                }) && matches!(
                    t.state.as_str(),
                    "sending" | "staging" | "queued" | "waiting_for_device"
                )
            });
            open.push(item);
        } else {
            let sent: Vec<_> = live.iter().filter(|t| t.state == "sent").copied().collect();
            item.detail = with_time(match sent.as_slice() {
                [one, ..] if distinct_recipients(&sent) == 1 => format!("To {}", name(&one.device)),
                many => format!("To {} devices", distinct_recipients(many)),
            })
            .into();
            item.can_open = first.path.is_some();
            done.push(item);
        }
    }
    done.truncate(3);
    open.extend(done);
    open
}

/// `last_seen` inside a sentence: "last seen yesterday", not "Yesterday".
fn seen(timestamp: u64) -> String {
    match super::last_seen(timestamp) {
        day if day == "Yesterday" => "yesterday".into(),
        time => time,
    }
}
fn is_picture(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".heic", ".bmp", ".avif",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}
fn is_video(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".mp4", ".mov", ".m4v", ".webm", ".mkv", ".3gp", ".avi"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}
fn fraction(sent: u64, size: u64) -> f32 {
    if size == 0 {
        0.0
    } else {
        (sent as f64 / size as f64).clamp(0.0, 1.0) as f32
    }
}
fn percent(sent: u64, size: u64) -> u32 {
    (fraction(sent, size) * 100.0).floor() as u32
}
/// "12 of 40 MB": both numbers in the total's unit.
fn size_pair(sent: u64, size: u64) -> String {
    let (unit, scale) = match size {
        0..1_000 => ("B", 1.0),
        1_000..1_000_000 => ("KB", 1e3),
        1_000_000..1_000_000_000 => ("MB", 1e6),
        _ => ("GB", 1e9),
    };
    let number = |bytes: u64| {
        let value = bytes as f64 / scale;
        if value < 10.0 && scale > 1.0 && (unit == "GB" || value.fract() >= 0.05) {
            format!("{value:.1}")
        } else {
            format!("{value:.0}")
        }
    };
    format!("{} of {} {unit}", number(sent.min(size)), number(size))
}
pub(crate) fn files_title(paths: &[PathBuf], prefix: &str) -> String {
    match paths {
        [one] => format!(
            "{prefix} {} to",
            one.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        ),
        many => format!("{prefix} {} files to", many.len()),
    }
}
// ---------- Window state that the worker keeps between status polls ----------

#[derive(Clone, Debug, Default, PartialEq)]
struct Frame {
    error: String,
    summary: SummaryData,
    devices: Vec<DeviceItem>,
    recent: Vec<RecentItem>,
    history: Vec<crate::store::history::Item>,
    keep_history: bool,
    manual_clipboard: bool,
    pair_again: bool,
    motion: bool,
    device: DeviceItem,
    device_summary: SummaryData,
    device_action: String,
    device_settings: Vec<SettingItem>,
    device_note: String,
    device_link: DeviceLink,
    top: TopCard,
    link_full: bool,
    paired_detail: String,
    paired_only: bool,
    first_run: bool,
    network: bool,
    pairing: String,
    paired: String,
    offer: String,
    code_state: CodeState,
    code_expires: String,
    code_error: String,
    code_link: String,
    clipboard_mode: i32,
    include_images: bool,
    choose_each_time: bool,
    queue_offline: bool,
    save_to_folder: bool,
    folder: String,
    notifications: bool,
    copy_images: bool,
    viewer: bool,
    name: String,
    unreachable: bool,
    tailscale: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Join {
    attempt: String,
    uri: String,
    name: String,
    confirmed: bool,
    #[serde(default)]
    cancel_requested: bool,
    // The offer card's attempt is a preview the user hasn't chosen yet.
    offer: bool,
    // Also link with your other devices, frozen when the user confirms.
    #[serde(default = "linked_by_default")]
    link: bool,
    #[serde(skip)]
    origin: View,
    reported: String,
}

fn linked_by_default() -> bool {
    true
}

struct Paired {
    origin: View,
    name: String,
    device: Option<String>,
    at: Instant,
    linked_at: Option<Instant>,
    linked: String,
}

#[derive(Default)]
struct Code {
    token: Option<pneu_proto::Hash>,
    uri: String,
    expires_at: u64,
    offline: bool,
    error: String,
    last_attempt: u64,
    shown: HashSet<String>,
}

pub fn run(paths: Paths) -> Result<()> {
    let startup_request = std::env::var("PNEU_WINDOW_REQUEST")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|request| request & 1 == 0)
        .unwrap_or(0);
    #[cfg(target_os = "linux")]
    {
        use slint::winit_030::winit::{
            platform::startup_notify::WindowAttributesExtStartupNotify, window::ActivationToken,
        };
        let startup_activation =
            std::cell::RefCell::new(std::env::var("XDG_ACTIVATION_TOKEN").ok());
        slint::BackendSelector::new()
            .with_winit_window_attributes_hook(move |attributes| {
                if let Some(token) = startup_activation.borrow_mut().take() {
                    attributes.with_activation_token(ActivationToken::from_raw(token))
                } else {
                    attributes
                }
            })
            .select()?;
        slint::set_xdg_app_id("pneu")?;
    }
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::{
            EventLoopBuilder,
            winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS},
        };
        let mut builder: EventLoopBuilder =
            slint::winit_030::winit::event_loop::EventLoop::with_user_event();
        builder
            .with_activation_policy(ActivationPolicy::Accessory)
            .with_default_menu(false);
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .with_winit_event_loop_builder(builder)
            .select()?;
    }
    #[cfg(windows)]
    {
        use slint::winit_030::{
            EventLoopBuilder,
            winit::platform::windows::{EventLoopBuilderExtWindows, WindowAttributesExtWindows},
        };
        let mut builder: EventLoopBuilder =
            slint::winit_030::winit::event_loop::EventLoop::with_user_event();
        // Both executables embed PerMonitorV2. Avoid winit's redundant process
        // awareness calls (which Windows rejects once a manifest has set it).
        // This leaves winit/Slint's per-window DPI handling enabled.
        builder.with_dpi_aware(false);
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("software".into())
            .with_winit_event_loop_builder(builder)
            .with_winit_window_attributes_hook(|attributes| attributes.with_drag_and_drop(false))
            .select()?;
    }
    let window = PneuWindow::new()?;
    let visible = Arc::new(AtomicBool::new(
        cfg!(any(target_os = "linux", windows)) || startup_request != 0,
    ));
    let focused = Arc::new(AtomicBool::new(false));
    let view = Arc::new(Mutex::new(View::default()));
    let (commands, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let focus = focused.clone();
    let focus_commands = commands.clone();
    window.global::<WindowFocus>().on_changed(move |value| {
        let was_focused = focus.swap(value, Ordering::SeqCst);
        if value && !was_focused {
            let _ = focus_commands.send(Action::Focused);
        }
    });
    connect(&window, &commands, &view);
    let c = commands.clone();
    window.window().on_close_requested(move || {
        #[cfg(target_os = "macos")]
        super::macos::camera::close();
        let _ = c.send(Action::Close);
        slint::CloseRequestResponse::KeepWindowShown
    });
    #[cfg(target_os = "macos")]
    {
        window.global::<Ui>().set_is_mac(true);
        let commands_for_drop = commands.clone();
        let commands = commands.clone();
        super::macos::native::install(&window, paths.cache.clone(), move |action| {
            let _ = commands.send(Action::Native(action));
        })?;
        super::macos::camera::install(&window);
        install_drop(&window, &view, &commands_for_drop);
    }
    #[cfg(windows)]
    {
        window.global::<Ui>().set_drop_supported(true);
        install_drop(&window, &view, &commands);
    }
    #[cfg(all(target_os = "linux", pneu_qt_drag))]
    install_drop(&window, &view, &commands);
    #[cfg(windows)]
    let _login_poll = super::windows_login::install(&window);
    let weak = window.as_weak();
    let worker = std::thread::Builder::new().name("pneu-window-ipc".into()).stack_size(8 * 1024 * 1024).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("UI runtime");
        runtime.block_on(Box::pin(async move {
            let mut worker = Worker::new(paths, weak, view, startup_request);
            let mut timer = tokio::time::interval(Duration::from_millis(500));
            let mut invitation_job: Option<futures_util::future::BoxFuture<'static, Result<Value>>> = None;
            loop {
                if invitation_job.is_none() { let socket=worker.paths.socket.clone(); let revision=worker.invitation_revision; invitation_job=Some(Box::pin(async move {ipc::request(&socket, ipc::Command::WaitPairingChange {revision}).await})); }
                tokio::select! {
                    _ = async {invitation_job.as_mut().expect("invitation subscription").await} => {invitation_job=None;},
                    _ = timer.tick() => {},
                    result = job_result(&mut worker.folder_job) => { worker.folder_job = None; if let Some(origin) = worker.folder_origin.take() { worker.origin = origin; if let Err(error) = result { worker.fail("Couldn't change the folder", error); } } },
                    result = job_result(&mut worker.open_folder_job) => { worker.opened_folder(result); },
                    result = job_result(&mut worker.open_job) => { worker.open_job = None; if let Some(origin) = worker.open_origin.take() { worker.origin = origin; if let Err(error) = result { let message = if error.is::<crate::actions::FileUnavailable>() { "The file was moved or deleted" } else { "Couldn't open it" }; worker.fail(message, error); } } },
                    result = job_result(&mut worker.chooser) => { worker.chooser = None; if let Some(origin) = worker.chooser_origin.take() { worker.origin = origin; if worker.current() { worker.chosen(result).await; } } },
                    command = rx.recv() => {
                        match command {
                            Some(Action::Close) => {
                                worker.close(&visible).await;
                                #[cfg(windows)]
                                let _ = ipc::request(&worker.paths.socket, ipc::Command::WindowClosed { request: worker.request }).await;
                            }
                            Some(action) => worker.act(action).await,
                            None => break,
                        }
                    }
                }
                let _ = ipc::request(&worker.paths.socket,ipc::Command::WindowFocus { request: worker.request, focused: visible.load(Ordering::SeqCst) && focused.load(Ordering::SeqCst) }).await;
                let value = match ipc::request(&worker.paths.socket,ipc::Command::UiStatus).await {
                    Ok(value) => value,
                    Err(error) => { worker.status_failed(error); continue; }
                };
                if let Err(error) = worker.refresh(value, &visible).await { worker.status_failed(error); }
            }
            let _ = ipc::request(&worker.paths.socket,ipc::Command::WindowFocus { request: worker.request, focused: false }).await;
            drop(invitation_job);
            worker.release().await;
            let _ = slint::quit_event_loop();
        }));
    });
    let worker = worker.context("start window IPC thread")?;
    #[cfg(any(target_os = "linux", windows))]
    window.show()?;
    #[cfg(windows)]
    {
        let weak = window.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                super::windows_window::activate(&window);
            }
        });
    }
    #[cfg(target_os = "macos")]
    if startup_request != 0 {
        window.show()?;
        let weak = window.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(w) = weak.upgrade() {
                super::macos::native::activate(&w);
            }
        });
    }
    #[cfg(target_os = "macos")]
    slint::Timer::single_shot(Duration::ZERO, || {
        if let Err(error) = super::macos::native::ready() {
            eprintln!("pneud: UI readiness: {error:#}");
            let _ = slint::quit_event_loop();
        }
    });
    let event_loop_result = slint::run_event_loop_until_quit();
    #[cfg(target_os = "macos")]
    super::macos::native::cleanup();
    #[cfg(windows)]
    super::windows_drag::cleanup();
    #[cfg(all(target_os = "linux", pneu_qt_drag))]
    super::linux_drag::cleanup();
    drop(window);
    drop(commands);
    let _ = worker.join();
    event_loop_result?;
    Ok(())
}

#[cfg(any(target_os = "macos", all(target_os = "linux", pneu_qt_drag), windows))]
fn install_drop(
    window: &PneuWindow,
    view: &Arc<Mutex<View>>,
    commands: &tokio::sync::mpsc::UnboundedSender<Action>,
) {
    #[cfg(all(target_os = "linux", pneu_qt_drag))]
    use super::linux_drag as native_drop;
    #[cfg(target_os = "macos")]
    use super::macos::drag as native_drop;
    #[cfg(windows)]
    use super::windows_drag as native_drop;
    #[cfg(target_os = "macos")]
    use slint::winit_030::{EventResult, WinitWindowAccessor};
    struct DropSession {
        origin: View,
        targets: Vec<Vec<contract::CapturedTarget>>,
        enabled: Vec<bool>,
    }
    impl DropSession {
        fn enabled_now(&self, window: &PneuWindow) -> Vec<bool> {
            let peers = window.global::<Ui>().get_devices();
            self.enabled
                .iter()
                .enumerate()
                .map(|(index, enabled)| {
                    *enabled
                        && super::drop_target::has_connected_target(
                            self.targets[index]
                                .iter()
                                .map(|target| target.device.as_str()),
                            |id| {
                                peers
                                    .iter()
                                    .any(|peer| peer.id.as_str() == id && peer.connected)
                            },
                        )
                })
                .collect()
        }
        fn hit(&self, window: &PneuWindow, point: (f64, f64)) -> Option<usize> {
            let enabled = self.enabled_now(window);
            let ui = window.global::<Ui>();
            let mut items = ui.get_drop_items().iter().collect::<Vec<_>>();
            for (item, enabled) in items.iter_mut().zip(&enabled) {
                item.enabled = *enabled;
            }
            sync(ui.get_drop_items(), &items);
            let g = window.get_drop_geometry();
            super::drop_target::hit(
                super::drop_target::Geometry {
                    x: g.x as f64,
                    y: g.y as f64,
                    width: g.width as f64,
                    height: g.height as f64,
                    scroll: g.scroll as f64,
                    row_height: 56.0,
                },
                point,
                &enabled,
            )
        }
    }
    let session = std::rc::Rc::new(std::cell::RefCell::new(None::<DropSession>));
    let pending = session.clone();
    let weak = window.as_weak();
    let frame = view.clone();
    let begin = std::rc::Rc::new(move || {
        let Some(w) = weak.upgrade() else {
            return false;
        };
        let ui = w.global::<Ui>();
        if ui.get_dialog() != Dialog::None {
            return false;
        }
        #[cfg(target_os = "macos")]
        if !super::macos::native::accepts_drop() {
            return false;
        }
        let origin = frame.lock().expect("drop frame").clone();
        let bindings = origin.targets.clone().unwrap_or_default();
        let all =
            contract::resolve_files(&bindings, crate::file_recipients::Selection::ConnectedOnly)
                .unwrap_or_default();
        let mut targets = vec![all];
        let mut labels = vec![DropItem {
            name: "All connected devices".into(),
            enabled: !targets[0].is_empty(),
        }];
        for device in ui.get_devices().iter() {
            let authority = bindings
                .iter()
                .find(|t| t.device == device.id.as_str())
                .and_then(|t| {
                    Some(contract::CapturedTarget {
                        device: t.device.clone(),
                        generation: t.generation,
                        fingerprint: t.fingerprint.clone()?,
                    })
                });
            labels.push(DropItem {
                name: device.name,
                enabled: device.connected && authority.is_some(),
            });
            targets.push(authority.into_iter().collect());
        }
        let enabled = labels.iter().map(|l| l.enabled).collect();
        ui.set_drop_items(ModelRc::new(VecModel::from(labels)));
        ui.set_drop_active(true);
        *pending.borrow_mut() = Some(DropSession {
            origin,
            targets,
            enabled,
        });
        true
    });
    let pending = session.clone();
    let weak = window.as_weak();
    let hover = std::rc::Rc::new(move |point| {
        let Some(w) = weak.upgrade() else {
            return false;
        };
        let selected = pending
            .borrow()
            .as_ref()
            .filter(|_| w.global::<Ui>().get_drop_active())
            .and_then(|s| s.hit(&w, point));
        w.global::<Ui>()
            .set_drop_hover(selected.map(|i| i as i32).unwrap_or(-1));
        #[cfg(any(target_os = "linux", windows))]
        if selected.is_none() && w.global::<Ui>().get_drop_active() {
            return outside_drop_list(&w, point);
        }
        selected.is_some()
    });
    let pending = session.clone();
    let weak = window.as_weak();
    let frame = view.clone();
    let output = commands.clone();
    let drop = std::rc::Rc::new(move |point, files: Vec<PathBuf>| {
        let Some(w) = weak.upgrade() else {
            return false;
        };
        let Some(session) = pending.borrow_mut().take() else {
            return false;
        };
        if !w.global::<Ui>().get_drop_active()
            || files.is_empty()
            || files.len() > 128
            || *frame.lock().expect("drop frame") != session.origin
        {
            return false;
        }
        let index = session.hit(&w, point);
        #[cfg(any(target_os = "linux", windows))]
        if index.is_none() && outside_drop_list(&w, point) {
            return output
                .send(Action::DropFiles {
                    files,
                    targets: Vec::new(),
                    origin: session.origin,
                    targeted: false,
                })
                .is_ok();
        }
        let Some(index) = index else {
            return false;
        };
        let Some(targets) = session.targets.get(index).filter(|t| !t.is_empty()) else {
            return false;
        };
        let peers = w.global::<Ui>().get_devices();
        let targets = targets
            .iter()
            .filter(|target| {
                peers
                    .iter()
                    .any(|peer| peer.id.as_str() == target.device.as_str() && peer.connected)
            })
            .cloned()
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return false;
        }
        output
            .send(Action::DropFiles {
                files,
                targets,
                origin: session.origin,
                targeted: index != 0,
            })
            .is_ok()
    });
    let weak = window.as_weak();
    let cancel = std::rc::Rc::new(move || {
        session.borrow_mut().take();
        if let Some(w) = weak.upgrade() {
            let ui = w.global::<Ui>();
            ui.set_drop_active(false);
            ui.set_drop_hover(-1);
        }
    });
    let callbacks = native_drop::Callbacks {
        begin,
        hover,
        drop,
        cancel,
    };
    #[cfg(target_os = "macos")]
    let weak = window.as_weak();
    #[cfg(target_os = "macos")]
    window.window().on_winit_window_event(move |_, _| {
        if let Some(w) = weak.upgrade() {
            native_drop::install(&w, callbacks.clone());
        }
        EventResult::Propagate
    });
    #[cfg(all(target_os = "linux", pneu_qt_drag))]
    window
        .global::<Ui>()
        .set_drop_supported(native_drop::install(window, callbacks));
    // Share the existing event filter: Slint can create its HWND after show()
    // and before the first native event, later than a zero-duration timer.
    #[cfg(windows)]
    super::windows_window::install(window, move |w| {
        native_drop::install(w, callbacks.clone());
    });
}

#[cfg(any(all(target_os = "linux", pneu_qt_drag), windows))]
fn outside_drop_list(window: &PneuWindow, point: (f64, f64)) -> bool {
    let g = window.get_drop_geometry();
    point.0.is_finite()
        && point.1.is_finite()
        && !(point.0 >= g.x as f64
            && point.1 >= g.y as f64
            && point.0 < (g.x + g.width) as f64
            && point.1 < (g.y + g.height) as f64)
}

#[cfg(all(test, target_os = "linux", pneu_qt_drag))]
#[path = "linux_drag_tests.rs"]
mod linux_drag_tests;

#[derive(Clone)]
struct UiSender {
    commands: tokio::sync::mpsc::UnboundedSender<Action>,
    view: Arc<Mutex<View>>,
}
impl UiSender {
    fn send(
        &self,
        action: Action,
    ) -> std::result::Result<(), Box<tokio::sync::mpsc::error::SendError<Action>>> {
        if matches!(action, Action::Close) {
            return self.commands.send(action).map_err(Box::new);
        }
        let view = self.view.lock().expect("window view").clone();
        self.commands
            .send(Action::Scoped(view, Box::new(action)))
            .map_err(Box::new)
    }
}

// Slint 1.15 elides horizontal overflow but silently drops extra wrapped lines.
// Use its renderer for the exact window font, fallback fonts and scale. These
// internals are available through Slint's re-exports; Slint is pinned to 1.15.1.
fn update_history_order(
    order: &mut Vec<String>,
    old: &[crate::store::history::Item],
    items: &[crate::store::history::Item],
    open: bool,
) {
    if !open {
        *order = items.iter().map(|i| i.id.clone()).collect();
        return;
    }
    order.retain(|id| items.iter().any(|i| &i.id == id));
    // Only the unpinned row moves. Other Recent rows keep their displayed order,
    // even if selecting one updated its copy time while this page was open.
    for (rank, item) in items.iter().enumerate() {
        if item.pinned.is_none() && old.iter().any(|i| i.id == item.id && i.pinned.is_some()) {
            order.retain(|id| id != &item.id);
            let position = order
                .iter()
                .position(|id| {
                    items
                        .iter()
                        .position(|i| &i.id == id && i.pinned.is_none())
                        .is_some_and(|r| r > rank)
                })
                .unwrap_or(order.len());
            order.insert(position, item.id.clone());
        }
    }
    let mut new: Vec<_> = items
        .iter()
        .filter(|i| !order.contains(&i.id))
        .map(|i| i.id.clone())
        .collect();
    new.append(order);
    *order = new;
}

fn history_text_size(window: &PneuWindow, text: &str, width: f32) -> (f32, f32) {
    use slint::private_unstable_api::re_exports::{
        ComplexText, ItemRc, LogicalLength, TextWrap, WindowInner,
    };
    let inner = WindowInner::from_pub(window.window());
    let adapter = inner.window_adapter();
    let item = ItemRc::new(inner.component(), 0);
    let probe = std::pin::pin!(ComplexText::default());
    probe.text.set(text.into());
    let size = adapter.renderer().text_size(
        probe.as_ref(),
        &item,
        Some(LogicalLength::new(width)),
        TextWrap::WordWrap,
    );
    (size.width, size.height)
}

fn history_title(window: &PneuWindow, text: &str, width: f32, height: f32) -> SharedString {
    if width <= 0.0 || height <= 0.0 {
        return SharedString::default();
    }
    fit_history_title(text, |candidate| {
        let (w, h) = history_text_size(window, candidate, width.floor());
        w <= width && h.ceil() <= height
    })
    .into()
}

fn fit_history_title(text: &str, mut fits: impl FnMut(&str) -> bool) -> String {
    use slint::private_unstable_api::re_exports::UnicodeSegmentation;
    if fits(text) {
        return text.into();
    }
    if !fits("…") {
        return String::new();
    }
    // Search complete graphemes, including the ellipsis in every measurement.
    let ends: Vec<_> = text.grapheme_indices(true).map(|(i, _)| i).collect();
    let mut low = 0;
    let mut high = ends.len();
    let mut result = String::from("…");
    while low < high {
        let mid = low + (high - low) / 2;
        let candidate = format!("{}…", text[..ends[mid]].trim_end());
        if fits(&candidate) {
            result = candidate;
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    result
}

fn connect(
    window: &PneuWindow,
    commands: &tokio::sync::mpsc::UnboundedSender<Action>,
    view: &Arc<Mutex<View>>,
) {
    let commands = UiSender {
        commands: commands.clone(),
        view: view.clone(),
    };
    let ui = window.global::<Ui>();
    ui.set_devices(ModelRc::new(VecModel::<DeviceItem>::default()));
    ui.set_recent(ModelRc::new(VecModel::<RecentItem>::default()));
    ui.set_history(ModelRc::new(VecModel::<HistoryItem>::default()));
    let weak = window.as_weak();
    ui.on_elide_history_title(move |text, width, height| {
        weak.upgrade()
            .map(|window| history_title(&window, &text, width, height))
            .unwrap_or_default()
    });
    ui.set_recipients(ModelRc::new(VecModel::<RecipientItem>::default()));
    ui.set_device_settings(ModelRc::new(VecModel::<SettingItem>::default()));
    // High error correction keeps the code readable under Pneu's icon.
    match qr_image(DOWNLOAD_PAGE, qrcode::EcLevel::H, 1) {
        Ok(pixels) => ui.set_download_qr(slint::Image::from_rgba8(pixels)),
        Err(error) => eprintln!("pneud window: download code: {error:#}"),
    }
    let send = |action: fn() -> Action| {
        let c = commands.clone();
        move || {
            let _ = c.send(action());
        }
    };
    let with = |action: fn(String) -> Action| {
        let c = commands.clone();
        move |value: SharedString| {
            let _ = c.send(action(value.into()));
        }
    };
    let close = send(|| Action::Close);
    ui.on_close(move || {
        #[cfg(target_os = "macos")]
        super::macos::camera::close();
        close();
    });
    #[cfg(target_os = "macos")]
    {
        let weak = window.as_weak();
        ui.on_scan_qr(move || {
            if let Some(window) = weak.upgrade() {
                super::macos::camera::open(&window);
            }
        });
    }
    ui.on_choose_folder(send(|| Action::Folder));
    ui.on_open_folder(send(|| Action::OpenFolder));
    ui.on_send_clipboard(send(|| Action::SendClipboard));
    ui.on_history_searching(|query| !crate::store::history::search_key(&query).is_empty());
    ui.on_filter_history(|items, query, pinned| {
        let query = crate::store::history::search_key(&query);
        ModelRc::new(slint::FilterModel::new(items, move |i| {
            i.pinned == pinned && (query.is_empty() || (!i.picture && i.search.contains(&query)))
        }))
    });
    let pin_tx = commands.clone();
    ui.on_pin_history(move |id, pinned| {
        let _ = pin_tx.send(Action::PinHistory(id.to_string(), pinned));
    });
    ui.on_copy_history(with(Action::CopyHistory));
    ui.on_disable_empty_history(send(|| Action::DisableEmptyHistory));
    ui.on_delete_history(with(Action::DeleteHistory));
    ui.on_clear_history(send(|| Action::ClearHistory));
    ui.on_show_code(send(|| Action::ShowCode));
    ui.on_new_code(send(|| Action::NewCode));
    let c = commands.clone();
    ui.on_offer_pair(move |link| {
        let _ = c.send(Action::OfferPair(link));
    });
    let c = commands.clone();
    ui.on_confirm_join(move |link| {
        let _ = c.send(Action::ConfirmJoin(link));
    });
    let c = commands.clone();
    ui.on_top_action(move |yes| {
        let _ = c.send(Action::TopCard(yes));
    });
    ui.on_top_close(send(|| Action::TopCardClose));
    ui.on_link_device(with(Action::LinkDevice));
    let c = commands.clone();
    ui.on_answer(move |device, allow| {
        let _ = c.send(Action::Answer(device.into(), allow));
    });
    ui.on_removing(with(Action::Removing));
    ui.on_remove(with(Action::Remove));
    ui.on_offer_dismiss(send(|| Action::OfferDismiss));
    ui.on_picker_cancel(send(|| Action::PickerCancel));
    ui.on_get_tailscale(send(|| Action::GetTailscale));
    ui.on_unpair(with(Action::Unpair));
    ui.on_send_files(with(Action::SendFiles));
    ui.on_cancel_item(with(Action::Cancel));
    ui.on_retry_item(with(Action::Retry));
    ui.on_dismiss_item(with(Action::Dismiss));
    ui.on_details_item(with(Action::Details));
    let c = commands.clone();
    ui.on_recipient_action(move |item, device, retry| {
        let _ = c.send(Action::Recipient(item.into(), device.into(), retry));
    });
    ui.on_valid_name(|name| !name.trim().is_empty());
    ui.on_open_item(with(Action::Open));
    ui.on_rename(with(Action::Rename));
    ui.on_system_settings(with(Action::SystemSettings));
    let c = commands.clone();
    ui.on_login(move |on| {
        let _ = c.send(Action::Login(on));
    });
    ui.on_paste_link(with(Action::PasteLink));
    ui.on_scanned_link(with(Action::ScannedLink));
    ui.on_probed(with(Action::Probed));
    let c = commands.clone();
    ui.on_set_peer(move |key, on| {
        let _ = c.send(Action::SetPeer(key.into(), on));
    });
    let c = commands.clone();
    ui.on_set_flag(move |key, on| {
        let _ = c.send(Action::SetFlag(key.into(), on));
    });
    let c = commands.clone();
    ui.on_set_mode(move |mode| {
        let _ = c.send(Action::SetMode(mode));
    });
    let c = commands.clone();
    ui.on_message_action(move |action, primary| {
        let _ = c.send(Action::Message(action.into(), primary));
    });
    let c = commands.clone();
    let weak = window.as_weak();
    ui.on_picker_send(move || {
        let Some(window) = weak.upgrade() else { return };
        let picks = window.global::<Ui>().get_picks();
        let chosen = picks
            .iter()
            .filter(|p| p.checked && p.eligible)
            .map(|p| p.id.to_string())
            .collect();
        let _ = c.send(Action::PickerSend(chosen));
    });
    let c = commands.clone();
    let weak = window.as_weak();
    let view = view.clone();
    ui.on_navigated(move || {
        let Some(window) = weak.upgrade() else { return };
        let ui = window.global::<Ui>();
        ui.set_navigation_revision(ui.get_navigation_revision().wrapping_add(1));
        let mut current = view.lock().expect("window view");
        *current = View {
            revision: ui.get_navigation_revision(),
            page: ui.get_page(),
            device: ui.get_device_id().into(),
            add_tab: ui.get_add_tab(),
            targets: current.targets.clone(),
            setup: current.setup.clone(),
            top: current.top.clone(),
        };
        drop(current);
        #[cfg(target_os = "macos")]
        super::macos::camera::navigated(&window);
        let _ = c.send(Action::Navigated);
    });
}

/// The target ("" for every connected device) and the files picked, if any.
type Chosen = (
    String,
    Vec<contract::CapturedTarget>,
    View,
    Option<Vec<PathBuf>>,
);

/// Window choices about linking that outlive the window (window_setup.rs).
fn setup_state_path(paths: &Paths) -> PathBuf {
    paths.data.join("pneu-window-linking.json")
}

fn join_journal_path(paths: &Paths) -> PathBuf {
    #[cfg(windows)]
    {
        paths.data.join("pneu-window-join.json")
    }
    #[cfg(not(windows))]
    {
        paths.socket.with_file_name("pneu-window-join.json")
    }
}

#[cfg(all(test, windows))]
#[test]
fn windows_join_journal_is_a_private_file_in_the_overridden_data_namespace() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = Paths {
        data: dir.path().join("own-data"),
        cache: dir.path().join("cache"),
        config: dir.path().join("config/config.toml"),
        socket: PathBuf::from(r"\\.\pipe\pneu-test-runtime"),
    };
    paths.prepare()?;
    let path = join_journal_path(&paths);
    assert_eq!(path, paths.data.join("pneu-window-join.json"));
    crate::config::atomic_private(&path, b"journal")?;
    assert!(path.is_file());
    let folder = crate::storage::Folder::open(&paths.data, false)?;
    let file = crate::platform::fs::open_journal_at(&folder.file, path.file_name().unwrap())?;
    assert!(crate::platform::fs::is_owned_private(&file)?);
    assert!(crate::platform::fs::is_private_directory(&folder.file)?);
    assert_eq!(std::fs::read(path)?, b"journal");
    Ok(())
}

struct Worker {
    paths: Paths,
    weak: slint::Weak<PneuWindow>,
    view: Arc<Mutex<View>>,
    request: u64,
    invitation_revision: u64,
    folder_job: Option<futures_util::future::BoxFuture<'static, Result<()>>>,
    folder_origin: Option<View>,
    open_folder_job: Option<futures_util::future::BoxFuture<'static, Result<()>>>,
    open_folder_origin: Option<(View, PathBuf)>,
    open_job: Option<futures_util::future::BoxFuture<'static, Result<()>>>,
    open_origin: Option<View>,
    chooser_origin: Option<View>,
    chooser: Option<futures_util::future::BoxFuture<'static, Result<Chosen>>>,
    picker_files: Vec<PathBuf>,
    picker_targets: Vec<contract::CapturedTarget>,
    picker_origin: View,
    picker_active: bool,
    picker_clipboard: bool,
    send_request_id: Option<String>,
    handled_send: Option<String>,
    details: Option<String>,
    origin: View,
    restored_join: bool,
    offer_navigation: Option<View>,
    action_error: String,
    status_error: String,
    status: Value,
    snapshot: super::Snapshot,
    history_order: Vec<String>,
    history_search: HashMap<String, String>,
    transfers: Vec<Transfer>,
    pushed: Option<Frame>,
    phone_token: Option<pneu_proto::Hash>,
    phone_failed: bool,
    last_pair_attempt: u64,
    code: Code,
    join: Option<Join>,
    dismissed_offer: Option<String>,
    live_pairing_link: Option<String>,
    live_pairing_revision: u64,
    probed: bool,
    seen_pairing_token: Option<pneu_proto::Hash>,
    paired: Option<Paired>,
    peer_settings: Option<(String, Value, Instant)>,
    last_view: View,
    requested_page: Option<Page>,
    tailscale: Option<(bool, Instant)>,
    setup: Setup,
    remembered: Remembered,
    // Remove from all devices, as the confirmation showed it.
    removal: Option<Removal>,
    // Your devices when a pairing page opened, for "Also linked".
    known_roster: HashSet<String>,
    removed_shown: Option<u64>,
    // A notification's Remove… asked for this device's confirmation.
    remove_request: Option<String>,
}

impl Worker {
    fn new(
        paths: Paths,
        weak: slint::Weak<PneuWindow>,
        view: Arc<Mutex<View>>,
        request: u64,
    ) -> Self {
        let remembered = Remembered::load(&setup_state_path(&paths));
        Self {
            paths,
            weak,
            view,
            request,
            invitation_revision: 0,
            folder_job: None,
            folder_origin: None,
            open_folder_job: None,
            open_folder_origin: None,
            open_job: None,
            open_origin: None,
            chooser_origin: None,
            chooser: None,
            picker_files: Vec::new(),
            picker_targets: Vec::new(),
            picker_origin: View::default(),
            picker_active: false,
            picker_clipboard: false,
            send_request_id: None,
            handled_send: None,
            details: None,
            origin: View::default(),
            restored_join: false,
            offer_navigation: None,
            action_error: String::new(),
            status_error: String::new(),
            status: Value::Null,
            snapshot: Default::default(),
            history_order: Vec::new(),
            history_search: HashMap::new(),
            transfers: Vec::new(),
            pushed: None,
            phone_token: None,
            phone_failed: false,
            last_pair_attempt: 0,
            code: Code::default(),
            join: None,
            dismissed_offer: None,
            live_pairing_link: None,
            live_pairing_revision: 0,
            probed: false,
            seen_pairing_token: None,
            paired: None,
            peer_settings: None,
            requested_page: None,
            tailscale: None,
            setup: Setup::default(),
            remembered,
            removal: None,
            known_roster: HashSet::new(),
            removed_shown: None,
            remove_request: None,
            last_view: View {
                page: Page::Main,
                ..Default::default()
            },
        }
    }
    fn view(&self) -> View {
        self.view.lock().expect("window view").clone()
    }
    fn current(&self) -> bool {
        self.view() == self.origin
    }
    fn ui(&self, f: impl FnOnce(&PneuWindow) + Send + 'static) {
        let origin = self.origin.clone();
        let view = self.view.clone();
        let _ = self.weak.upgrade_in_event_loop(move |w| {
            if *view.lock().expect("window view") == origin {
                f(&w);
            }
        });
    }
    fn captured_targets(&self, device: &str) -> Vec<contract::CapturedTarget> {
        let flag = |key| {
            contract::setting(&self.status, key)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        use crate::file_recipients::Selection;
        let targets = self
            .origin
            .targets
            .clone()
            .unwrap_or_else(|| contract::list::<SendTarget>(&self.status, "send_targets"));
        let ids = if device.is_empty() {
            Vec::new()
        } else {
            match pneu_proto::Id::new(device) {
                Ok(id) => vec![id],
                Err(_) => return Vec::new(),
            }
        };
        let mode = if ids.is_empty() {
            Selection::AllEligible {
                queue_offline: flag("choose_devices_each_time") || flag("queue_offline"),
            }
        } else {
            Selection::Explicit(&ids)
        };
        contract::resolve_files(&targets, mode).unwrap_or_default()
    }
    fn status_failed(&mut self, error: anyhow::Error) {
        if self.status_error.is_empty() {
            eprintln!("pneud window: couldn't update status: {error:#}");
        }
        self.status_error = "Couldn't update Pneu. Retrying…".into();
        let message = self.status_error.clone();
        self.ui(move |w| w.global::<Ui>().set_error(message.into()));
        self.pushed = None;
    }
    fn fail(&mut self, message: &str, error: anyhow::Error) {
        eprintln!("pneud window: {message}: {error:#}");
        if self.current() {
            self.action_error = format!("{message}.");
        }
    }
    fn start_open_folder(&mut self) {
        if self.open_folder_job.is_some() {
            return;
        }
        let folder = PathBuf::from(
            self.status["config"]["save_path"]
                .as_str()
                .unwrap_or_default(),
        );
        self.open_folder_origin = Some((self.view(), folder.clone()));
        let socket = self.socket();
        self.open_folder_job = Some(Box::pin(async move { open_folder(&socket, &folder).await }));
    }
    fn opened_folder(&mut self, result: Result<()>) {
        self.open_folder_job = None;
        if let Some((origin, folder)) = self.open_folder_origin.take() {
            self.origin = origin;
            if let Err(error) = result {
                eprintln!(
                    "pneud window: couldn't open {}: {error:#}",
                    folder.display()
                );
                if self.current() {
                    self.action_error = format!("Couldn't open {}", super::display_path(&folder));
                }
            }
        }
    }
    fn socket(&self) -> PathBuf {
        self.paths.socket.clone()
    }
    async fn command(&self, command: ipc::Command) -> Result<Value> {
        ipc::request(&self.paths.socket, command).await
    }
    async fn transfer_command(
        &self,
        item: &str,
        device: Option<&str>,
        retry: bool,
    ) -> Result<Value> {
        let row = self
            .transfers
            .iter()
            .find(|t| artifact_key(t) == item)
            .context("Recent item is no longer available")?;
        anyhow::ensure!(
            device.is_none() || artifact_id(row).is_some(),
            "Recipient action requires an artifact"
        );
        let command = serde_json::from_value(json!({
            "command": if retry { "retry_transfer" } else { "cancel_transfer" },
            "operation": row.operation,
            "artifact": artifact_id(row),
            "device": device,
        }))?;
        self.command(command).await
    }
    fn message(
        &self,
        title: String,
        text: String,
        tile: i32,
        secondary: &str,
        primary: &str,
        action: &str,
    ) {
        let message = Message {
            title: title.into(),
            text: text.into(),
            tile,
            primary: primary.into(),
            secondary: secondary.into(),
            action: action.into(),
        };
        self.ui(move |w| {
            let ui = w.global::<Ui>();
            ui.set_message(message);
            ui.set_dialog(Dialog::Message);
        });
    }
    fn show_details(&self) {
        let Some(key) = &self.details else {
            return;
        };
        let recipients = recipient_details(
            &peers(&self.snapshot, &self.transfers),
            &self.transfers,
            key,
        );
        let title = self
            .transfers
            .iter()
            .find(|t| artifact_key(t) == *key)
            .map(|t| t.file_name.clone())
            .unwrap_or_default();
        self.ui(move |w| {
            let ui = w.global::<Ui>();
            // A dismissed sheet must stay dismissed during subsequent polls.
            if ui.get_dialog() == Dialog::None || ui.get_dialog() == Dialog::Details {
                sync(ui.get_recipients(), &recipients);
                ui.set_details_title(title.into());
            }
        });
    }
    async fn act(&mut self, action: Action) {
        let action = match action {
            Action::Scoped(view, action) => {
                // Global writes retain the user's click across navigation. Their
                // eventual UI feedback is still gated by the originating view.
                let global_write = matches!(
                    *action,
                    Action::SetMode(_)
                        | Action::SetFlag(_, _)
                        | Action::Rename(_)
                        | Action::DisableEmptyHistory
                );
                if self.view() != view && !global_write {
                    return;
                }
                self.origin = view;
                *action
            }
            action => {
                self.origin = self.view();
                action
            }
        };
        if !matches!(action, Action::Navigated | Action::Probed(_)) {
            self.action_error.clear();
        }
        match action {
            Action::Scoped(_, _) => unreachable!("nested UI action"),
            Action::Close => {}
            Action::Navigated => self.navigated(true).await,
            Action::Focused => {
                if self.view().page == Page::Add && self.join.is_none() {
                    self.ui(|w| w.invoke_probe_clipboard());
                }
            }
            Action::Folder => {
                if self.folder_job.is_none() {
                    self.folder_origin = Some(self.view());
                    let socket = self.socket();
                    self.folder_job = Some(Box::pin(async move { change_folder(&socket).await }));
                }
            }
            Action::OpenFolder => self.start_open_folder(),
            Action::Unpair(device) => match pneu_proto::Id::new(device.as_str()) {
                Ok(device) => {
                    if let Err(error) = self.command(ipc::Command::Unpair { device }).await {
                        self.fail("Couldn't unpair", error);
                    }
                    self.phone_token = None;
                }
                Err(error) => self.fail("Couldn't unpair", error.into()),
            },
            Action::SendFiles(target) => {
                if self.chooser.is_none() {
                    self.chooser_origin = Some(self.view());
                    let bindings = self.captured_targets(&target);
                    let origin = self.view();
                    self.chooser = Some(Box::pin(async move {
                        Ok((target, bindings, origin, choose_files("Send Files").await?))
                    }));
                }
            }
            Action::PickerSend(devices) => {
                self.picker_active = false;
                let files = std::mem::take(&mut self.picker_files);
                if !devices.is_empty() {
                    if self.view() != self.picker_origin {
                        return;
                    }
                    let targets: Vec<_> = self
                        .picker_targets
                        .iter()
                        .filter(|t| devices.contains(&t.device))
                        .cloned()
                        .collect();
                    if !targets.is_empty() {
                        if self.picker_clipboard {
                            self.send_clipboard(&targets).await;
                        } else {
                            self.send(files, &targets).await;
                        }
                    }
                }
            }
            Action::PickerCancel => {
                self.picker_active = false;
                self.send_request_id = None;
                self.picker_files.clear();
                self.picker_targets.clear();
            }
            #[cfg(any(target_os = "macos", all(target_os = "linux", pneu_qt_drag), windows))]
            Action::DropFiles {
                files,
                targets,
                origin,
                targeted,
            } => {
                if self.view() == origin {
                    let result = async {
                        let targets: Vec<crate::outbox::Authority> =
                            serde_json::from_value(serde_json::to_value(targets)?)?;
                        #[cfg(any(target_os = "linux", windows))]
                        if targets.is_empty() {
                            self.command(ipc::Command::PresentFiles {
                                paths: files,
                                targets: crate::outbox::Targets::All("all".into()),
                                request_id: None,
                            })
                            .await?;
                            return Ok::<_, anyhow::Error>(());
                        }
                        self.command(ipc::Command::PresentPicker {
                            paths: Some(files),
                            targets,
                            clipboard: false,
                            targeted,
                        })
                        .await?;
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        self.fail("Couldn't send files", error);
                    }
                }
            }
            #[cfg(target_os = "macos")]
            Action::Native(action) => {
                use super::macos::native::MenuAction;
                let result = match action {
                    MenuAction::Command(value) => match serde_json::from_value(value) {
                        Ok(command) => self.command(command).await.map(|_| ()),
                        Err(error) => Err(error.into()),
                    },
                    MenuAction::Send {
                        recipients,
                        targeted,
                        clipboard,
                    } => self
                        .command(ipc::Command::PresentPicker {
                            paths: None,
                            targets: recipients,
                            clipboard,
                            targeted,
                        })
                        .await
                        .map(|_| ()),
                    MenuAction::Folder => {
                        self.start_open_folder();
                        Ok(())
                    }
                    MenuAction::Settings => {
                        let result = self
                            .command(ipc::Command::ShowWindow {
                                choose_folder: false,
                            })
                            .await
                            .map(|_| ());
                        if result.is_ok() {
                            self.requested_page = Some(Page::Settings);
                        }
                        result
                    }
                };
                if let Err(error) = result {
                    self.fail("Couldn't complete the menu action", error);
                }
            }
            Action::DisableEmptyHistory => {
                match self
                    .command(ipc::Command::DisableEmptyClipboardHistory)
                    .await
                {
                    Ok(count) => {
                        let Some(count) = count.as_u64() else {
                            self.fail(
                                "Couldn't save setting",
                                anyhow::anyhow!("invalid history count"),
                            );
                            return;
                        };
                        if count > 0 {
                            self.ui(move |w| {
                                let ui = w.global::<Ui>();
                                ui.set_history_off_count(count.min(i32::MAX as u64) as i32);
                                ui.set_dialog(Dialog::HistoryOff);
                            });
                        }
                    }
                    Err(error) => self.fail("Couldn't save setting", error),
                }
            }
            Action::CopyHistory(id) => {
                match self
                    .command(ipc::Command::CopyClipboardHistory { id: id.clone() })
                    .await
                {
                    Ok(_) => self.ui(move |w| w.invoke_history_copied(id.into())),
                    Err(error) => self.fail("Couldn't copy", error),
                }
            }
            Action::PinHistory(id, pinned) => {
                match self
                    .command(ipc::Command::PinClipboardHistory { id, pinned })
                    .await
                {
                    Ok(_) => (),
                    Err(error)
                        if error
                            .to_string()
                            .contains(crate::store::history::PIN_LIMIT_ERROR) =>
                    {
                        if self.current() {
                            self.action_error = crate::store::history::PIN_LIMIT_ERROR.into();
                        }
                    }
                    Err(error) => self.fail("Couldn't change pin", error),
                }
            }
            Action::DeleteHistory(id) => {
                if let Err(error) = self
                    .command(ipc::Command::DeleteClipboardHistory { id })
                    .await
                {
                    self.fail("Couldn't delete", error);
                }
            }
            Action::ClearHistory => {
                if let Err(error) = self.command(ipc::Command::ClearClipboardHistory).await {
                    self.fail("Couldn't clear history", error);
                }
            }
            Action::SendClipboard => {
                let bindings = self
                    .origin
                    .targets
                    .clone()
                    .unwrap_or_else(|| contract::list::<SendTarget>(&self.status, "send_targets"))
                    .into_iter()
                    .filter_map(|t| {
                        Some(contract::CapturedTarget {
                            device: t.device,
                            generation: t.generation,
                            fingerprint: t.fingerprint?,
                        })
                    })
                    .collect();
                self.select_send(Vec::new(), bindings, self.view(), false, true)
                    .await;
            }
            Action::Cancel(item) => {
                if let Err(error) = self.transfer_command(&item, None, false).await {
                    self.fail("Couldn't cancel", error);
                }
            }
            Action::Retry(item) => {
                let failed: HashSet<_> = self
                    .transfers
                    .iter()
                    .filter(|t| {
                        artifact_key(t) == item && t.state == "failed" && artifact_id(t).is_some()
                    })
                    .map(|t| t.device.clone())
                    .collect();
                for device in failed {
                    if let Err(error) = self.transfer_command(&item, Some(&device), true).await {
                        self.fail("Couldn't retry", error);
                    }
                }
            }
            Action::Dismiss(item) => {
                if let Some(row) = self.transfers.iter().find(|t| artifact_key(t) == item) {
                    let command = serde_json::from_value(json!({
                        "command":"dismiss_transfer", "operation":row.operation, "artifact":artifact_id(row)
                    }));
                    match command {
                        Ok(command) => {
                            if let Err(error) = self.command(command).await {
                                self.fail("Couldn't remove item", error);
                            }
                        }
                        Err(error) => self.fail("Couldn't remove item", error.into()),
                    }
                }
            }
            Action::Details(item) => {
                self.details = Some(item);
                self.show_details();
                self.ui(|w| w.global::<Ui>().set_dialog(Dialog::Details));
            }
            Action::Recipient(item, device, retry) => {
                let result = self.transfer_command(&item, Some(&device), retry).await;
                if let Err(error) = result {
                    self.fail(
                        if retry {
                            "Couldn't retry"
                        } else {
                            "Couldn't stop sending"
                        },
                        error,
                    );
                }
            }
            Action::Open(item) => {
                if self.open_job.is_none()
                    && let Some(row) = self
                        .transfers
                        .iter()
                        .find(|t| artifact_key(t) == item)
                        .filter(|t| matches!(t.state.as_str(), "sent" | "received"))
                        .filter(|t| t.path.is_some())
                        .cloned()
                {
                    self.open_origin = Some(self.view());
                    self.open_job = Some(Box::pin(async move {
                        let path = row.path.as_ref().context("file path")?;
                        let file = recent_file(path)?;
                        #[cfg(target_os = "linux")]
                        {
                            open_file_on(file, zbus::Connection::session().await?).await
                        }
                        #[cfg(target_os = "macos")]
                        {
                            open_recent(file).await
                        }
                        #[cfg(windows)]
                        {
                            use crate::actions::PcActions;
                            crate::actions::DesktopActions::new(PathBuf::new())
                                .open_received(&file)
                                .await
                        }
                    }));
                }
            }
            Action::Login(enabled) => {
                #[cfg(target_os = "macos")]
                {
                    let (send, receive) = tokio::sync::oneshot::channel();
                    let result = slint::invoke_from_event_loop(move || {
                        let _ = send.send(super::macos::native::set_login(enabled));
                    });
                    let result = match result {
                        Ok(()) => receive.await.map_err(anyhow::Error::from).and_then(|r| r),
                        Err(error) => Err(error.into()),
                    };
                    if let Err(error) = result {
                        self.fail("Couldn't change Open at Login", error);
                    }
                }
                #[cfg(any(target_os = "linux", windows))]
                let _ = enabled;
            }
            Action::SystemSettings(pane) => {
                #[cfg(target_os = "macos")]
                {
                    let _ = slint::invoke_from_event_loop(move || {
                        let _ = super::macos::native::system_settings(&pane);
                    });
                }
                #[cfg(any(target_os = "linux", windows))]
                let _ = pane;
            }
            Action::SetPeer(key, enabled) => {
                let device = self.view().device;
                match pneu_proto::Id::new(device.as_str()) {
                    Ok(id) => match self
                        .command(ipc::Command::SetPeerSetting {
                            device: id,
                            key,
                            enabled,
                        })
                        .await
                    {
                        Ok(reply) if self.current() => {
                            self.peer_settings = Some((device, reply, Instant::now()))
                        }
                        Ok(_) => {}
                        Err(error) => {
                            self.peer_settings = None;
                            self.fail("Couldn't save setting", error);
                        }
                    },
                    Err(error) => self.fail("Couldn't save setting", error.into()),
                }
            }
            Action::SetMode(mode) => {
                let mode = [
                    ClipboardMode::Automatic,
                    ClipboardMode::Manual,
                    ClipboardMode::Off,
                ]
                .get(mode as usize)
                .copied()
                .unwrap_or_default();
                if let Err(error) = self
                    .command(ipc::Command::SetSetting {
                        key: "clipboard_mode".into(),
                        value: json!(mode.key()),
                    })
                    .await
                {
                    self.fail("Couldn't save setting", error);
                }
            }
            Action::SetFlag(key, on) => {
                if let Err(error) = self.set_flag(&key, on).await {
                    self.fail("Couldn't save setting", error);
                }
            }
            Action::Rename(name) => {
                let name = name.trim().to_owned();
                if !name.is_empty()
                    && let Err(error) = self.command(ipc::Command::Rename { name }).await
                {
                    self.fail(&format!("Couldn't rename {}", this_computer()), error);
                }
            }
            Action::ShowCode => self.show_code(false).await,
            Action::NewCode => self.show_code(true).await,
            Action::PasteLink(text) => self.paste(text, false).await,
            Action::ScannedLink(text) => self.scan(text).await,
            Action::Probed(text) => {
                if self.join.is_none() && self.dismissed_offer.as_deref() != Some(text.trim()) {
                    self.paste(text, true).await;
                }
            }
            Action::OfferPair(link) | Action::ConfirmJoin(link) => {
                if let Some(join) = &mut self.join {
                    join.link = link;
                }
                self.confirm_join().await
            }
            Action::TopCard(yes) => self.top_action(yes).await,
            Action::TopCardClose => self.top_close(),
            Action::LinkDevice(device) => {
                self.link(&device).await;
            }
            Action::Answer(device, allow) => self.answer(&[device], allow).await,
            Action::Removing(device) => self.removal = self.origin.setup.removal(&device),
            Action::Remove(device) => self.remove(&device).await,
            Action::OfferDismiss => {
                if let Some(join) = self.join.take() {
                    self.dismissed_offer = Some(join.uri.clone());
                    self.cancel_join(&join).await;
                }
            }
            Action::GetTailscale => {
                if self.open_job.is_none() {
                    self.open_origin = Some(self.view());
                    self.open_job = Some(Box::pin(open_web(TAILSCALE_DOWNLOAD)));
                }
            }
            Action::Message(action, primary) => match action.as_str() {
                "join-confirm" if primary => self.confirm_join().await,
                "join-retry" if primary => {
                    self.confirm_join().await;
                }
                "join-confirm" | "join-retry" | "join-close" => {
                    if let Some(join) = self.join.take() {
                        self.cancel_join(&join).await;
                    }
                }
                "removed-ok" => {
                    let generation = self.setup.generation;
                    self.remember(|r| r.removed = Some(generation));
                }
                _ => {}
            },
        }
    }

    async fn navigated(&mut self, probe_clipboard: bool) {
        if self.view().page == Page::History {
            self.history_order = self
                .snapshot
                .clipboard_history
                .iter()
                .map(|i| i.id.clone())
                .collect();
        }

        let view = self.view();
        self.pushed = None;
        self.action_error.clear();
        self.details = None;
        self.picker_active = false;
        self.send_request_id = None;
        self.picker_files.clear();
        self.picker_targets.clear();
        if self.paired.as_ref().is_some_and(|p| p.origin != view) {
            self.paired = None;
        }
        let previous = std::mem::replace(&mut self.last_view, view.clone());
        if is_pairing_page(view.page) && !is_pairing_page(previous.page) {
            self.known_roster = self.setup.roster_ids();
            self.paired = None;
            self.probed = false;
        }
        if probe_clipboard && view.page == Page::Add && !self.probed {
            // A usable link from another computer on the clipboard is offered at once.
            self.probed = true;
            self.ui(|w| w.invoke_probe_clipboard());
        }
        if let Some(origin) = self.offer_navigation.take()
            && view.page == Page::Add
            && view.add_tab == 1
            && view.revision == origin.revision.wrapping_add(1)
            && let Some(join) = &mut self.join
            && join.offer
            && join.origin == origin
        {
            join.origin = view.clone();
        }
        if !self.restored_join
            && self.join.as_ref().is_some_and(|j| j.origin != view)
            && let Some(join) = self.join.take()
        {
            // Leaving Add Device stops an offer or attempt that hasn't paired.
            self.cancel_join(&join).await;
        }
        if view.page == Page::Device {
            self.peer_settings = None;
        }
        if self
            .remove_request
            .as_ref()
            .is_some_and(|d| view.page != Page::Device || view.device != *d)
        {
            self.remove_request = None;
        }
    }
    async fn set_flag(&self, key: &str, on: bool) -> Result<()> {
        anyhow::ensure!(
            matches!(
                key,
                "choose_devices_each_time"
                    | "queue_offline"
                    | "save_to_folder"
                    | "notification_preview"
                    | "copy_image_to_clipboard"
                    | "open_in_viewer"
                    | "clipboard_history"
                    | "clipboard_include_images"
            ),
            "unknown setting {key}"
        );
        self.command(ipc::Command::SetSetting {
            key: key.into(),
            value: json!(on),
        })
        .await?;
        Ok(())
    }

    async fn chosen(&mut self, result: Result<Chosen>) {
        let (target, bindings, origin, files) = match result {
            Ok((target, bindings, origin, Some(files))) if !files.is_empty() => {
                (target, bindings, origin, files)
            }
            Ok(_) => {
                self.send_request_id = None;
                return;
            }
            Err(error) => {
                self.send_request_id = None;
                return self.fail("Couldn't choose files", error);
            }
        };
        self.select_send(files, bindings, origin, !target.is_empty(), false)
            .await;
    }

    async fn select_send(
        &mut self,
        files: Vec<PathBuf>,
        bindings: Vec<contract::CapturedTarget>,
        origin: View,
        targeted: bool,
        clipboard: bool,
    ) {
        if self.view() != origin {
            return;
        }
        self.origin = origin.clone();
        let choose = contract::setting(&self.status, "choose_devices_each_time")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if targeted || !choose {
            if clipboard {
                self.send_clipboard(&bindings).await;
            } else {
                self.send(files, &bindings).await;
            }
            return;
        }
        let targets: Vec<SendTarget> = contract::list(&self.status, "send_targets");
        let eligible_files = if clipboard {
            Vec::new()
        } else {
            let captured = serde_json::from_value::<Vec<crate::outbox::Authority>>(
                serde_json::to_value(&bindings).unwrap_or_default(),
            )
            .unwrap_or_default();
            contract::resolve_files(
                &targets,
                crate::file_recipients::Selection::Captured(&captured),
            )
            .unwrap_or_default()
        };
        let peers = peers(&self.snapshot, &self.transfers);
        let picks: Vec<PickItem> = peers
            .iter()
            .map(|p| {
                let target = targets.iter().find(|t| t.device == p.id);
                let eligible = bindings.iter().any(|t| t.device == p.id)
                    && if clipboard {
                        p.connected()
                    } else {
                        eligible_files.iter().any(|t| t.device == p.id)
                    };
                let would_queue = target.is_some_and(|t| t.would_queue);
                let status = match target.and_then(|t| t.reason.clone()) {
                    Some(reason) if !eligible => reason,
                    _ if !eligible => p.connection(),
                    _ if would_queue => "Not connected".into(),
                    _ => p.connection(),
                };
                PickItem {
                    id: p.id.as_str().into(),
                    name: p.name.as_str().into(),
                    phone: p.phone,
                    tone: p.health.tone(),
                    connected: !would_queue,
                    eligible,
                    status: status.into(),
                    checked: eligible && !would_queue,
                }
            })
            .collect();
        let count = picks.iter().filter(|p| p.checked).count() as i32;
        let title = if clipboard {
            "Send clipboard to".into()
        } else {
            files_title(&files, "Send")
        };
        self.picker_active = true;
        self.picker_clipboard = clipboard;
        self.picker_files = files;
        self.picker_targets = bindings;
        self.picker_origin = origin;
        self.ui(move |w| {
            let ui = w.global::<Ui>();
            ui.set_picker_title(title.into());
            ui.set_picks(ModelRc::new(VecModel::from(picks)));
            ui.set_picker_count(count);
            ui.set_dialog(Dialog::Picker);
        });
    }

    async fn send_clipboard(&mut self, targets: &[contract::CapturedTarget]) {
        let notify_too_large = self.send_request_id.take().is_some();
        let result = async {
            let targets: crate::outbox::Targets =
                serde_json::from_value(serde_json::to_value(targets)?)?;
            self.command(ipc::Command::SendClipboard {
                targets,
                notify_too_large,
            })
            .await
        }
        .await;
        if let Err(error) = result
            && let Some(message) = clipboard_send_error(&error.to_string(), notify_too_large)
        {
            self.fail(message, error);
        }
    }

    async fn receive_send(&mut self, request: SendRequest) {
        self.send_request_id = Some(request.id);
        if let Some(error) = request.error {
            self.fail("Couldn't send files", anyhow::anyhow!(error));
            self.send_request_id = None;
            return;
        }
        let bindings = request
            .targets
            .into_iter()
            .map(|t| contract::CapturedTarget {
                device: t.device.as_str().into(),
                generation: t.generation,
                fingerprint: t.fingerprint.as_str().into(),
            })
            .collect();
        let origin = self.view();
        if request.clipboard {
            self.select_send(Vec::new(), bindings, origin, request.targeted, true)
                .await;
        } else if let Some(files) = request.paths {
            self.select_send(files, bindings, origin, request.targeted, false)
                .await;
        } else {
            self.chooser_origin = Some(origin.clone());
            let target = if request.targeted {
                "captured".into()
            } else {
                String::new()
            };
            self.chooser = Some(Box::pin(async move {
                Ok((target, bindings, origin, choose_files("Send Files").await?))
            }));
        }
    }

    async fn send(&mut self, files: Vec<PathBuf>, targets: &[contract::CapturedTarget]) {
        let request_id = self
            .send_request_id
            .take()
            .unwrap_or_else(|| crate::random_hex(16));
        let result = async {
            anyhow::ensure!(!targets.is_empty(), "No eligible devices");
            let command = || contract::send_command(files.clone(), targets, &request_id);
            match self.command(command()?).await {
                Ok(reply) => Ok(reply),
                Err(error) if daemon_rejection(&error) => Err(error),
                Err(_) => self.command(command()?).await,
            }
        }
        .await;
        if let Err(error) = result {
            self.fail("Couldn't send files", error);
        }
        // Admission now returns an operation immediately; staging and individual
        // rejections are rendered from subsequent snapshots, not guessed here.
    }

    async fn show_code(&mut self, new: bool) {
        self.code.last_attempt = crate::now_ms();
        let reply = if new {
            self.command(ipc::Command::Pair).await.and_then(|r| {
                Ok(pneu_proto::PairingUri::parse(
                    r["uri"].as_str().context("pairing reply")?,
                )?)
            })
        } else {
            self.command(ipc::Command::EnsurePair)
                .await
                .and_then(|r| Ok(serde_json::from_value::<pneu_proto::PairingUri>(r)?))
        };
        if !self.current() {
            return;
        }
        match reply {
            Ok(uri) => {
                self.code.error.clear();
                self.code.offline = false;
                self.status["invitation"] = serde_json::to_value(&uri).expect("pairing URI");
                self.adopt_code(&uri);
            }
            Err(error) => {
                eprintln!("pneud window: pairing code unavailable: {error:#}");
                let error_text = format!("{error:#}");
                self.code.offline = error_text.contains("no usable")
                    || error_text.contains("network")
                    || error_text.contains("no addresses")
                    || error_text.contains("no reachable interface");
                self.code.error = if self.code.offline {
                    "Connect this PC to a network to pair".into()
                } else {
                    "Couldn't create a pairing code".into()
                };
            }
        }
    }
    fn adopt_code(&mut self, uri: &pneu_proto::PairingUri) {
        if self.code.token.as_ref() == Some(&uri.token) {
            return;
        }
        let Ok(link) = uri.build() else { return };
        let Ok(pixels) = qr_pixels(&link) else { return };
        self.code.token = Some(uri.token.clone());
        self.code.shown.insert(uri.token.as_str().to_owned());
        self.code.uri = link;
        self.code.expires_at = uri.expires_at;
        self.ui(move |w| {
            w.global::<Ui>()
                .set_code_qr(slint::Image::from_rgba8(pixels))
        });
    }

    async fn scan(&mut self, text: String) {
        if self.origin.page != Page::Scan {
            return;
        }
        // Await the UI navigation inside this action: the worker must not
        // refresh Add Device and start a clipboard preview before this Join.
        let (send, receive) = tokio::sync::oneshot::channel();
        let view = self.view.clone();
        self.ui(move |w| {
            if send.is_closed() {
                return;
            }
            w.invoke_open_add(1);
            let _ = send.send(view.lock().expect("window view").clone());
        });
        let Ok(Ok(destination)) = tokio::time::timeout(Duration::from_secs(5), receive).await
        else {
            return;
        };
        if self.view() != destination {
            return;
        }
        self.origin = destination;
        // Scanning supersedes even a restored clipboard preview. JoinCancel
        // only abandons the local attempt; it never unpairs the other device.
        self.restored_join = false;
        self.navigated(false).await;
        self.probed = true;
        self.live_pairing_link = self.snapshot.pairing_clipboard.clone();
        self.live_pairing_revision = self.snapshot.pairing_clipboard_revision;
        if self.current() {
            self.paste(text, false).await;
        }
    }

    async fn paste(&mut self, text: String, offer: bool) {
        if !offer
            && self.join.as_ref().is_some_and(|j| j.offer && !j.confirmed)
            && let Some(join) = self.join.take()
        {
            self.cancel_join(&join).await;
            if !self.current() {
                return;
            }
        }
        let text = text.trim().to_owned();
        if !text.starts_with("pneu://pair") || text.len() > 8193 {
            if !offer {
                self.message(
                    "No pairing link on the clipboard".into(),
                    "Copy the link on the other computer first.".into(),
                    0,
                    "",
                    "OK",
                    "ok",
                );
            }
            return;
        }
        let reply = match self.command(ipc::Command::Join { uri: text.clone() }).await {
            Ok(reply) => reply,
            Err(error) => {
                if !offer {
                    self.fail("Couldn't pair", error);
                }
                return;
            }
        };
        if !self.current() {
            if let Some(attempt) = reply["attempt"].as_str() {
                let _ = self
                    .command(ipc::Command::JoinCancel {
                        attempt: attempt.into(),
                    })
                    .await;
            }
            return;
        }
        let name = display_peer_text(reply["name"].as_str().unwrap_or("the other computer"));
        let state = reply["state"].as_str().unwrap_or_default();
        if state == "confirmation" {
            self.join = Some(Join {
                attempt: reply["attempt"].as_str().unwrap_or_default().into(),
                uri: text,
                name: name.clone(),
                confirmed: false,
                cancel_requested: false,
                offer,
                link: true,
                origin: self.view(),
                reported: String::new(),
            });
            // Every confirmation starts with the switch on.
            self.ui(|w| w.global::<Ui>().set_link_on(true));
            if let Some(join) = &self.join
                && let Err(error) = self.save_join(join)
            {
                let join = self.join.take().expect("preview join");
                self.cancel_join(&join).await;
                self.fail("Couldn't retain the pairing attempt", error);
                return;
            }
            if offer {
                // Offers open Add Device on Computer.
                // Preserve the originating page/tab; a late clipboard result
                // must never navigate over a newer user choice.
                if self.view().page == Page::Add && self.view().add_tab == 0 {
                    self.offer_navigation = Some(self.view());
                    self.ui(|w| w.invoke_open_add(1));
                }
            } else {
                self.ask_pair(&name, true);
            }
        } else if !offer {
            self.join_message(state, &name);
        }
    }
    /// The joining computer's confirmation, with Also link with your other devices.
    fn ask_pair(&self, name: &str, link: bool) {
        self.ui(move |w| w.global::<Ui>().set_link_on(link));
        self.message(
            format!("Pair with {name}?"),
            String::new(),
            1,
            "Cancel",
            "Pair",
            "join-confirm",
        );
    }
    fn join_message(&self, state: &str, name: &str) {
        let (title, text, tile, secondary, primary, action) = match state {
            "rejected" => (
                "Couldn't pair".into(),
                "Paste the pairing link again to try again.".into(),
                0,
                "",
                "OK",
                "join-close",
            ),
            "no_link" => (
                "No pairing link on the clipboard".into(),
                "Copy the link on the other computer first.".into(),
                0,
                "",
                "OK",
                "ok",
            ),
            "garbled" => (
                "This pairing link is incomplete".into(),
                "Copy it again on the other computer.".into(),
                0,
                "",
                "OK",
                "ok",
            ),
            "own_code" => (
                format!("That's {}'s own code", this_computer()),
                "Use it on the other computer.".into(),
                0,
                "",
                "OK",
                "ok",
            ),
            "expired" => (
                "This code expired".into(),
                format!("Show a new one on {name} and try again."),
                0,
                "",
                "OK",
                "ok",
            ),
            "already_used" => (
                "This code was already used".into(),
                format!("Each code works once. Show a new one on {name}."),
                0,
                "",
                "OK",
                "ok",
            ),
            "already_paired" => (
                format!("Already paired with {name}"),
                String::new(),
                1,
                "",
                "OK",
                "ok",
            ),
            "unreachable" => (
                format!("Can't reach {name}"),
                "Make sure both are on the same network, or that Tailscale is connected.".into(),
                0,
                "Cancel",
                "Try Again",
                "join-retry",
            ),
            "fingerprint_mismatch" | "binding_mismatch" => (
                format!("Couldn't verify {name}"),
                "A different device answered at its address, so nothing was paired.".into(),
                3,
                "",
                "OK",
                "join-close",
            ),
            "update_required" => (
                "Update required".into(),
                "Update Pneu on both computers, then try again.".into(),
                0,
                "",
                "OK",
                "join-close",
            ),
            _ => (
                "Couldn't pair".into(),
                String::new(),
                0,
                "",
                "OK",
                "join-close",
            ),
        };
        self.message(title, text, tile, secondary, primary, action);
    }
    fn journal_path(&self) -> PathBuf {
        join_journal_path(&self.paths)
    }
    fn save_join(&self, join: &Join) -> Result<()> {
        crate::config::atomic_private(&self.journal_path(), &serde_json::to_vec(join)?)
    }
    fn forget_join(&self) {
        let _ = std::fs::remove_file(self.journal_path());
    }
    fn load_join(&self) -> Option<Join> {
        use std::io::Read;
        let path = self.journal_path();
        let folder = crate::storage::Folder::open(path.parent()?, false).ok()?;
        let file = crate::platform::fs::open_journal_at(&folder.file, path.file_name()?).ok()?;
        let mut bytes = Vec::new();
        file.take(32769).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 32768 {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    }
    async fn confirm_join(&mut self) {
        let Some(mut join) = self.join.take() else {
            return;
        };
        if join.origin != self.view() {
            self.cancel_join(&join).await;
            return;
        }
        let was_confirmed = join.confirmed;
        join.confirmed = true;
        join.cancel_requested = false;
        join.offer = false;
        join.reported.clear();
        // Persist BEFORE confirm: replacing the UI after daemon acceptance must
        // leave the next UI with the same opaque handle, including lost replies.
        if let Err(error) = self.save_join(&join) {
            join.confirmed = was_confirmed;
            self.join = Some(join);
            self.fail("Couldn't retain the pairing attempt", error);
            return;
        }
        let reply = self
            .command(ipc::Command::JoinConfirmScoped {
                attempt: join.attempt.clone(),
                enabled: join.link,
            })
            .await;
        if !self.current() {
            self.cancel_join(&join).await;
            return;
        }
        self.join = Some(join.clone());
        match reply {
            Ok(reply) if reply["state"] == "pairing" => {}
            Ok(reply) => self.join_message(reply["state"].as_str().unwrap_or_default(), &join.name),
            Err(error) => {
                if daemon_rejection(&error) {
                    self.join = None;
                    self.forget_join();
                }
                self.fail("Couldn't pair", error);
            }
        }
    }
    // ---------- Linking your devices (window_setup.rs) ----------

    fn remember(&mut self, change: impl FnOnce(&mut Remembered)) {
        change(&mut self.remembered);
        if let Err(error) = self.remembered.save(&setup_state_path(&self.paths)) {
            eprintln!("pneud window: couldn't keep a linking choice: {error:#}");
        }
    }
    async fn top_action(&mut self, yes: bool) {
        match self.origin.top.as_ref().clone() {
            Top::Offer { device } if yes && self.setup.requests().is_empty() => {
                if self.link(&device).await {
                    self.remember(|r| r.offer = Some(setup::LINKED.into()));
                }
            }
            Top::Readmit { record } => {
                if let Err(error) = self
                    .command(ipc::Command::ReadmissionDecision {
                        key: record.key,
                        allow: yes,
                    })
                    .await
                {
                    self.fail("Couldn't save the answer", error);
                }
            }
            Top::Request { devices, .. } => {
                self.answer_from(&self.origin.setup.clone(), &devices, yes)
                    .await;
            }
            _ => {}
        }
    }
    /// ✕ hides the offer for good; a request waits until Pneu restarts.
    fn top_close(&mut self) {
        match self.origin.top.as_ref().clone() {
            Top::Offer { .. } => self.remember(|r| r.offer = Some(setup::DISMISSED.into())),
            Top::Waiting { .. } => self.remember(|r| r.offer = Some(setup::DONE.into())),
            Top::Request { devices, .. } => {
                let keys: Vec<String> = devices
                    .iter()
                    .filter_map(|d| self.origin.setup.link(d))
                    .map(|l| setup::request_key(&self.origin.setup, l))
                    .collect();
                self.remember(|r| r.close(keys));
            }
            Top::Diagnostic { key, .. } => self.remember(|r| r.close([key])),
            Top::Readmit { record } => {
                self.remember(|r| r.close([setup::readmission_key(&record.key)]))
            }
            Top::None => {}
        }
    }
    /// Link covers every pairing here that isn't decided yet, and this one.
    async fn link(&mut self, device: &str) -> bool {
        let shown = &self.origin.setup;
        let Some(binding) = shown.binding_generations.get(device).copied().flatten() else {
            return false;
        };
        let Some(scope) = shown.link(device).and_then(|link| {
            serde_json::from_value::<pneu_proto::SetupScope>(json!(link.scope)).ok()
        }) else {
            return false;
        };
        let result = match pneu_proto::Id::new(device) {
            Ok(device) => {
                self.command(ipc::Command::LinkExistingDevice {
                    device,
                    binding_generation: Some(binding),
                    expected_scope: Some(scope),
                })
                .await
            }
            Err(error) => Err(error.into()),
        };
        match result {
            Ok(reply) => reply != json!(false),
            Err(error) => {
                self.fail("Couldn't link", error);
                false
            }
        }
    }
    /// Allow answers every device that asked, and the daemon also links this
    /// device's other undecided pairings. Don't allow answers only these.
    async fn answer(&mut self, devices: &[String], allow: bool) {
        self.answer_from(&self.origin.setup.clone(), devices, allow)
            .await;
    }
    async fn answer_from(&mut self, shown: &Setup, devices: &[String], allow: bool) {
        let requests = devices
            .iter()
            .filter_map(|device| {
                shown.link(device).filter(|l| l.prompt)?;
                Some(setup::Decision {
                    readmission: None,
                    device: pneu_proto::Id::new(device.as_str()).ok()?,
                    setup_generation: shown.generation,
                    binding_generation: shown.binding_generations.get(device).copied().flatten()?,
                })
            })
            .collect();
        if let Err(error) = self
            .command(ipc::Command::SetupDecisions { requests, allow })
            .await
        {
            self.fail(
                if allow {
                    "Couldn't link"
                } else {
                    "Couldn't save the answer"
                },
                error,
            );
        }
    }
    /// Remove from all devices, exactly as the confirmation showed it. The
    /// daemon refuses a changed membership, so nothing is claimed then.
    async fn remove(&mut self, device: &str) {
        let Some(removal) = self.removal.take().filter(|r| r.device == device) else {
            return;
        };
        let name = peers(&self.snapshot, &[])
            .into_iter()
            .find(|p| p.id == device)
            .map(|p| p.name)
            .unwrap_or_else(|| "the device".into());
        let result = async {
            let command = ipc::Command::RemoveFromSetup {
                device: pneu_proto::Id::new(removal.device.as_str())?,
                setup_generation: removal.setup_generation,
                binding_generation: removal.binding_generation,
                fingerprint: pneu_proto::Hash::new(removal.fingerprint.as_str())?,
            };
            self.command(command).await
        }
        .await;
        match result {
            // The page closes and the row goes; nothing follows later.
            Ok(_) => self.ui(|w| w.invoke_go(Page::Main)),
            Err(error) if daemon_rejection(&error) => {
                eprintln!("pneud window: couldn't remove: {error:#}");
                self.message(
                    format!("Couldn't remove {name}"),
                    "Nothing was changed. Try again.".into(),
                    0,
                    "",
                    "OK",
                    "ok",
                );
            }
            // A lost reply says nothing about whether it was removed.
            Err(error) => self.fail(&format!("Couldn't remove {name}"), error),
        }
    }
    fn device_link(&self, peer: &Peer) -> DeviceLink {
        let row = setup::row(&self.setup, peer);
        let (title, sub) = setup::row_text(row, &peer.name);
        let member = self.setup.member(&peer.id).is_some();
        let via = self
            .setup
            .link(&peer.id)
            .and_then(|l| l.introducer_name.as_deref())
            .filter(|_| member)
            .map(display_peer_text)
            .unwrap_or_default();
        let waiting = self
            .transfers
            .iter()
            .filter(|t| t.device == peer.id && !t.incoming() && t.state == "waiting_for_device")
            .map(artifact_key)
            .collect::<HashSet<_>>()
            .len();
        DeviceLink {
            row: match row {
                Row::None => LinkRow::None,
                Row::Link => LinkRow::Link,
                Row::Waiting => LinkRow::Waiting,
                Row::Theirs => LinkRow::Theirs,
                Row::Asked => LinkRow::Asked,
                Row::Update => LinkRow::Update,
                Row::Separate => LinkRow::Separate,
            },
            title: title.into(),
            sub: sub.into(),
            via: via.into(),
            member,
            waiting: waiting as i32,
        }
    }
    fn device_item(&self, peer: &Peer) -> DeviceItem {
        let mut item = peer.item();
        if let Some(link) = self.setup.link(&peer.id)
            && link.introduced_by.is_some()
        {
            if peer.health == Health::Lost {
                item.status = format!("Not allowed on {} yet", peer.name).into();
                item.tone = Tone::Warn;
                item.status_tone = Tone::Warn;
            } else if !link.ready && matches!(peer.health, Health::Offline | Health::Connecting) {
                item.status = format!("Waiting for {}", peer.name).into();
                item.tone = Tone::Neutral;
                item.status_tone = Tone::Neutral;
            }
        }
        item
    }
    fn top_card(&self, peers: &[Peer]) -> TopCard {
        match setup::top(&self.setup, peers, &self.remembered) {
            Top::Diagnostic { title, .. } => TopCard {
                kind: TopKind::Diagnostic,
                title: title.into(),
                error: String::new().into(),
                key: String::new().into(),
                phone: false,
            },
            Top::Readmit { record } => TopCard {
                kind: TopKind::Readmit,
                title: setup::readmission_text(&record).into(),
                key: setup::readmission_key(&record.key).into(),
                error: record.error.unwrap_or_default().into(),
                phone: false,
            },
            Top::None => TopCard::default(),
            Top::Offer { .. } if !self.snapshot.refreshing_devices.is_empty() => TopCard::default(),
            Top::Offer { .. } => TopCard {
                kind: TopKind::Offer,
                title: "Link your devices".into(),
                error: String::new().into(),
                key: String::new().into(),
                phone: false,
            },
            Top::Waiting { title } => TopCard {
                kind: TopKind::Waiting,
                title: title.into(),
                error: String::new().into(),
                key: String::new().into(),
                phone: false,
            },
            Top::Request { title, phone, .. } => TopCard {
                kind: TopKind::Request,
                title: title.into(),
                error: String::new().into(),
                key: String::new().into(),
                phone,
            },
        }
    }
    /// The done screen: what the new device linked with, or why it didn't.
    fn paired_detail(&self) -> (String, bool) {
        let Some(device) = self.paired.as_ref().and_then(|p| p.device.as_deref()) else {
            return Default::default();
        };
        if self.setup.link(device).is_some_and(|l| l.separate()) {
            return (setup::OWN.into(), true);
        }
        let me = self.status["self_id"].as_str().unwrap_or_default();
        let learned = self.setup.learned(&self.known_roster, &[device, me]);
        (setup::also_linked(&learned), false)
    }

    async fn cancel_join(&self, join: &Join) {
        let mut cancellation = join.clone();
        cancellation.cancel_requested = true;
        // Retain cancellation intent as well as the opaque handle across a
        // lost reply or UI replacement; recovery must not resume a cancelled UI.
        let _ = self.save_join(&cancellation);
        match self
            .command(ipc::Command::JoinCancel {
                attempt: join.attempt.clone(),
            })
            .await
        {
            Ok(_) => self.forget_join(),
            Err(error) if daemon_rejection(&error) => self.forget_join(),
            Err(_) => {}
        }
    }
    async fn poll_join(&mut self) {
        let Some(join) = self.join.as_ref().filter(|j| j.confirmed) else {
            return;
        };
        if join.origin != self.view() || self.restored_join {
            return;
        }
        let attempt = join.attempt.clone();
        let reply =
            match ipc::request(&self.paths.socket, ipc::Command::JoinStatus { attempt }).await {
                Ok(reply) => reply,
                Err(error) if daemon_rejection(&error) => {
                    let name = self.join.take().expect("join").name;
                    self.forget_join();
                    self.join_message("rejected", &name);
                    return;
                }
                // A transport failure says nothing about whether the join is active.
                Err(_) => return,
            };
        if !self.current() {
            return;
        }
        let state = reply["state"].as_str().unwrap_or_default();
        let join = self.join.as_mut().expect("join");
        match state {
            "pairing" => {}
            "confirmation" => {
                join.confirmed = false;
                let (name, link) = (join.name.clone(), join.link);
                self.ask_pair(&name, link);
            }
            "paired" => {
                // Only the daemon's authenticated outcome identifies this peer.
                let Some(device) = reply["device"]
                    .as_str()
                    .filter(|id| pneu_proto::Id::new(*id).is_ok())
                else {
                    return;
                };
                let join = self.join.take().expect("join");
                self.forget_join();
                self.paired = Some(Paired {
                    origin: self.view(),
                    name: join.name,
                    device: Some(device.into()),
                    at: Instant::now(),
                    linked_at: None,
                    linked: String::new(),
                });
                self.ui(|w| {
                    if w.global::<Ui>().get_dialog() == Dialog::Message {
                        w.global::<Ui>().set_dialog(Dialog::None);
                    }
                });
            }
            "unreachable" | "fingerprint_mismatch" | "binding_mismatch" | "update_required" => {
                if join.reported != state {
                    join.reported = state.into();
                    let name = join.name.clone();
                    self.join_message(state, &name);
                }
            }
            "cancelled" => {
                self.join = None;
                self.forget_join();
            }
            other => {
                let name = join.name.clone();
                self.join = None;
                self.forget_join();
                self.join_message(other, &name);
            }
        }
    }

    async fn close(&mut self, visible: &AtomicBool) {
        self.folder_job = None;
        self.open_folder_job = None;
        self.open_folder_origin = None;
        self.open_job = None;
        self.open_origin = None;
        self.chooser = None;
        self.paired = None;
        self.picker_active = false;
        self.send_request_id = None;
        self.picker_files.clear();
        self.picker_targets.clear();
        self.details = None;
        visible.store(false, Ordering::SeqCst);
        // Close is a window-lifetime action. Hide before awaiting cancellation;
        // navigation may have advanced since the worker's last status frame.
        let _ = self.weak.upgrade_in_event_loop(|w| {
            #[cfg(target_os = "macos")]
            super::macos::camera::close();
            w.global::<Ui>().set_dialog(Dialog::None);
            let _ = w.hide();
        });
        if let Some(join) = self.join.take() {
            // Closing the joining window stops the attempt; the code still works.
            self.cancel_join(&join).await;
        }
        self.release_phone_code().await;
    }
    /// v2 closed the phone QR with the window. A code shown on the Pairing Code
    /// page stays until it is used or expires.
    async fn release_phone_code(&mut self) {
        if let Some(token) = self.phone_token.take()
            && !self.code.shown.contains(token.as_str())
        {
            let _ = self.command(ipc::Command::ClosePairingIf { token }).await;
        }
    }
    async fn release(&mut self) {
        if let Some(join) = self.join.take() {
            self.cancel_join(&join).await;
        }
        self.release_phone_code().await;
    }

    async fn refresh(&mut self, value: Value, visible: &AtomicBool) -> Result<()> {
        self.origin = self.view();
        let mut snapshot: super::Snapshot = serde_json::from_value(value["snapshot"].clone())?;
        // Text is immutable for an ID. Fetch complete search keys once rather
        // than retransmitting up to 75 MiB on every status refresh.
        self.history_search
            .retain(|id, _| snapshot.clipboard_history.iter().any(|i| &i.id == id));
        let ids: Vec<_> = snapshot
            .clipboard_history
            .iter()
            .filter(|i| !i.image && !self.history_search.contains_key(&i.id))
            .map(|i| i.id.clone())
            .collect();
        if !ids.is_empty() {
            let chunks = self
                .command(ipc::Command::ClipboardHistorySearch { ids: ids.clone() })
                .await?;
            for id in ids {
                self.history_search.insert(id, String::new());
            }
            for chunk in chunks.as_array().context("invalid history search reply")? {
                if let (Some(id), Some(text)) = (chunk["id"].as_str(), chunk["text"].as_str())
                    && let Some(key) = self.history_search.get_mut(id)
                {
                    key.push_str(text);
                }
            }
        }
        for item in &mut snapshot.clipboard_history {
            item.search = self
                .history_search
                .get(&item.id)
                .cloned()
                .unwrap_or_default();
        }
        let config: Config = serde_json::from_value(value["config"].clone())?;
        self.status_error.clear();
        self.invitation_revision = value["invitation_revision"].as_u64().unwrap_or(0);
        self.transfers = snapshot.transfers.iter().map(Transfer::from).collect();
        let history_open = self.view().page == Page::History;
        update_history_order(
            &mut self.history_order,
            &self.snapshot.clipboard_history,
            &snapshot.clipboard_history,
            history_open,
        );
        self.snapshot = snapshot;
        #[cfg(target_os = "macos")]
        {
            let snapshot = self.snapshot.clone();
            let config = config.clone();
            let _ = slint::invoke_from_event_loop(move || {
                super::macos::native::update(snapshot, config)
            });
        }
        let initial_status = self.status.is_null();
        self.status = value;
        self.setup = Setup::parse(&self.status);
        let saved_join = if initial_status || self.join.is_none() {
            self.load_join()
        } else {
            None
        };
        if let Some(join) = saved_join.as_ref().filter(|j| j.cancel_requested) {
            self.cancel_join(join).await;
        }
        if initial_status && let Some(mut join) = saved_join.filter(|j| !j.cancel_requested) {
            join.origin = self.view();
            self.join = Some(join);
            self.restored_join = true;
            self.ui(|w| w.invoke_open_add(1));
        }
        if self.restored_join
            && !initial_status
            && self.view().page == Page::Add
            && self.view().add_tab == 1
        {
            let view = self.view();
            if let Some(join) = &mut self.join {
                join.origin = view;
            }
            self.restored_join = false;
            if let Some(join) = &self.join
                && !join.confirmed
                && !join.offer
            {
                self.ask_pair(&join.name, join.link);
            }
        }
        let first_run = self.snapshot.devices.is_empty();
        let current = self.status["window_request"].as_u64().unwrap_or(0);
        let add_device = self.status["add_device"].as_bool().unwrap_or(false);
        let history_page =
            config.clipboard_history && self.status["history_page"].as_bool().unwrap_or(false);
        // A joined notification's Remove… also applies when this child was
        // launched with the same request number in PNEU_WINDOW_REQUEST.
        let remove_device = self.status["remove_device"]
            .as_str()
            .filter(|id| pneu_proto::Id::new(*id).is_ok())
            .filter(|id| {
                self.snapshot
                    .devices
                    .iter()
                    .any(|d| d.device.as_str() == *id)
            })
            .map(String::from);
        if !self.restored_join
            && (current != self.request
                || (initial_status && (add_device || history_page || remove_device.is_some())))
        {
            self.request = current;
            self.remove_request = remove_device.clone();
            let was_visible = visible.swap(true, Ordering::SeqCst);
            let choose_folder = current & 1 != 0;
            let preserve_send = !self.status["pending_send"].is_null()
                && (self.picker_active || self.chooser.is_some());
            let requested_page = self.requested_page.take();
            self.ui(move |w| {
                #[cfg(target_os = "linux")]
                if !initial_status {
                    let _ = w.hide();
                    let _ = w.show();
                }
                #[cfg(target_os = "macos")]
                super::macos::native::activate(w);
                #[cfg(windows)]
                super::windows_window::activate(w);
                if history_page {
                    w.invoke_go(Page::History);
                    return;
                }
                if let Some(page) = requested_page {
                    w.invoke_go(page);
                    return;
                }
                // A Windows tray click presents the existing page and its state.
                if cfg!(windows)
                    && was_visible
                    && !choose_folder
                    && !add_device
                    && remove_device.is_none()
                {
                    return;
                }
                // Reopening starts at the top: Add Device without devices, else
                // the main page (or Settings for Choose Folder…).
                if preserve_send {
                    return;
                }
                if let Some(device) = remove_device {
                    w.invoke_open_device(device.into());
                } else if first_run || add_device {
                    w.invoke_open_add(0);
                } else if choose_folder {
                    w.invoke_go(Page::Settings);
                } else {
                    w.invoke_go(Page::Main);
                }
            });
            if choose_folder && self.folder_job.is_none() {
                self.folder_origin = Some(self.view());
                let socket = self.socket();
                self.folder_job = Some(Box::pin(async move { change_folder(&socket).await }));
            }
            return Ok(());
        }
        if !visible.load(Ordering::SeqCst) {
            return Ok(());
        }
        let view = self.view();
        if first_run
            && !is_pairing_page(view.page)
            && self.paired.is_none()
            && !matches!(view.page, Page::Settings | Page::History)
        {
            // Unpairing the last device returns to Add Device.
            self.ui(|w| w.invoke_open_add(0));
            return Ok(());
        }
        // A removed device says so once, then shows what it has left.
        let generation = self.setup.generation;
        if self.setup.withdrawn
            && self.remembered.removed != Some(generation)
            && self.removed_shown != Some(generation)
            && self.join.is_none()
            && !self.picker_active
        {
            self.removed_shown = Some(generation);
            let this = this_computer();
            self.message(
                format!("T{} was removed from your devices", &this[1..]),
                String::new(),
                0,
                "",
                "OK",
                "removed-ok",
            );
        }
        if self.remembered.offer.as_deref() == Some(setup::LINKED)
            && self.setup.unanswered().is_empty()
        {
            self.remember(|r| r.offer = Some(setup::DONE.into()));
        }
        if let Ok(request) =
            serde_json::from_value::<SendRequest>(self.status["pending_send"].clone())
        {
            if self.handled_send.as_deref() == Some(&request.id) {
                let _ = self
                    .command(ipc::Command::AcknowledgeSend {
                        request: request.id,
                    })
                    .await;
            } else if self.chooser.is_none() && !self.picker_active {
                self.handled_send = Some(request.id.clone());
                self.receive_send(request.clone()).await;
                let _ = self
                    .command(ipc::Command::AcknowledgeSend {
                        request: request.id,
                    })
                    .await;
            }
        }
        // A new clipboard hint never switches tabs under the user.
        if view.page == Page::Add && view.add_tab == 1 {
            let link = self.snapshot.pairing_clipboard.clone();
            if self.live_pairing_link != link
                || self.live_pairing_revision != self.snapshot.pairing_clipboard_revision
            {
                self.live_pairing_link = link.clone();
                self.live_pairing_revision = self.snapshot.pairing_clipboard_revision;
                if link
                    .as_ref()
                    .is_some_and(|uri| self.dismissed_offer.as_ref().is_some_and(|old| old != uri))
                {
                    self.dismissed_offer = None;
                }
                if self.join.as_ref().is_some_and(|j| j.offer && !j.confirmed)
                    && let Some(join) = self.join.take()
                {
                    self.cancel_join(&join).await;
                }
                if let Some(link) = link
                    && self.join.is_none()
                    && self.dismissed_offer.as_deref() != Some(link.as_str())
                {
                    self.paste(link, true).await;
                }
            }
        } else {
            self.live_pairing_link = None;
        }
        if self.join.as_ref().is_some_and(|j| {
            j.offer
                && !j.confirmed
                && pneu_proto::PairingUri::parse(&j.uri)
                    .is_ok_and(|uri| uri.expires_at <= crate::now_ms())
        }) && let Some(join) = self.join.take()
        {
            self.cancel_join(&join).await;
        }
        let peers = peers(&self.snapshot, &self.transfers);
        if view.page == Page::Device
            && !peers.iter().any(|p| p.id == view.device)
            && self.paired.is_none()
        {
            self.ui(|w| w.invoke_go(Page::Main));
        }
        if view.page == Page::History && !config.clipboard_history {
            self.ui(|w| w.invoke_go(Page::Main));
        }
        let now = crate::now_ms();
        let mut invitation = serde_json::from_value::<Option<pneu_proto::PairingUri>>(
            self.status["invitation"].clone(),
        )
        .unwrap_or(None);

        self.poll_join().await;

        // Finish only for the displayed invitation that was actually consumed,
        // including a direct pairing to a device already in the list.
        if is_pairing_page(view.page)
            && self.join.is_none()
            && self.current()
            && self.paired.is_none()
            && let Some(peer) = peers.iter().find(|p| {
                self.snapshot
                    .pairing_completion
                    .as_ref()
                    .is_some_and(|done| {
                        done.device.as_str() == p.id
                            && self.seen_pairing_token.as_ref() != Some(&done.token)
                            && (self.phone_token.as_ref() == Some(&done.token)
                                || self.code.token.as_ref() == Some(&done.token))
                    })
                    && !p.name.is_empty()
            })
        {
            self.seen_pairing_token = self
                .snapshot
                .pairing_completion
                .as_ref()
                .map(|done| done.token.clone());
            self.paired = Some(Paired {
                origin: view.clone(),
                name: peer.name.clone(),
                device: Some(peer.id.clone()),
                at: Instant::now(),
                linked_at: None,
                linked: String::new(),
            });
            #[cfg(target_os = "macos")]
            if view.page == Page::Scan {
                self.ui(|_| super::macos::camera::close());
            }
        }
        // Phone pairing QR (v2): keep an invitation open while the tab shows.
        let phone_tab = view.page == Page::Add && view.add_tab == 0;
        if phone_tab {
            if let Some(invitation) = &invitation {
                if self.phone_token.as_ref() != Some(&invitation.token)
                    && let Ok(uri) = invitation.build()
                    && let Ok(pixels) = qr_pixels(&uri)
                {
                    self.phone_token = Some(invitation.token.clone());
                    self.phone_failed = false;
                    self.ui(move |w| w.global::<Ui>().set_qr(slint::Image::from_rgba8(pixels)));
                }
            } else {
                // Never show a URI which Core no longer accepts.
                if self.phone_token.is_some() {
                    self.ui(|w| w.global::<Ui>().set_qr(slint::Image::default()));
                }
                if self.snapshot.pairing.is_none()
                    && now.saturating_sub(self.last_pair_attempt) >= 3000
                {
                    self.last_pair_attempt = now;
                    self.phone_failed = self.command(ipc::Command::EnsurePair).await.is_err();
                }
            }
        }
        if view.page == Page::Code
            && invitation.is_none()
            && self.code.token.is_none()
            && now.saturating_sub(self.code.last_attempt) >= 3000
        {
            self.show_code(false).await;
            invitation = serde_json::from_value(self.status["invitation"].clone()).unwrap_or(None);
        }
        if view.page == Page::Code
            && let Some(invitation) = &invitation
        {
            self.adopt_code(invitation);
        }
        if self
            .paired
            .as_ref()
            .is_some_and(|p| p.origin != self.view())
        {
            self.paired = None;
        }
        let (detail, _) = self.paired_detail();
        if let Some(paired) = &mut self.paired
            && detail.starts_with("Also linked:")
            && paired.linked != detail
        {
            paired.linked = detail;
            paired.linked_at = Some(Instant::now());
        }
        // MEMBERS and READY can arrive in several batches after direct pairing.
        // Keep a five-second discovery window even before the first introduction,
        // then leave each updated line readable for three seconds.
        let linking = self.paired.as_ref().is_some_and(|p| {
            p.device.as_deref().is_some_and(|device| {
                self.setup
                    .link(device)
                    .is_none_or(|l| l.scope == "enabled" && !l.separate())
            })
        });
        if let Some(paired) = &self.paired
            && paired.at.elapsed()
                >= if linking {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(1500)
                }
            && paired
                .linked_at
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(3))
        {
            let device = paired.device.clone();
            self.paired = None;
            self.ui(move |w| match device {
                Some(device) => w.invoke_open_device(device.into()),
                None => w.invoke_go(Page::Main),
            });
        }

        // Peer switches: on entering the page, after changes, then every 2 s.
        let device_peer = peers.iter().find(|p| p.id == view.device).cloned();
        if view.page == Page::Device
            && let Some(peer) = &device_peer
            && self
                .peer_settings
                .as_ref()
                .is_none_or(|(id, _, at)| *id != peer.id || at.elapsed() >= Duration::from_secs(2))
            && let Ok(id) = pneu_proto::Id::new(peer.id.as_str())
        {
            match self
                .command(ipc::Command::PeerSettings { device: id })
                .await
            {
                Ok(reply) if self.current() => {
                    self.peer_settings = Some((peer.id.clone(), reply, Instant::now()))
                }
                Ok(_) => {}
                Err(_) => self.peer_settings = Some((peer.id.clone(), Value::Null, Instant::now())),
            }
        }

        // Checked only while Can't connect? shows, and at most every 5 s.
        if view.page == Page::Main
            && unreachable(&peers)
            && self
                .tailscale
                .is_none_or(|(_, at)| at.elapsed() >= Duration::from_secs(5))
        {
            self.tailscale = Some((tailscale_running(), Instant::now()));
        }

        if !self.current() {
            return Ok(());
        }
        self.show_details();
        let frame = self.frame(
            &config,
            &peers,
            device_peer.as_ref(),
            invitation.as_ref(),
            first_run,
            now,
        );
        let targets = contract::list::<SendTarget>(&self.status, "send_targets");
        let view = self.view();
        let remove = frame.device_link.member
            && view.page == Page::Device
            && self.remove_request.as_ref() == Some(&view.device);
        if remove {
            self.remove_request = None;
        }
        let shown_setup = Arc::new(self.setup.clone());
        let shown_top = Arc::new(setup::top(&self.setup, &peers, &self.remembered));
        if self.pushed.as_ref() != Some(&frame)
            || self.view().setup != shown_setup
            || self.view().top != shown_top
            || self.view().targets.as_ref() != Some(&targets)
            || remove
        {
            self.pushed = Some(frame.clone());
            let view = self.view.clone();
            self.ui(move |w| {
                {
                    let mut shown = view.lock().expect("window view");
                    shown.targets = Some(targets);
                    shown.setup = shown_setup;
                    shown.top = shown_top;
                }
                apply(w, frame);
                if remove {
                    w.invoke_prepare_remove();
                }
            });
        }
        Ok(())
    }

    fn frame(
        &self,
        config: &Config,
        peers: &[Peer],
        device: Option<&Peer>,
        invitation: Option<&pneu_proto::PairingUri>,
        first_run: bool,
        now: u64,
    ) -> Frame {
        let mode = ClipboardMode::from_config(
            contract::setting(&self.status, "clipboard_mode"),
            config.clipboard_sync_to_phone,
        );
        let flag = |key: &str| {
            contract::setting(&self.status, key)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let error = if !self.status_error.is_empty() {
            self.status_error.clone()
        } else {
            self.snapshot
                .error
                .as_ref()
                .map(|e| e.banner())
                .unwrap_or_else(|| self.action_error.clone())
        };
        let pairing = self
            .join
            .as_ref()
            .filter(|j| j.confirmed)
            .map(|j| j.name.clone())
            .or_else(|| self.snapshot.pairing.as_deref().map(display_peer_text))
            .unwrap_or_default();
        let paired = self
            .paired
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let code_live = invitation
            .is_some_and(|i| self.code.token.as_ref() == Some(&i.token) && i.expires_at > now);
        let code_state = if !paired.is_empty() {
            CodeState::Paired
        } else if !pairing.is_empty() {
            CodeState::Pairing
        } else if code_live {
            CodeState::Showing
        } else if self.code.offline || !self.code.error.is_empty() {
            CodeState::Offline
        } else if self.code.token.is_none() {
            // Waiting for the first code: show nothing rather than "expired".
            CodeState::Showing
        } else {
            CodeState::Expired
        };
        let left = self.code.expires_at.saturating_sub(now).div_ceil(1000);
        let (paired_detail, paired_only) = self.paired_detail();
        let (device_item, device_summary, device_action, device_settings, device_note) =
            match device {
                Some(peer) => {
                    let (mut summary, mut action) = device_summary(peer);
                    let introduced = self
                        .setup
                        .link(&peer.id)
                        .is_some_and(|l| l.introduced_by.is_some());
                    if introduced
                        && self.setup.link(&peer.id).is_some_and(|l| !l.ready)
                        && matches!(peer.health, Health::Offline | Health::Connecting)
                    {
                        summary.title = format!("Waiting for {}", peer.name);
                        summary.detail =
                            format!("Open Pneu on {}, or pair with it directly", peer.name);
                        action = "pair-warning";
                    }
                    if introduced && peer.health == Health::Lost {
                        summary.title = format!("{} hasn't allowed this computer", peer.name);
                        summary.detail =
                            format!("Allow it on {}, or pair with it directly", peer.name);
                        summary.link.tube = Tube::Dashed;
                        summary.link.tiles[0].tone = Tone::Warn;
                        action = "pair-warning";
                    }
                    let (settings, mut note) = self
                        .peer_settings
                        .as_ref()
                        .filter(|(id, _, _)| *id == peer.id)
                        .map(|(_, reply, _)| device_settings(peer, reply, mode))
                        .unwrap_or_default();
                    if introduced
                        && self.setup.link(&peer.id).is_some_and(|l| !l.hello_seen)
                        && note.starts_with("Update Pneu")
                    {
                        note = format!("Switches sync when {} connects.", peer.name);
                    }
                    (
                        self.device_item(peer),
                        summary,
                        action.to_owned(),
                        settings,
                        note,
                    )
                }
                None => Default::default(),
            };
        // Phones never accept a desktop dial, so they cannot report refusal.
        // A fresh phone connection is evidence that this computer still belongs.
        let pair_again = !peers.is_empty()
            && ((peers.iter().all(Peer::lost)
                && peers.iter().all(|p| {
                    p.health != Health::Lost
                        || self
                            .setup
                            .link(&p.id)
                            .is_none_or(|l| l.introduced_by.is_none())
                }))
                || (self.snapshot.setup_refusal_since.is_some()
                    && peers.iter().any(|p| !p.phone)
                    && peers
                        .iter()
                        .filter(|p| !p.phone)
                        .all(|p| p.health == Health::Lost)
                    && peers.iter().filter(|p| p.phone).all(|p| {
                        !p.connected()
                            && p.last_seen <= self.snapshot.setup_refusal_since.unwrap_or(0)
                    })));
        let presented: Vec<_> = peers
            .iter()
            .cloned()
            .map(|mut p| {
                if p.health == Health::Lost
                    && self
                        .setup
                        .link(&p.id)
                        .is_some_and(|l| l.introduced_by.is_some())
                {
                    p.health = Health::Offline;
                }
                p
            })
            .collect();
        let mut hero = summary(&presented, &self.transfers);
        if pair_again {
            hero.title = "Pairing lost".into();
            hero.detail = format!("Your devices don't recognize {}", this_computer());
        } else if peers.iter().all(|p| !p.connected())
            && peers.iter().any(|p| {
                p.health == Health::Lost
                    && self
                        .setup
                        .link(&p.id)
                        .is_some_and(|l| l.introduced_by.is_some())
            })
        {
            hero.title = "Waiting for your devices".into();
            hero.detail.clear();
            hero.link.tube = Tube::Dashed;
        }
        Frame {
            error,
            summary: hero,
            devices: peers.iter().map(|p| self.device_item(p)).collect(),
            recent: recent(peers, &self.transfers),
            history: {
                let mut items: Vec<_> = self
                    .history_order
                    .iter()
                    .filter_map(|id| {
                        self.snapshot
                            .clipboard_history
                            .iter()
                            .find(|i| &i.id == id)
                            .cloned()
                    })
                    .collect();
                // Stable sorting keeps Recent frozen and puts new pins first.
                items.sort_by_key(|i| (i.pinned.is_none(), std::cmp::Reverse(i.pinned)));
                items
            },
            keep_history: config.clipboard_history,
            manual_clipboard: mode == ClipboardMode::Manual,
            pair_again,
            motion: animations_enabled(&super::palette::globals()),
            device: device_item,
            device_summary,
            device_action,
            device_settings,
            device_note,
            device_link: device.map(|p| self.device_link(p)).unwrap_or_default(),
            top: self.top_card(peers),
            link_full: self.setup.full(),
            paired_detail,
            paired_only,
            first_run,
            network: !self.phone_failed,
            pairing,
            paired,
            offer: self
                .join
                .as_ref()
                .filter(|j| j.offer)
                .map(|j| j.name.clone())
                .unwrap_or_default(),
            code_state,
            code_error: self.code.error.clone(),
            code_expires: format!("Expires in {}:{:02}", left / 60, left % 60),
            code_link: if code_live {
                self.code.uri.clone()
            } else {
                String::new()
            },
            clipboard_mode: match mode {
                ClipboardMode::Automatic => 0,
                ClipboardMode::Manual => 1,
                ClipboardMode::Off => 2,
            },
            include_images: config.clipboard_include_images,
            choose_each_time: flag("choose_devices_each_time"),
            queue_offline: flag("queue_offline"),
            save_to_folder: config.save_to_folder,
            folder: middle_elide(&super::display_path(&config.save_path), 26),
            notifications: config.notification_preview,
            copy_images: config.copy_image_to_clipboard,
            viewer: config.open_in_viewer,
            name: display_peer_text(&config.name),
            unreachable: unreachable(peers),
            tailscale: self.tailscale.is_some_and(|(on, _)| on),
        }
    }
}

fn apply(w: &PneuWindow, frame: Frame) {
    let ui = w.global::<Ui>();
    ui.set_error(frame.error.into());
    ui.set_summary(frame.summary.summary());
    ui.set_pair_again(frame.pair_again);
    sync(ui.get_devices(), &frame.devices);
    sync(ui.get_recent(), &frame.recent);
    ui.set_keep_history(frame.keep_history);
    if frame.history.is_empty() {
        ui.set_history_query("".into());
    }
    ui.set_history_pins(frame.history.iter().filter(|i| i.pinned.is_some()).count() as i32);
    let history: Vec<_> = frame
        .history
        .iter()
        .map(|item| HistoryItem {
            id: item.id.clone().into(),
            title: item.title.clone().into(),
            search: item.search.clone().into(),
            pinned: item.pinned.is_some(),
            url: item.url,
            picture: item.image,
            detail: format!(
                "{} · {}",
                item.origin
                    .as_ref()
                    .map(|o| format!("From {}", display_peer_text(o)))
                    .unwrap_or_else(|| if cfg!(target_os = "macos") {
                        "This Mac".into()
                    } else {
                        "This computer".into()
                    }),
                seen(item.at)
            )
            .into(),
            thumbnail: item
                .thumbnail
                .as_ref()
                .and_then(|p| slint::Image::load_from_path(p).ok())
                .unwrap_or_default(),
        })
        .collect();
    sync(ui.get_history(), &history);
    ui.set_manual_clipboard(frame.manual_clipboard);
    ui.set_motion(frame.motion);
    ui.set_device_revision(ui.get_navigation_revision());
    ui.set_device(frame.device);
    ui.set_device_summary(frame.device_summary.summary());
    ui.set_device_action(frame.device_action.into());
    sync(ui.get_device_settings(), &frame.device_settings);
    ui.set_device_note(frame.device_note.into());
    ui.set_device_link(frame.device_link);
    ui.set_top(frame.top);
    ui.set_link_full(frame.link_full);
    ui.set_paired_detail(frame.paired_detail.into());
    ui.set_paired_only(frame.paired_only);
    ui.set_first_run(frame.first_run);
    ui.set_network(frame.network);
    ui.set_pairing(frame.pairing.into());
    ui.set_paired(frame.paired.into());
    ui.set_offer(frame.offer.into());
    ui.set_code_state(frame.code_state);
    ui.set_code_error(frame.code_error.into());
    ui.set_code_expires(frame.code_expires.into());
    ui.set_code_link(frame.code_link.into());
    ui.set_clipboard_mode(frame.clipboard_mode);
    ui.set_include_images(frame.include_images);
    ui.set_choose_each_time(frame.choose_each_time);
    ui.set_queue_offline(frame.queue_offline);
    ui.set_save_to_folder(frame.save_to_folder);
    ui.set_folder(frame.folder.into());
    ui.set_notifications(frame.notifications);
    ui.set_copy_images(frame.copy_images);
    ui.set_viewer(frame.viewer);
    ui.set_name(frame.name.into());
    ui.set_unreachable(frame.unreachable);
    ui.set_tailscale(frame.tailscale);
}

/// Update a list in place so rows (and their keyboard focus) survive polling.
fn sync<T: Clone + PartialEq + 'static>(model: ModelRc<T>, items: &[T]) {
    let Some(rows) = model.as_any().downcast_ref::<VecModel<T>>() else {
        return;
    };
    for (i, item) in items.iter().enumerate() {
        if i < rows.row_count() {
            if rows.row_data(i).as_ref() != Some(item) {
                rows.set_row_data(i, item.clone());
            }
        } else {
            rows.push(item.clone());
        }
    }
    while rows.row_count() > items.len() {
        rows.remove(rows.row_count() - 1);
    }
}

// Peer names are labels, not paragraphs. Keep their bytes on the wire and in
// storage unchanged while rendering all line separators as spaces in the window.
pub(crate) use super::this_computer;

pub(crate) fn display_peer_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect()
}
fn middle_elide(text: &str, max: usize) -> String {
    let chars: Vec<_> = text.chars().collect();
    if chars.len() <= max {
        text.into()
    } else {
        format!(
            "{}…{}",
            chars[..max / 2].iter().collect::<String>(),
            chars[chars.len() - (max - 1) / 2..]
                .iter()
                .collect::<String>()
        )
    }
}
// Plasma's global animation slider applies to the Link as well. Re-read with
// each status update so changing the system preference needs no daemon restart.
fn animations_enabled(ini: &str) -> bool {
    super::palette::value(ini, "KDE", "AnimationDurationFactor")
        .and_then(|value| value.parse::<f64>().ok())
        .is_none_or(|factor| factor > 0.0)
}
pub fn qr_pixels(uri: &str) -> Result<slint::SharedPixelBuffer<slint::Rgba8Pixel>> {
    qr_image(uri, qrcode::EcLevel::M, 4)
}
/// Modules of `scale` pixels inside a four-module white quiet zone.
fn qr_image(
    text: &str,
    level: qrcode::EcLevel,
    scale: usize,
) -> Result<slint::SharedPixelBuffer<slint::Rgba8Pixel>> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), level)?;
    let modules = code.width();
    let offset = 4 * scale;
    let size = (modules + 8) * scale;
    let mut image = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size as u32, size as u32);
    image.make_mut_bytes().fill(255);
    for y in 0..modules {
        for x in 0..modules {
            if code[(x, y)] == qrcode::Color::Dark {
                for py in offset + y * scale..offset + (y + 1) * scale {
                    for px in offset + x * scale..offset + (x + 1) * scale {
                        let index = (py * size + px) * 4;
                        image.make_mut_bytes()[index..index + 3].fill(0);
                    }
                }
            }
        }
    }
    Ok(image)
}
async fn job_result<T>(
    job: &mut Option<futures_util::future::BoxFuture<'_, Result<T>>>,
) -> Result<T> {
    match job {
        Some(job) => job.await,
        None => std::future::pending().await,
    }
}
/// Opens a web page in the default browser.
async fn open_web(url: &'static str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        crate::actions::open_uri(url, &zbus::Connection::session().await?).await
    }
    #[cfg(target_os = "macos")]
    {
        let (send, receive) = tokio::sync::oneshot::channel();
        slint::invoke_from_event_loop(move || {
            let _ = send.send(super::macos::native::open_url(url));
        })?;
        receive.await?
    }
    #[cfg(windows)]
    {
        crate::actions::open_url(url).await
    }
}
async fn open_folder(socket: &Path, folder: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", windows))]
    {
        let _ = folder;
        ipc::request(socket, ipc::Command::OpenFolder).await?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = socket;
        let folder = folder.to_owned();
        let create = folder.clone();
        let _anchor =
            tokio::task::spawn_blocking(move || crate::storage::Folder::open(&create, true))
                .await??;
        let (send, receive) = tokio::sync::oneshot::channel();
        slint::invoke_from_event_loop(move || {
            let _ = send.send(super::macos::native::open_path(&folder));
        })?;
        receive.await?
    }
}

async fn change_folder(socket: &Path) -> Result<()> {
    let value = ipc::request(socket, ipc::Command::UiStatus).await?;
    let config: Config = serde_json::from_value(value["config"].clone())?;
    if let Some(folder) = choose_folder(&config.save_path).await? {
        ipc::request(socket, ipc::Command::SaveFolder { folder }).await?;
    }
    Ok(())
}
fn recent_file(path: &Path) -> Result<crate::actions::ReceivedFile> {
    let file =
        crate::storage::open_regular_file(path).map_err(|_| crate::actions::FileUnavailable)?;
    crate::actions::ReceivedFile::from_file(path.to_owned(), file)
}
#[cfg(target_os = "macos")]
async fn open_recent(file: crate::actions::ReceivedFile) -> Result<()> {
    super::macos::native::open_received_local(file).await
}
#[cfg(windows)]
pub async fn choose_folder(current: &Path) -> Result<Option<PathBuf>> {
    Ok(
        super::windows_picker::choose("Choose Folder", Some(current))
            .await?
            .into_iter()
            .next(),
    )
}
#[cfg(windows)]
pub async fn choose_files(title: &str) -> Result<Option<Vec<PathBuf>>> {
    let paths = super::windows_picker::choose(title, None).await?;
    Ok((!paths.is_empty()).then_some(paths))
}
#[cfg(target_os = "macos")]
pub async fn choose_folder(current: &Path) -> Result<Option<PathBuf>> {
    Ok(
        super::macos::native::choose("Choose Folder", true, Some(current))
            .await?
            .and_then(|paths| paths.into_iter().next()),
    )
}
#[cfg(target_os = "macos")]
pub async fn choose_files(title: &str) -> Result<Option<Vec<PathBuf>>> {
    super::macos::native::choose(title, false, None).await
}
#[cfg(target_os = "linux")]
async fn open_file_on(file: crate::actions::ReceivedFile, bus: zbus::Connection) -> Result<()> {
    crate::actions::begin_open_received_file(&file, &bus, None)
        .await?
        .await
}
#[cfg(target_os = "linux")]
pub async fn choose_folder(current: &Path) -> Result<Option<PathBuf>> {
    choose_folder_on(zbus::Connection::session().await?, current).await
}
#[cfg(target_os = "linux")]
async fn choose_folder_on(connection: zbus::Connection, current: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    use zbus::zvariant::Value;
    let request = super::portal::Request::new(&connection).await?;
    let proxy = request
        .method_proxy("org.freedesktop.portal.FileChooser")
        .await?;
    let mut folder = current.as_os_str().as_bytes().to_vec();
    folder.push(0);
    let options = HashMap::from([
        ("handle_token", Value::from(request.token.as_str())),
        ("directory", Value::from(true)),
        ("multiple", Value::from(false)),
        ("current_folder", Value::from(folder)),
    ]);
    let path =
        super::portal::accept(proxy.call("OpenFile", &("", "Choose Folder", options))).await?;
    let Some(values) = request.response(path).await? else {
        return Ok(None);
    };
    let uris: Vec<String> = values
        .get("uris")
        .context("folder URI")?
        .try_clone()?
        .try_into()?;
    let uri = url::Url::parse(uris.first().context("selected folder")?)?;
    Ok(Some(
        uri.to_file_path()
            .map_err(|_| anyhow::anyhow!("folder is not local"))?,
    ))
}
/// The system file picker (XDG portal); several files at once.
#[cfg(target_os = "linux")]
pub async fn choose_files(title: &str) -> Result<Option<Vec<PathBuf>>> {
    choose_files_on(zbus::Connection::session().await?, title).await
}
#[cfg(target_os = "linux")]
async fn choose_files_on(
    connection: zbus::Connection,
    title: &str,
) -> Result<Option<Vec<PathBuf>>> {
    use zbus::zvariant::Value;
    let request = super::portal::Request::new(&connection).await?;
    let proxy = request
        .method_proxy("org.freedesktop.portal.FileChooser")
        .await?;
    let options = HashMap::from([
        ("handle_token", Value::from(request.token.as_str())),
        ("multiple", Value::from(true)),
    ]);
    let path = super::portal::accept(proxy.call("OpenFile", &("", title, options))).await?;
    let Some(values) = request.response(path).await? else {
        return Ok(None);
    };
    let uris: Vec<String> = values
        .get("uris")
        .context("file URIs")?
        .try_clone()?
        .try_into()?;
    uris.iter()
        .map(|uri| {
            url::Url::parse(uri)?
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("file is not local"))
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn resume_recent_failure_and_retry_strings() {
        let ps = peers(&snapshot(two()), &[]);
        let mut failed = transfer(
            "resume",
            1,
            "Recording.mp4",
            "failed",
            1_200_000_000,
            4_000_000_000,
        );
        failed.error = Some("Connection lost".into());
        assert_eq!(
            transfer_failure(&failed, "Desktop"),
            format!(
                "Connection lost · {} sent",
                size_pair(failed.sent, failed.size)
            )
        );
        assert!(
            recent(&ps, &[failed.clone()])[0]
                .detail
                .contains("Connection lost ·")
        );
        failed.sent = 0;
        assert_eq!(recent(&ps, &[failed.clone()])[0].detail, "Connection lost");
        failed.state = "sending".into();
        failed.sent = 1_200_000_000;
        let rows = recent(&ps, &[failed]);
        assert_eq!(rows[0].progress, 0.3);
        assert!(rows[0].detail.starts_with("Sending ·"));
    }
    #[test]
    fn peer_labels_normalize_a_protocol_limit_name_and_unicode_line_breaks() {
        let name = format!("{}xy", "Phone\n".repeat(21));
        assert_eq!(name.len(), 128);
        assert_eq!(
            display_peer_text(&name),
            format!("{}xy", "Phone ".repeat(21))
        );
        assert_eq!(
            display_peer_text("Galaxy\r\nS25\t+\u{2028}Phone\u{2029}"),
            "Galaxy  S25 + Phone "
        );
    }
    #[test]
    fn link_respects_plasma_reduced_motion() {
        assert!(animations_enabled(""));
        assert!(animations_enabled("[KDE]\nAnimationDurationFactor=0.5\n"));
        assert!(!animations_enabled("[KDE]\nAnimationDurationFactor=0\n"));
    }

    fn id(n: u8) -> pneu_proto::Id {
        pneu_proto::Id::from_bytes([n; 16])
    }
    fn device(
        n: u8,
        name: &str,
        phone: bool,
        state: &str,
        last_seen: u64,
    ) -> crate::core::mesh::DeviceSnapshot {
        crate::core::mesh::DeviceSnapshot {
            device: id(n),
            name: name.into(),
            role: if phone { "phone" } else { "computer" }.into(),
            state: state.into(),
            last_seen,
            dialer: None,
            route: (state == "connected").then(|| "Local network".into()),
        }
    }
    fn snapshot(devices: Vec<crate::core::mesh::DeviceSnapshot>) -> super::super::Snapshot {
        super::super::Snapshot {
            paired: !devices.is_empty(),
            paired_devices: devices.iter().map(|d| d.device.clone()).collect(),
            connected_devices: devices
                .iter()
                .filter(|d| d.state == "connected")
                .map(|d| d.device.clone())
                .collect(),
            devices,
            ..Default::default()
        }
    }
    fn transfer(op: &str, device: u8, file: &str, state: &str, sent: u64, size: u64) -> Transfer {
        Transfer {
            operation: op.into(),
            transfer_id: {
                use sha2::{Digest, Sha256};
                hex::encode(&Sha256::digest(file.as_bytes())[..16])
            },
            device: id(device).as_str().into(),
            file_name: file.into(),
            size,
            sent,
            state: state.into(),
            ..Default::default()
        }
    }

    #[test]
    fn link_titles_follow_the_rules_table() {
        let phone = || device(1, "Galaxy S25+", true, "connected", 0);
        let mac = |state| device(2, "MacBook Air", false, state, 0);
        let office = |state| device(3, "Office-PC", false, state, 0);
        let s = |devices| {
            let snapshot = snapshot(devices);
            let peers = peers(&snapshot, &[]);
            let summary = summary(&peers, &[]);
            (summary.title, summary.detail, summary.link.tube)
        };
        assert_eq!(
            s(vec![phone(), mac("connected")]),
            (
                "Connected".into(),
                "Galaxy S25+ and MacBook Air".into(),
                Tube::Solid
            )
        );
        assert_eq!(
            s(vec![phone(), mac("connected"), office("connected")]).1,
            "3 devices"
        );
        assert_eq!(
            s(vec![phone(), mac("disconnected"), office("connected")]).1,
            "2 of 3 devices"
        );
        assert_eq!(
            s(vec![mac("connecting"), office("disconnected")]),
            ("Connecting…".into(), "2 devices".into(), Tube::March)
        );
        assert_eq!(
            s(vec![mac("disconnected"), office("disconnected")]),
            ("Not connected".into(), String::new(), Tube::Dashed)
        );
        assert_eq!(
            s(vec![mac("unauthorized"), office("pairing_lost")]).2,
            Tube::Broken
        );
        // One device uses its page status; the Devices row retains the name.
        assert_eq!(
            s(vec![phone()]),
            ("Connected".into(), "Local network".into(), Tube::Solid)
        );
        let mut paused = snapshot(vec![phone()]);
        paused.paused_devices = vec![id(1)];
        assert_eq!(summary(&peers(&paused, &[]), &[]).link.tube, Tube::Paused);
        // Sending runs the capsule outwards; the device row shows its share.
        let snap = snapshot(vec![phone(), mac("connected")]);
        let sending = [transfer(
            "a",
            1,
            "report.pdf",
            "sending",
            12_000_000,
            40_000_000,
        )];
        let peers = peers(&snap, &sending);
        let summary = summary(&peers, &sending);
        assert_eq!(
            (summary.detail.as_str(), summary.link.capsule),
            ("Sending report.pdf…", 1)
        );
        assert_eq!(peers[0].item().status, "Sending report.pdf · 12 of 40 MB");
    }

    #[test]
    fn cant_connect_shows_only_when_no_device_can_be_reached() {
        let with = |states: &[&str]| {
            let devices = states
                .iter()
                .enumerate()
                .map(|(i, state)| device(i as u8 + 1, "Device", i == 0, state, 0))
                .collect();
            unreachable(&peers(&snapshot(devices), &[]))
        };
        assert!(!with(&[]));
        assert!(with(&["disconnected"]));
        assert!(with(&["disconnected", "connecting"]));
        assert!(!with(&["disconnected", "connected"]));
        // A paused phone is still connected.
        let mut paused = snapshot(vec![device(1, "Phone", true, "connected", 0)]);
        paused.paused_devices = vec![id(1)];
        assert!(!unreachable(&peers(&paused, &[])));
        // Devices which answered need re-pairing or an update, not a network.
        assert!(!with(&["disconnected", "unauthorized"]));
        assert!(!with(&["update_required"]));
    }

    #[test]
    fn per_device_routes_and_receive_capsules_use_their_own_device() {
        let mut mac = device(2, "Mac", false, "connected", 0);
        mac.route = Some("Tailscale".into());
        let snap = snapshot(vec![device(1, "Phone", true, "connected", 0), mac]);
        let mut incoming = transfer("in", 2, "report.pdf", "sending", 5, 10);
        incoming.direction = Some("incoming".into());
        let peers = peers(&snap, &[incoming]);
        assert_eq!(peers[0].connection(), "Connected over local network");
        assert_eq!(peers[1].connection(), "Connected over Tailscale");
        assert!(!peers[0].receiving());
        assert!(peers[1].receiving());
        assert_eq!(device_summary(&peers[0]).0.link.capsule, 0);
        assert_eq!(device_summary(&peers[1]).0.link.capsule, 2);
        assert_eq!(peers[1].item().status, "Receiving report.pdf · 50%");
    }

    #[test]
    fn stack_shows_three_tiles_then_two_and_more() {
        let many: Vec<_> = (1..=5)
            .map(|n| device(n, "PC", false, "connected", 0))
            .collect();
        let (tiles, slots) = stack(&peers(&snapshot(many), &[]));
        assert_eq!(slots, 3);
        // Back to front: "+3" behind, the first device in front.
        assert_eq!(
            tiles.iter().map(|t| (t.slot, t.more)).collect::<Vec<_>>(),
            [(2, 3), (1, 0), (0, 0)]
        );
        let three: Vec<_> = (1..=3)
            .map(|n| device(n, "PC", false, "connected", 0))
            .collect();
        assert_eq!(stack(&peers(&snapshot(three), &[])).1, 3);
    }

    #[test]
    fn recent_keeps_unfinished_items_first_and_three_done() {
        let snap = snapshot(vec![
            device(1, "Galaxy S25+", true, "connected", 0),
            device(2, "MacBook Air", false, "disconnected", 0),
        ]);
        let peers = peers(&snap, &[]);
        let mut transfers = vec![
            transfer("done1", 1, "a.png", "sent", 1, 1),
            transfer("done2", 1, "b.pdf", "sent", 1, 1),
            transfer("done2", 2, "b.pdf", "sent", 1, 1),
            transfer("wait", 2, "report.pdf", "waiting_for_device", 0, 9),
            transfer("fail", 1, "budget.xlsx", "failed", 0, 9),
            transfer("done3", 1, "c.txt", "sent", 1, 1),
            transfer("done4", 1, "d.txt", "sent", 1, 1),
            transfer("gone", 1, "e.txt", "cancelled", 0, 1),
        ];
        transfers[4].error = Some("DiskFull".into());
        let items = recent(&peers, &transfers);
        let lines: Vec<_> = items
            .iter()
            .map(|i| (i.title.to_string(), i.detail.to_string()))
            .collect();
        assert_eq!(
            lines,
            [
                ("report.pdf".into(), "Waiting for MacBook Air".into()),
                ("budget.xlsx".into(), "Galaxy S25+'s disk is full".into()),
                ("a.png".into(), "To Galaxy S25+".into()),
                ("b.pdf".into(), "To 2 devices".into()),
                ("c.txt".into(), "To Galaxy S25+".into()),
            ]
        );
        assert!(items[1].can_retry && items[1].can_dismiss && items[1].detail_tone == Tone::Danger);
        assert!(items[2].picture);
        assert!(!items[2].video);
        assert!(!items[3].picture && !items[3].video);
    }

    #[test]
    fn recent_classifies_file_extensions() {
        let ps = peers(&snapshot(two()), &[]);
        for (extensions, picture, video) in [
            (
                &["png", "jpg", "jpeg", "gif", "webp", "heic", "bmp", "avif"][..],
                true,
                false,
            ),
            (
                &["mp4", "mov", "m4v", "webm", "mkv", "3gp", "avi"][..],
                false,
                true,
            ),
            (&["pdf", "txt", "mp4.bak", "png.bak"][..], false, false),
        ] {
            for ext in extensions {
                for name in [
                    format!("capture.{ext}"),
                    format!("CAPTURE.{}", ext.to_ascii_uppercase()),
                    format!("Capture.{}{}", ext[..1].to_ascii_uppercase(), &ext[1..]),
                ] {
                    for incoming in [false, true] {
                        let mut t = transfer("kind", 1, &name, "sent", 1, 1);
                        if incoming {
                            t.direction = Some("incoming".into());
                        }
                        let items = recent(&ps, &[t]);
                        assert_eq!(items.len(), 1);
                        assert_eq!(items[0].picture, picture, "{name}, incoming={incoming}");
                        assert_eq!(items[0].video, video, "{name}, incoming={incoming}");
                    }
                }
            }
        }
        for name in ["", "capture", "capture.", "mp4", "capturemp4"] {
            assert!(!is_picture(name), "{name}");
            assert!(!is_video(name), "{name}");
        }
    }

    #[test]
    fn delivery_failures_use_plain_words_in_recent_and_recipient_details() {
        let ps = peers(&snapshot(two()), &[]);
        for (reason, expected) in [
            (
                Some("UnsupportedKind"),
                "Update Pneu on MacBook Air to send this kind of file",
            ),
            (Some("disk_full"), "MacBook Air's disk is full"),
            (
                Some("NotWritable"),
                "MacBook Air can't save files right now",
            ),
            (
                Some("StorageFailed"),
                "MacBook Air can't save files right now",
            ),
            (
                Some("unrecognized failure"),
                "Something went wrong. Try again.",
            ),
            (None, "Something went wrong. Try again."),
        ] {
            let mut failed = transfer("operation", 2, "report.pdf", "failed", 0, 10);
            failed.error = reason.map(str::to_owned);
            let ts = [failed];
            assert_eq!(recent(&ps, &ts)[0].detail, expected);
            let details = recipient_details(&ps, &ts, &artifact_key(&ts[0]));
            assert_eq!(details[0].detail, format!("Failed · {expected}"));
        }
    }

    #[test]
    fn recent_artifacts_and_recipient_controls_keep_identity() {
        let ps = peers(&snapshot(two()), &[]);
        let mut ts = vec![
            transfer("operation", 1, "same.pdf", "sending", 2, 10),
            transfer("operation", 1, "same.pdf", "failed", 0, 10),
            transfer("operation", 2, "same.pdf", "waiting_for_device", 0, 10),
        ];
        ts[0].transfer_id = "first-artifact".into();
        ts[1].transfer_id = "second-artifact".into();
        ts[2].transfer_id = "first-artifact".into();
        let rows = recent(&ps, &ts);
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].id, rows[1].id);
        assert!(rows[0].detail.starts_with("Sending to 2 devices"));
        assert!(rows[1].can_retry && rows[1].can_dismiss); // active siblings are independent
        let details = recipient_details(&ps, &ts, &rows[0].id);
        assert_eq!(details.len(), 2);
        assert!(details.iter().all(|d| d.can_stop && !d.can_retry));
        let failed = recipient_details(&ps, &ts, &rows[1].id);
        assert!(failed[0].can_retry && !failed[0].can_stop);
        ts[0].state = "sent".into();
        ts[0].path = Some("/isolated/report.pdf".into());
        ts[2].state = "sent".into();
        assert!(
            recent(&ps, &ts)
                .iter()
                .any(|r| r.id == rows[0].id && r.can_open)
        );
        let mut duplicate = ts[0].clone();
        duplicate.device = ts[0].device.clone();
        assert_eq!(distinct_recipients(&[&ts[0], &duplicate]), 1);
    }

    #[test]
    fn admission_placeholders_never_become_fake_recipient_controls() {
        let ps = peers(&snapshot(two()), &[]);
        let mut staging = transfer("operation", 0, "2 files", "staging", 0, 0);
        staging.transfer_id = staging.operation.clone();
        let rows = recent(&ps, std::slice::from_ref(&staging));
        assert_eq!(rows[0].detail, "Preparing…");
        assert!(
            rows[0].batch && rows[0].can_cancel && !rows[0].can_details && rows[0].progress < 0.0
        );
        assert!(
            recipient_details(&ps, std::slice::from_ref(&staging), &artifact_key(&staging))
                .is_empty()
        );
        staging.state = "failed".into();
        staging.file_name = "one.pdf".into();
        staging.error = Some("File is too large".into());
        staging.path = Some("/one.pdf".into());
        let mut other = staging.clone();
        other.path = Some("/two.pdf".into());
        other.file_name = "two.pdf".into();
        let rows = recent(&ps, &[staging, other]);
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].id, rows[1].id);
        assert!(rows.iter().all(|r| r.batch
            && !r.can_details
            && !r.can_retry
            && r.can_dismiss
            && r.detail == "File is too large"));
    }

    #[test]
    fn pair_switches_are_labelled_from_this_side() {
        let snap = snapshot(vec![device(1, "Galaxy S25+", true, "disconnected", 0)]);
        let phone = &peers(&snap, &[])[0];
        let (me, it) = (id(9), id(1));
        let reply = json!({"can_sync": true, "settings": [
            {"key": format!("{it}/{me}/automatic_media"), "enabled": true, "pending": false},
            {"key": format!("{it}/{me}/clipboard"), "enabled": false, "pending": true},
            {"key": format!("{me}/{it}/clipboard"), "enabled": true, "pending": false},
        ]});
        let (items, note) = device_settings(phone, &reply, ClipboardMode::Automatic);
        assert!(note.is_empty());
        assert_eq!(
            items
                .iter()
                .map(|i| (i.label.as_str(), i.on, i.sub.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Send clipboard", true, ""),
                (
                    "Receive clipboard",
                    false,
                    "Applies when Galaxy S25+ connects"
                ),
                ("Receive automatic photos", true, ""),
            ]
        );
        let (items, note) = device_settings(
            phone,
            &json!({"can_sync": false, "settings": []}),
            ClipboardMode::Automatic,
        );
        assert!(items.is_empty());
        assert_eq!(note, "Switches sync when Galaxy S25+ connects.");
        let mut seen = phone.clone();
        seen.last_seen = 1;
        assert_eq!(
            device_settings(&seen, &json!({"can_sync": false}), ClipboardMode::Automatic).1,
            "Update Pneu on Galaxy S25+ to sync these switches."
        );
        let mut connected = phone.clone();
        connected.health = Health::Connected;
        let (items, _) = device_settings(&connected, &reply, ClipboardMode::Automatic);
        assert!(
            items.iter().all(|s| s.sub.is_empty()),
            "connected settings must not say waiting to connect"
        );
        assert_eq!(phone.connection(), "Not connected yet");
    }

    #[test]
    fn manual_image_limit_maps_to_window_banner_or_menu_notice_only() {
        assert_eq!(
            clipboard_send_error("clipboard image too large", false),
            Some("Couldn't send the clipboard. The image is over 20 MB")
        );
        assert_eq!(
            clipboard_send_error("clipboard image too large", true),
            None
        );
        for from_menu in [false, true] {
            assert_eq!(
                clipboard_send_error("clipboard unavailable", from_menu),
                Some("Couldn't send the clipboard")
            );
        }
    }
    #[test]
    fn share_off_disables_only_send_clipboard_without_changing_pair_settings() {
        let ps = peers(
            &snapshot(vec![device(1, "Galaxy S25+", true, "disconnected", 0)]),
            &[],
        );
        let reply = json!({"can_sync":true,"settings":[
            {"key":format!("{}/{}/clipboard", id(9), id(1)),"enabled":true,"pending":true},
            {"key":format!("{}/{}/clipboard", id(1), id(9)),"enabled":true,"pending":true}
        ]});
        for mode in [
            ClipboardMode::Automatic,
            ClipboardMode::Manual,
            ClipboardMode::Off,
        ] {
            let (items, _) = device_settings(&ps[0], &reply, mode);
            assert_eq!(items[0].enabled, mode != ClipboardMode::Off);
            assert_eq!(items[0].on, mode != ClipboardMode::Off);
            assert_eq!(
                items[0].sub,
                if mode == ClipboardMode::Off {
                    "Off for all devices in Settings"
                } else {
                    "Applies when Galaxy S25+ connects"
                }
            );
            assert!(items[1].enabled && items[1].on);
            assert_eq!(items[1].sub, "Applies when Galaxy S25+ connects");
        }
        assert_eq!(reply["settings"][0]["enabled"], true);
    }
    #[test]
    fn one_device_status_matches_device_page_for_every_health_and_progress_state() {
        for state in [
            "connected",
            "connecting",
            "disconnected",
            "unauthorized",
            "update_required",
        ] {
            for last in [0, crate::now_ms() - 60_000] {
                let snap = snapshot(vec![device(1, "Galaxy S25+", true, state, last)]);
                let ps = peers(&snap, &[]);
                let main = summary(&ps, &[]);
                let page = device_summary(&ps[0]).0;
                assert_eq!((main.title, main.detail), (page.title, page.detail));
                assert_eq!(ps[0].item().name, "Galaxy S25+");
            }
        }
        for incoming in [false, true] {
            let snap = snapshot(vec![device(1, "Galaxy S25+", true, "connected", 0)]);
            let mut t = transfer("s", 1, "report.pdf", "sending", 5, 10);
            if incoming {
                t.direction = Some("incoming".into());
            }
            let ps = peers(&snap, std::slice::from_ref(&t));
            assert_eq!(summary(&ps, &[t]).detail, device_summary(&ps[0]).0.detail);
        }
        let mut snap = snapshot(vec![device(1, "Galaxy S25+", true, "connected", 0)]);
        snap.paused_devices = vec![id(1)];
        let ps = peers(&snap, &[]);
        assert_eq!(summary(&ps, &[]).title, "Paused");
        assert_eq!(summary(&ps, &[]).detail, "Automatic sending is off");
    }
    #[test]
    fn recent_direction_always_names_one_device_and_limits_name_before_time() {
        for name in ["Galaxy S25+".to_owned(), "é".repeat(25)] {
            let ps = peers(&snapshot(vec![device(1, &name, true, "connected", 0)]), &[]);
            for incoming in [false, true] {
                let mut t = transfer("done", 1, "report.pdf", "sent", 10, 10);
                t.finished_at = Some(crate::now_ms() - 60_000);
                if incoming {
                    t.direction = Some("incoming".into());
                }
                let row = recent(&ps, std::slice::from_ref(&t)).remove(0);
                let short = if name.chars().count() > 24 {
                    format!("{}…", name.chars().take(23).collect::<String>())
                } else {
                    name.clone()
                };
                assert_eq!(
                    row.detail,
                    format!(
                        "{} {short} · {}",
                        if incoming { "From" } else { "To" },
                        super::super::last_seen(t.finished_at.unwrap())
                    )
                );
            }
        }
    }
    #[test]
    fn sizes_titles_and_modes() {
        assert_eq!(size_pair(12_000_000, 40_000_000), "12 of 40 MB");
        assert_eq!(size_pair(1_200_000, 2_400_000), "1.2 of 2.4 MB");
        assert_eq!(size_pair(320, 900), "320 of 900 B");
        assert_eq!(size_pair(1_200_000_000, 4_000_000_000), "1.2 of 4.0 GB");
        assert_eq!(
            files_title(&[PathBuf::from("/a/report.pdf")], "Send"),
            "Send report.pdf to"
        );
        assert_eq!(
            files_title(&[PathBuf::from("/a"), PathBuf::from("/b")], "Send"),
            "Send 2 files to"
        );
        assert_eq!(
            ClipboardMode::from_config(None, true),
            ClipboardMode::Automatic
        );
        assert_eq!(ClipboardMode::from_config(None, false), ClipboardMode::Off);
        assert_eq!(
            ClipboardMode::from_config(Some(&json!("manual")), true),
            ClipboardMode::Manual
        );
    }

    #[test]
    fn recent_open_rejects_redirected_paths_and_keeps_an_anchored_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let folder = dir.path().join("files");
        std::fs::create_dir(&folder)?;
        let path = folder.join("report.pdf");
        std::fs::write(&path, b"original")?;
        let file = recent_file(&path)?;
        std::fs::rename(&path, folder.join("original.pdf"))?;
        std::os::unix::fs::symlink("original.pdf", &path)?;
        assert!(recent_file(&path).is_err());
        assert_eq!(file.read_bounded(100)?, b"original");
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&folder, &linked)?;
        assert!(recent_file(&linked.join("original.pdf")).is_err());
        assert!(recent_file(&folder).is_err());
        assert!(recent_file(Path::new("relative.pdf")).is_err());
        Ok(())
    }

    #[test]
    fn recent_open_accepts_send_parent_paths_without_applying_outbox_limits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("child"))?;
        let actual = dir.path().join("large.bin");
        let file = std::fs::File::create(&actual)?;
        file.set_len(crate::outbox::PEER_BYTES + 1)?; // sparse, no large allocation
        let selected = dir.path().join("child/../large.bin");
        assert!(crate::ipc::send_command(&[selected.to_string_lossy().into_owned()]).is_err());
        assert_eq!(
            recent_file(&selected)?
                .read_bounded(0)
                .unwrap_err()
                .to_string(),
            "input exceeds bounded read limit"
        );
        std::os::unix::fs::symlink(dir.path().join("child"), dir.path().join("linked"))?;
        assert!(recent_file(&dir.path().join("linked/../large.bin")).is_err());
        std::fs::remove_file(actual)?;
        std::os::unix::fs::symlink("child", dir.path().join("large.bin"))?;
        assert!(recent_file(&selected).is_err());
        Ok(())
    }

    type UiEvents = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;
    static UI_EVENTS: std::sync::OnceLock<UiEvents> = std::sync::OnceLock::new();
    struct TestProxy(UiEvents);
    impl slint::platform::EventLoopProxy for TestProxy {
        fn quit_event_loop(&self) -> std::result::Result<(), slint::EventLoopError> {
            Ok(())
        }
        fn invoke_from_event_loop(
            &self,
            event: Box<dyn FnOnce() + Send>,
        ) -> std::result::Result<(), slint::EventLoopError> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }
    fn flush_ui() {
        loop {
            let events = std::mem::take(&mut *UI_EVENTS.get().unwrap().lock().unwrap());
            if events.is_empty() {
                break;
            }
            for event in events {
                event();
            }
        }
        slint::platform::update_timers_and_animations();
    }
    // What the window last copied; tests never reach the system clipboard.
    static COPIED: Mutex<String> = Mutex::new(String::new());
    struct Headless(std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>);
    impl slint::platform::Platform for Headless {
        fn set_clipboard_text(&self, text: &str, _: slint::platform::Clipboard) {
            *COPIED.lock().unwrap() = text.into();
        }
        fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
            Some(Box::new(TestProxy(
                UI_EVENTS.get_or_init(Default::default).clone(),
            )))
        }
        fn create_window_adapter(
            &self,
        ) -> std::result::Result<
            std::rc::Rc<dyn slint::platform::WindowAdapter>,
            slint::PlatformError,
        > {
            Ok(self.0.clone())
        }
    }
    // The native palette initialises Qt even with a software window. Isolate
    // Qt's offscreen setting from the user's desktop and parallel tests.
    async fn in_offscreen_child(test: &str, env: &[(&str, &std::ffi::OsStr)]) -> Result<bool> {
        if std::env::var_os("PNEU_WINDOW_TEST_CHILD").is_some() {
            return Ok(false);
        }
        let private_runtime = tempfile::tempdir()?;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .args(["--exact", test, "--nocapture"])
            .env("PNEU_WINDOW_TEST_CHILD", "1")
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("QT_ACCESSIBILITY", "0")
            .env("NO_AT_BRIDGE", "1")
            .env("XDG_RUNTIME_DIR", private_runtime.path())
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!(
                    "unix:path={}/unavailable-test-bus",
                    private_runtime.path().display()
                ),
            )
            .env_remove("AT_SPI_BUS_ADDRESS")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        let status = tokio::time::timeout(Duration::from_secs(60), command.status()).await??;
        anyhow::ensure!(status.success(), "offscreen window test {test} failed");
        Ok(true)
    }
    fn headless() -> Result<std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>>
    {
        let window = slint::platform::software_renderer::MinimalSoftwareWindow::new(
            slint::platform::software_renderer::RepaintBufferType::NewBuffer,
        );
        slint::platform::set_platform(Box::new(Headless(window.clone())))?;
        Ok(window)
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn headless_native_refresh_and_activation_do_not_enter_appkit() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::headless_native_refresh_and_activation_do_not_enter_appkit",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        assert!(!window.window().is_visible());
        super::super::macos::native::activate(&window);
        super::super::macos::native::update(super::super::Snapshot::default(), Config::default());
        assert!(!super::super::macos::native::reduced_motion());
        assert!(!window.window().is_visible());
        let picker = super::super::macos::native::choose("Headless test", false, None);
        tokio::pin!(picker);
        assert!(futures_util::poll!(picker.as_mut()).is_pending());
        flush_ui();
        assert!(picker.await.is_err());
        super::super::macos::native::cleanup();
        Ok(())
    }

    struct MockIpc {
        requests: Arc<Mutex<Vec<Value>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for MockIpc {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    fn mock_ipc(
        path: &Path,
        handler: impl FnMut(&Value) -> (bool, Value) + Send + 'static,
    ) -> Result<MockIpc> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let listener = tokio::net::UnixListener::bind(path)?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let mut handler = handler;
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let mut stream = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                if stream.read_line(&mut line).await.is_err() {
                    continue;
                }
                let Ok(command) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                log.lock().unwrap().push(command.clone());
                let (ok, value) = handler(&command);
                if value == json!({"mock_disconnect":true}) {
                    continue;
                }
                let mut reply = serde_json::to_vec(&json!({"ok":ok,"value":value})).unwrap();
                reply.push(b'\n');
                let _ = stream.get_mut().write_all(&reply).await;
            }
        });
        Ok(MockIpc { requests, task })
    }
    fn model_worker(
        window: &PneuWindow,
        dir: &Path,
    ) -> (Worker, tokio::sync::mpsc::UnboundedReceiver<Action>) {
        let view = Arc::new(Mutex::new(View::default()));
        let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
        connect(window, &commands, &view);
        let paths = Paths {
            data: dir.join("data"),
            cache: dir.join("cache"),
            config: dir.join("config.toml"),
            socket: dir.join("pneud.sock"),
        };
        let mut worker = Worker::new(paths, window.as_weak(), view, 0);
        state(&mut worker, two(), vec![]);
        (worker, rx)
    }
    // Production runs the UI event loop alongside the IPC worker. Headless
    // tests pump that loop while an action awaits a navigation acknowledgement.
    async fn act_with_ui(worker: &mut Worker, action: Action) {
        tokio::select! {
            _ = worker.act(action) => {},
            _ = async {
                loop {
                    flush_ui();
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            } => unreachable!(),
        }
        flush_ui();
    }

    #[tokio::test]
    async fn open_folder_controls_follow_save_setting_and_recent_visibility() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::open_folder_controls_follow_save_setting_and_recent_visibility",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (_worker, mut actions) = model_worker(&window, dir.path());
        let ui = window.global::<Ui>();
        window.show()?;
        let mut recent_height = None;
        for page in [Page::Main, Page::Settings] {
            for save in [false, true] {
                for has_recent in [false, true] {
                    ui.set_page(page);
                    ui.set_save_to_folder(save);
                    ui.set_recent(ModelRc::new(VecModel::from(if has_recent {
                        vec![RecentItem {
                            title: "report.pdf".into(),
                            ..Default::default()
                        }]
                    } else {
                        Vec::new()
                    })));
                    let height = window.get_content_height();
                    if page == Page::Main && has_recent {
                        if let Some(previous) = recent_height {
                            assert_eq!(
                                height, previous,
                                "the link must not enlarge the Recent header"
                            );
                        }
                        recent_height = Some(height);
                    }
                    surface.set_size(slint::LogicalSize::new(400.0, height).to_physical(1.0));
                    let mut found = false;
                    // Exercise the actual hit targets, including hidden and
                    // disabled controls, rather than duplicating the Slint rule.
                    for y in (0..height as usize).step_by(4) {
                        ui.set_page(page);
                        ui.set_dialog(Dialog::None);
                        let position = slint::LogicalPosition::new(
                            if page == Page::Main { 330.0 } else { 235.0 },
                            y as f32,
                        );
                        let button = slint::platform::PointerEventButton::Left;
                        window.window().dispatch_event(
                            slint::platform::WindowEvent::PointerPressed { position, button },
                        );
                        window.window().dispatch_event(
                            slint::platform::WindowEvent::PointerReleased { position, button },
                        );
                        while let Ok(action) = actions.try_recv() {
                            if matches!(action, Action::Scoped(_, action) if matches!(*action, Action::OpenFolder))
                            {
                                found = true;
                            }
                        }
                    }
                    assert_eq!(
                        found,
                        page == Page::Settings || (save && has_recent),
                        "page {page:?}, save {save}, Recent {has_recent}"
                    );
                }
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn open_folder_uses_daemon_command_and_reports_full_path_without_blocking_window()
    -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::open_folder_uses_daemon_command_and_reports_full_path_without_blocking_window",
            &[],
        ).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let folder = dir
            .path()
            .join("A long received-files folder with spaces and 日本語");
        worker.status["config"]["save_path"] = json!(folder);
        worker.status["config"]["save_to_folder"] = json!(false);
        let mock = mock_ipc(&worker.paths.socket, |command| {
            assert_eq!(command, &json!({"command":"open_folder"}));
            (false, json!("opener unavailable"))
        })?;
        window.global::<Ui>().invoke_open_folder();
        worker.act(actions.try_recv()?).await;
        assert!(worker.open_folder_job.is_some());
        // Repeated clicks coalesce; other window actions remain available.
        worker.act(Action::OpenFolder).await;
        worker.act(Action::Focused).await;
        let result = job_result(&mut worker.open_folder_job).await;
        worker.opened_folder(result);
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        let config: Config = serde_json::from_value(worker.status["config"].clone())?;
        let frame = worker.frame(&config, &[], None, None, false, crate::now_ms());
        assert_eq!(frame.folder.chars().count(), 26);
        apply(&window, frame);
        assert_eq!(
            window.global::<Ui>().get_error().as_str(),
            format!("Couldn't open {}", super::super::display_path(&folder))
        );
        worker.act(Action::Navigated).await;
        assert!(
            worker.action_error.is_empty(),
            "normal navigation clears the banner"
        );
        worker.act(Action::OpenFolder).await;
        let result = job_result(&mut worker.open_folder_job).await;
        worker.view.lock().unwrap().page = Page::Settings;
        worker.opened_folder(result);
        assert!(
            worker.action_error.is_empty(),
            "late failures stay on the originating view"
        );
        worker.act(Action::OpenFolder).await;
        worker.close(&AtomicBool::new(true)).await;
        assert!(worker.open_folder_job.is_none());
        assert!(worker.open_folder_origin.is_none());
        Ok(())
    }

    fn status_value(worker: &Worker) -> Value {
        json!({"snapshot":worker.snapshot, "config":Config::default(), "window_request":0, "invitation":null})
    }
    #[tokio::test]
    async fn first_run_scan_survives_refresh_and_requires_confirmation() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::first_run_scan_survives_refresh_and_requires_confirmation",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        for confirm in [false, true] {
            let window = PneuWindow::new()?;
            let dir = tempfile::tempdir()?;
            let (mut worker, mut actions) = model_worker(&window, dir.path());
            state(&mut worker, vec![], vec![]);
            let mock = mock_ipc(&worker.paths.socket, |command| {
                let reply = match command["command"].as_str() {
                    Some("join") => {
                        json!({"state":"confirmation", "attempt":"first-scan", "name":"Other Mac"})
                    }
                    Some("join_confirm_scoped") => json!({"state":"pairing"}),
                    Some("join_status") => json!({"state":"paired", "device":id(2)}),
                    _ => Value::Null,
                };
                (true, reply)
            })?;
            let visible = AtomicBool::new(true);
            // The initial empty status opens Add Device without a back header.
            worker.refresh(status_value(&worker), &visible).await?;
            flush_ui();
            assert_eq!(worker.view().page, Page::Add);
            while let Ok(action) = actions.try_recv() {
                act_with_ui(&mut worker, action).await;
            }
            window.invoke_open_add(1);
            while let Ok(action) = actions.try_recv() {
                act_with_ui(&mut worker, action).await;
            }
            // Exercise navigation only: no native camera is installed or opened.
            window.invoke_go(Page::Scan);
            while let Ok(action) = actions.try_recv() {
                act_with_ui(&mut worker, action).await;
            }
            let scan_view = worker.view();
            for scan_state in [
                ScanState::Requesting,
                ScanState::Scanning,
                ScanState::Denied,
            ] {
                window.global::<Ui>().set_scan_state(scan_state);
                for _ in 0..3 {
                    worker.refresh(status_value(&worker), &visible).await?;
                    flush_ui();
                    assert!(window.global::<Ui>().get_first_run());
                    assert_eq!(worker.view(), scan_view);
                    assert_eq!(window.global::<Ui>().get_scan_state(), scan_state);
                    assert_eq!(window.global::<Ui>().get_dialog(), Dialog::None);
                }
            }
            scanned_pairing(&window, sample_uri());
            while let Ok(action) = actions.try_recv() {
                act_with_ui(&mut worker, action).await;
            }
            worker.refresh(status_value(&worker), &visible).await?;
            flush_ui();
            assert_eq!(worker.view().page, Page::Add);
            assert_eq!(worker.view().add_tab, 1);
            assert_eq!(window.global::<Ui>().get_dialog(), Dialog::Message);
            let message = window.global::<Ui>().get_message();
            assert_eq!(message.title, "Pair with Other Mac?");
            assert_eq!(message.secondary, "Cancel");
            assert_eq!(message.primary, "Pair");
            assert!(!worker.join.as_ref().unwrap().confirmed);
            assert!(
                !mock
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r["command"] == "join_confirm_scoped")
            );
            // Match the confirmation buttons: dismiss the popup and submit its choice.
            window.global::<Ui>().set_dialog(Dialog::None);
            window
                .global::<Ui>()
                .invoke_message_action(message.action, confirm);
            while let Ok(action) = actions.try_recv() {
                act_with_ui(&mut worker, action).await;
            }
            if confirm {
                assert!(worker.join.as_ref().unwrap().confirmed);
                state(
                    &mut worker,
                    vec![device(2, "Other Mac", false, "connected", 0)],
                    vec![],
                );
                worker.refresh(status_value(&worker), &visible).await?;
                flush_ui();
                assert_eq!(window.global::<Ui>().get_paired(), "Other Mac");
                assert!(!window.global::<Ui>().get_first_run());
                assert_eq!(
                    worker.paired.as_ref().unwrap().device.as_deref(),
                    Some(id(2).as_str())
                );
                worker.paired.as_mut().unwrap().at = Instant::now() - Duration::from_millis(5100);
                worker.refresh(status_value(&worker), &visible).await?;
                flush_ui();
                while let Ok(action) = actions.try_recv() {
                    act_with_ui(&mut worker, action).await;
                }
                worker.refresh(status_value(&worker), &visible).await?;
                flush_ui();
                assert_eq!(worker.view().page, Page::Device);
                assert_eq!(worker.view().device, id(2).as_str());
                assert!(worker.join.is_none());
                assert!(worker.paired.is_none());
                assert!(!worker.journal_path().exists());
            } else {
                worker.refresh(status_value(&worker), &visible).await?;
                flush_ui();
                assert_eq!(worker.view().page, Page::Add);
                assert_eq!(worker.view().add_tab, 1);
                assert_eq!(window.global::<Ui>().get_dialog(), Dialog::None);
                assert!(worker.join.is_none());
                assert!(worker.paired.is_none());
                assert!(!worker.journal_path().exists());
            }
            let requests = mock.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r["command"] == "join_confirm_scoped")
                    .count(),
                usize::from(confirm)
            );
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r["command"] == "join_cancel")
                    .count(),
                usize::from(!confirm)
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn scan_preserves_pairing_baseline_until_leaving_pairing_pages() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::scan_preserves_pairing_baseline_until_leaving_pairing_pages",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        state(
            &mut worker,
            vec![device(1, "Phone", true, "pairing_lost", 0)],
            vec![],
        );
        window.invoke_open_add(1);
        while let Ok(action) = actions.try_recv() {
            worker.act(action).await;
        }
        let token = pneu_proto::PairingUri::parse(&sample_uri())?.token;
        worker.code.token = Some(token.clone());
        assert!(worker.probed);
        // A peer appears while Scan is open. Returning to Add must not absorb
        // that peer into the baseline and hide the pairing-success transition.
        window.invoke_go(Page::Scan);
        while let Ok(action) = actions.try_recv() {
            worker.act(action).await;
        }
        state(
            &mut worker,
            vec![
                device(1, "Phone", true, "connected", 0),
                device(2, "Other Mac", false, "connected", 0),
            ],
            vec![],
        );
        window.invoke_back();
        while let Ok(action) = actions.try_recv() {
            worker.act(action).await;
        }
        assert_eq!(worker.view().page, Page::Add);
        assert_eq!(worker.view().add_tab, 1);
        worker.snapshot.pairing_completion = Some(super::super::PairingCompletion {
            token: token.clone(),
            device: id(2),
        });
        assert!(worker.probed);
        worker
            .refresh(status_value(&worker), &AtomicBool::new(true))
            .await?;
        assert!(worker.paired.is_some());
        // A fresh entry, including direct entry to Scan, resets all baselines.
        window.invoke_go(Page::Main);
        while let Ok(action) = actions.try_recv() {
            worker.act(action).await;
        }
        window.invoke_go(Page::Scan);
        while let Ok(action) = actions.try_recv() {
            worker.act(action).await;
        }
        assert_eq!(worker.seen_pairing_token, Some(token));
        assert!(worker.paired.is_none());
        assert!(!worker.probed);
        // Newly paired peers are also recognized while Scan itself is open.
        let next_token = pneu_proto::Hash::new("bc".repeat(32))?;
        worker.code.token = Some(next_token.clone());
        worker.snapshot.pairing_completion = Some(super::super::PairingCompletion {
            token: next_token,
            device: id(3),
        });
        worker
            .snapshot
            .devices
            .push(device(3, "Office PC", false, "connected", 0));
        worker
            .refresh(status_value(&worker), &AtomicBool::new(true))
            .await?;
        flush_ui();
        assert_eq!(
            worker.paired.as_ref().unwrap().device.as_deref(),
            Some(id(3).as_str())
        );
        assert_eq!(window.global::<Ui>().get_paired(), "Office PC");
        worker.paired.as_mut().unwrap().at = Instant::now() - Duration::from_millis(5100);
        worker
            .refresh(status_value(&worker), &AtomicBool::new(true))
            .await?;
        flush_ui();
        assert_eq!(worker.view().page, Page::Device);
        assert_eq!(worker.view().device, id(3).as_str());
        Ok(())
    }

    // Model Core's single join slot, so a missing preview cancellation rejects
    // the explicit link just as it does in production.
    fn mock_join_slot(path: &Path, clipboard: String) -> Result<MockIpc> {
        let mut active = None;
        mock_ipc(path, move |command| match command["command"].as_str() {
            Some("join") => {
                if active.is_some() {
                    return (false, json!("pairing in progress"));
                }
                let (attempt, name) = if command["uri"] == clipboard {
                    ("clipboard-preview", "Clipboard Mac")
                } else {
                    ("scan-preview", "Scanned Mac")
                };
                active = Some(attempt);
                (
                    true,
                    json!({"state":"confirmation", "attempt":attempt, "name":name}),
                )
            }
            Some("join_cancel") => {
                assert_eq!(command["attempt"], active.unwrap());
                active = None;
                (true, Value::Null)
            }
            other => panic!("unexpected pairing command: {other:?}"),
        })
    }

    async fn check_clipboard_handoff(preview: bool, scan: bool) -> Result<()> {
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let mut clipboard = pneu_proto::PairingUri::parse(&sample_uri())?;
        clipboard.name = "Clipboard Mac".into();
        clipboard.token = pneu_proto::Hash::new("ef".repeat(32))?;
        let clipboard = clipboard.build()?;
        let mock = mock_join_slot(&worker.paths.socket, clipboard.clone())?;
        window.invoke_open_add(1);
        while let Ok(action) = actions.try_recv() {
            act_with_ui(&mut worker, action).await;
        }
        worker.snapshot.pairing_clipboard = Some(clipboard);
        worker.snapshot.pairing_clipboard_revision = 1;
        let visible = AtomicBool::new(true);
        if preview {
            worker.refresh(status_value(&worker), &visible).await?;
            flush_ui();
            assert_eq!(window.global::<Ui>().get_offer(), "Clipboard Mac");
            assert!(worker.join.as_ref().unwrap().offer);
        }
        if scan {
            // Retain an offer across navigation as can happen during restart
            // recovery. The scan action itself must cancel this preview.
            worker.restored_join = preview;
            window.invoke_go(Page::Scan);
            while let Ok(action) = actions.try_recv() {
                act_with_ui(&mut worker, action).await;
            }
            for _ in 0..4 {
                worker.refresh(status_value(&worker), &visible).await?;
                flush_ui();
            }
            assert_eq!(worker.join.is_some(), preview);
            window
                .global::<Ui>()
                .on_probed(|_| panic!("scan handoff must not probe the clipboard"));
            scanned_pairing(&window, sample_uri());
            // Admission queues exactly one action, and leaves Scan active until
            // that action can navigate and Join without a worker refresh.
            assert_eq!(worker.view().page, Page::Scan);
        } else {
            window.global::<Ui>().invoke_paste_link(sample_uri().into());
        }
        for _ in 0..4 {
            worker.refresh(status_value(&worker), &visible).await?;
            flush_ui();
        }
        let action = actions.try_recv()?;
        assert!(matches!(&action, Action::Scoped(_, action) if matches!(
            (&**action, scan), (Action::ScannedLink(_), true) | (Action::PasteLink(_), false)
        )));
        assert!(actions.try_recv().is_err());
        act_with_ui(&mut worker, action).await;
        // Match the real worker: refresh after every action, including the
        // navigation notification generated inside the atomic scan action.
        loop {
            for _ in 0..4 {
                worker.refresh(status_value(&worker), &visible).await?;
                flush_ui();
                assert_eq!(worker.join.as_ref().unwrap().attempt, "scan-preview");
                assert_eq!(worker.view().page, Page::Add);
                assert_eq!(worker.view().add_tab, 1);
                assert_eq!(window.global::<Ui>().get_dialog(), Dialog::Message);
                assert_eq!(
                    window.global::<Ui>().get_message().title,
                    "Pair with Scanned Mac?"
                );
                assert!(window.global::<Ui>().get_offer().is_empty());
                assert!(!worker.join.as_ref().unwrap().confirmed);
            }
            let Ok(action) = actions.try_recv() else {
                break;
            };
            assert!(
                matches!(&action, Action::Scoped(_, action) if matches!(**action, Action::Navigated))
            );
            act_with_ui(&mut worker, action).await;
        }
        let commands = mock
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["command"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            commands,
            if preview {
                vec!["join", "join_cancel", "join"]
            } else {
                vec!["join"]
            }
        );
        worker.release().await;
        assert!(!worker.journal_path().exists());
        Ok(())
    }

    #[tokio::test]
    async fn scan_wins_over_clipboard_across_refreshes() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::scan_wins_over_clipboard_across_refreshes",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        check_clipboard_handoff(false, true).await
    }

    #[tokio::test]
    async fn scan_cancels_existing_clipboard_preview_across_refreshes() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::scan_cancels_existing_clipboard_preview_across_refreshes",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        check_clipboard_handoff(true, true).await
    }

    #[tokio::test]
    async fn explicit_paste_supersedes_clipboard_preview() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::explicit_paste_supersedes_clipboard_preview",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        check_clipboard_handoff(true, false).await
    }

    #[tokio::test]
    async fn scanned_links_use_paste_validation_confirmation_and_navigation_fences() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::scanned_links_use_paste_validation_confirmation_and_navigation_fences",
            &[],
        ).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut rx) = model_worker(&window, dir.path());
        let response = Arc::new(Mutex::new("confirmation"));
        let reply_state = response.clone();
        let mock = mock_ipc(&worker.paths.socket, move |_| {
            (
                true,
                json!({"state":*reply_state.lock().unwrap(), "attempt":"scan-preview", "name":"Other Mac"}),
            )
        })?;
        for state in [
            "confirmation",
            "own_code",
            "expired",
            "already_paired",
            "garbled",
        ] {
            *response.lock().unwrap() = state;
            window.invoke_go(Page::Scan);
            while let Ok(action) = rx.try_recv() {
                act_with_ui(&mut worker, action).await;
            }
            scanned_pairing(&window, sample_uri());
            assert_eq!(window.global::<Ui>().get_page(), Page::Scan);
            while let Ok(action) = rx.try_recv() {
                act_with_ui(&mut worker, action).await;
            }
            flush_ui();
            assert_eq!(window.global::<Ui>().get_dialog(), Dialog::Message);
            let message = window.global::<Ui>().get_message();
            match state {
                "confirmation" => {
                    assert_eq!(message.title, "Pair with Other Mac?");
                    assert_eq!(message.secondary, "Cancel");
                    assert_eq!(message.primary, "Pair");
                    assert!(!worker.join.as_ref().unwrap().confirmed);
                    window
                        .global::<Ui>()
                        .invoke_message_action(message.action, false);
                    while let Ok(action) = rx.try_recv() {
                        act_with_ui(&mut worker, action).await;
                    }
                    assert!(worker.join.is_none());
                }
                "own_code" => assert!(message.title.ends_with("own code")),
                "expired" => assert_eq!(message.title, "This code expired"),
                "already_paired" => assert_eq!(message.title, "Already paired with Other Mac"),
                "garbled" => assert_eq!(message.title, "This pairing link is incomplete"),
                _ => unreachable!(),
            }
        }
        let before = mock.requests.lock().unwrap().len();
        window.invoke_go(Page::Scan);
        while let Ok(action) = rx.try_recv() {
            act_with_ui(&mut worker, action).await;
        }
        scanned_pairing(&window, sample_uri());
        // A newer user choice also fences out an already queued decoded link.
        window.invoke_go(Page::Settings);
        scanned_pairing(&window, sample_uri());
        while let Ok(action) = rx.try_recv() {
            act_with_ui(&mut worker, action).await;
        }
        assert_eq!(worker.view().page, Page::Settings);
        assert_eq!(mock.requests.lock().unwrap().len(), before);
        let requests = mock.requests.lock().unwrap();
        assert_eq!(
            requests.iter().filter(|r| r["command"] == "join").count(),
            5
        );
        assert!(
            !requests
                .iter()
                .any(|r| r["command"] == "join_confirm_scoped")
        );
        Ok(())
    }
    #[tokio::test]
    async fn status_failures_keep_the_window_alive_and_clear_after_recovery() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::status_failures_keep_the_window_alive_and_clear_after_recovery",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _rx) = model_worker(&window, dir.path());
        let visible = AtomicBool::new(true);
        let status = status_value(&worker);
        for message in [
            "IPC frame too large",
            "IPC EOF",
            "connection refused",
            "timeout",
        ] {
            worker.status_failed(anyhow::anyhow!(message));
            flush_ui();
            assert_eq!(
                window.global::<Ui>().get_error(),
                "Couldn't update Pneu. Retrying…"
            );
            assert!(visible.load(Ordering::SeqCst));
            assert!(!worker.snapshot.devices.is_empty());
        }
        assert!(worker.refresh(Value::Null, &visible).await.is_err());
        assert!(!worker.status_error.is_empty());
        worker.refresh(status, &visible).await?;
        flush_ui();
        assert!(worker.status_error.is_empty());
        assert!(window.global::<Ui>().get_error().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn recent_received_open_reports_deleted_path_from_snapshot() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::recent_received_open_reports_deleted_path_from_snapshot",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _rx) = model_worker(&window, dir.path());
        let parent = dir.path().join("received");
        std::fs::create_dir(&parent)?;
        let path = parent.join("report.pdf");
        std::fs::write(&path, b"received")?;
        let mut row = transfer(id(9).as_str(), 1, "report.pdf", "sent", 8, 8);
        row.direction = Some("incoming".into());
        row.path = Some(path.clone());
        // Exercise the actual IPC snapshot -> UI adapter, not just the helper.
        let mut value = serde_json::to_value(&worker.snapshot)?;
        value["transfers"] = json!([{"operation":row.operation,"transfer_id":row.transfer_id,"device":row.device,"file_name":row.file_name,"state":row.state,"size":row.size,"sent":row.sent,"direction":row.direction,"path":row.path}]);
        let snapshot: crate::ui::Snapshot = serde_json::from_value(value)?;
        worker.transfers = snapshot.transfers.iter().map(Transfer::from).collect();
        std::fs::rename(&parent, dir.path().join("old-parent"))?;
        std::fs::create_dir(&parent)?;
        worker.act(Action::Open(artifact_key(&row))).await;
        let error = job_result(&mut worker.open_job).await.unwrap_err();
        assert!(error.is::<crate::actions::FileUnavailable>());
        Ok(())
    }

    fn target(n: u8, generation: u64, would_queue: bool) -> Value {
        json!({"device":id(n), "name":"PC", "eligible":true, "generation":generation, "fingerprint":"ab".repeat(32), "would_queue":would_queue})
    }
    fn advance_ui() {
        flush_ui();
        std::thread::sleep(Duration::from_millis(5));
        flush_ui();
    }
    fn key(window: &PneuWindow, key: slint::platform::Key) {
        let text: SharedString = key.into();
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
        flush_ui();
    }

    #[tokio::test]
    async fn queued_window_sends_keep_the_displayed_pairing_before_picker_and_clipboard_read()
    -> Result<()> {
        if in_offscreen_child("ui::window::tests::queued_window_sends_keep_the_displayed_pairing_before_picker_and_clipboard_read", &[]).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut rx) = model_worker(&window, dir.path());
        worker.view.lock().unwrap().targets =
            Some(vec![serde_json::from_value(target(1, 7, false))?]);
        window
            .global::<Ui>()
            .invoke_send_files(id(1).as_str().into());
        window.global::<Ui>().invoke_send_clipboard();
        // A poll can finish before the queued callback is processed. The UI
        // still displays generation 7 until its replacement frame is applied.
        worker.status["snapshot"]["send_targets"] = json!([target(1, 99, false)]);
        worker.act(rx.recv().await.unwrap()).await;
        let chooser = worker.chooser.take().unwrap();
        // Drop the unpolled portal future; inspect its frozen bindings through
        // the same originating view used to construct it, without a real picker.
        drop(chooser);
        assert_eq!(worker.captured_targets(id(1).as_str())[0].generation, 7);
        let mock = mock_ipc(&worker.paths.socket, |_| (true, json!({})))?;
        worker.act(rx.recv().await.unwrap()).await;
        assert_eq!(
            mock.requests.lock().unwrap()[0]["targets"][0]["generation"],
            7
        );
        // Even after the replacement is displayed, a queued callback retains
        // its original authority for the daemon to reject.
        let obsolete = worker.view();
        worker.view.lock().unwrap().targets =
            Some(vec![serde_json::from_value(target(1, 99, false))?]);
        worker
            .act(Action::Scoped(obsolete, Box::new(Action::SendClipboard)))
            .await;
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
        assert_eq!(
            mock.requests.lock().unwrap()[1]["targets"][0]["generation"],
            7
        );
        Ok(())
    }

    #[tokio::test]
    async fn picker_freezes_authority_and_uses_admission_queue_state() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::picker_freezes_authority_and_uses_admission_queue_state",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _) = model_worker(&window, dir.path());
        worker.status["config"]["choose_devices_each_time"] = json!(true);
        worker.status["snapshot"]["send_targets"] = json!([target(1, 7, false), target(2, 8, true), {"device":id(3),"fingerprint":null,"eligible":false}]);
        let captured = worker.captured_targets("");
        assert_eq!(captured.len(), 2);
        let origin = worker.view();
        worker
            .chosen(Ok((
                String::new(),
                captured,
                origin,
                Some(vec![dir.path().join("report.pdf")]),
            )))
            .await;
        flush_ui();
        let picks = window.global::<Ui>().get_picks();
        assert!(picks.row_data(0).unwrap().checked);
        let queued = picks.row_data(1).unwrap();
        assert!(!queued.connected && !queued.checked && queued.eligible);
        // Re-pair the same ID while the dialog is open. The submitted command
        // MUST still contain generation 7, never the newly observed generation.
        worker.status["snapshot"]["send_targets"] =
            json!([target(1, 99, false), target(2, 8, false)]);
        let mock = mock_ipc(&worker.paths.socket, |_| (true, json!({"operation":id(9)})))?;
        worker
            .act(Action::PickerSend(vec![id(1).as_str().into()]))
            .await;
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["targets"][0]["generation"], 7);
        assert_eq!(requests[0]["targets"][0]["fingerprint"], "ab".repeat(32));
        assert!(
            requests[0]["request_id"]
                .as_str()
                .is_some_and(|id| id.len() == 32)
        );
        drop(requests);
        worker.status["config"]["choose_devices_each_time"] = json!(false);
        worker.status["snapshot"]["send_targets"] =
            json!([target(1, 99, false), target(2, 8, true)]);
        assert_eq!(worker.captured_targets("").len(), 1);
        assert_eq!(worker.captured_targets(id(2).as_str()).len(), 1); // explicit offline selection
        worker.status["config"]["queue_offline"] = json!(true);
        assert_eq!(worker.captured_targets("").len(), 2);
        assert_eq!(worker.captured_targets(id(2).as_str()).len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn desktop_handoff_and_clipboard_reuse_picker_and_keep_captured_authority() -> Result<()>
    {
        if in_offscreen_child("ui::window::tests::desktop_handoff_and_clipboard_reuse_picker_and_keep_captured_authority", &[]).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _rx) = model_worker(&window, dir.path());
        worker.status["config"]["choose_devices_each_time"] = json!(true);
        worker.status["snapshot"]["send_targets"] =
            json!([target(1, 7, false), target(2, 8, false)]);
        let captured = |n, generation| crate::outbox::Authority {
            device: id(n),
            generation,
            fingerprint: pneu_proto::Hash::new("ab".repeat(32)).unwrap(),
        };
        let request = SendRequest {
            id: "desktop-request".into(),
            paths: Some(vec![dir.path().join("report.pdf")]),
            targets: vec![captured(1, 7), captured(2, 8)],
            clipboard: false,
            targeted: false,
            error: None,
        };
        let mock = mock_ipc(&worker.paths.socket, |_| (true, json!({"operation":id(9)})))?;
        worker.receive_send(request.clone()).await;
        flush_ui();
        assert_eq!(window.global::<Ui>().get_dialog(), Dialog::Picker);
        assert_eq!(window.global::<Ui>().get_picker_count(), 2);
        assert!(
            mock.requests.lock().unwrap().is_empty(),
            "no admission before selection"
        );
        worker.status["snapshot"]["send_targets"] =
            json!([target(1, 99, false), target(2, 99, false)]);
        worker
            .act(Action::PickerSend(vec![id(2).as_str().into()]))
            .await;
        {
            let requests = mock.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["command"], "send_files");
            assert_eq!(requests[0]["request_id"], "desktop-request");
            assert_eq!(requests[0]["paths"], json!(request.paths.unwrap()));
            assert_eq!(requests[0]["targets"], json!([captured(2, 8)]));
        }
        // The one-recipient rule is identical: keep asking, and Cancel admits nothing.
        worker.snapshot.devices.truncate(1);
        worker.status["snapshot"]["send_targets"] = json!([target(1, 7, false)]);
        worker
            .receive_send(SendRequest {
                id: "one".into(),
                paths: Some(vec![dir.path().join("one.txt")]),
                targets: vec![captured(1, 7)],
                clipboard: false,
                targeted: false,
                error: None,
            })
            .await;
        flush_ui();
        assert_eq!(window.global::<Ui>().get_dialog(), Dialog::Picker);
        worker.act(Action::PickerCancel).await;
        assert!(!worker.picker_active);
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        // Window clipboard and tray clipboard use the same picker, with connected-only eligibility.
        worker.origin.targets = Some(vec![serde_json::from_value(target(1, 7, false))?]);
        worker.act(Action::SendClipboard).await;
        flush_ui();
        assert_eq!(
            window.global::<Ui>().get_picker_title(),
            "Send clipboard to"
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        worker
            .act(Action::PickerSend(vec![id(1).as_str().into()]))
            .await;
        assert_eq!(
            mock.requests.lock().unwrap()[1]["command"],
            "send_clipboard"
        );
        assert_eq!(
            mock.requests.lock().unwrap()[1]["targets"],
            json!([captured(1, 7)])
        );
        worker
            .receive_send(SendRequest {
                id: "tray-clipboard".into(),
                paths: None,
                targets: vec![captured(1, 7)],
                clipboard: true,
                targeted: false,
                error: None,
            })
            .await;
        flush_ui();
        assert!(worker.picker_active);
        assert_eq!(window.global::<Ui>().get_dialog(), Dialog::Picker);
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn queued_desktop_handoffs_preserve_an_open_picker_and_acknowledge_once_adopted()
    -> Result<()> {
        if in_offscreen_child("ui::window::tests::queued_desktop_handoffs_preserve_an_open_picker_and_acknowledge_once_adopted", &[]).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut rx) = model_worker(&window, dir.path());
        worker.snapshot.send_targets =
            serde_json::from_value(json!([target(1, 7, false), target(2, 8, false)]))?;
        let mut status = status_value(&worker);
        status["config"]["choose_devices_each_time"] = json!(true);
        let request = |name: &str| SendRequest {
            id: name.into(),
            paths: Some(vec![dir.path().join(format!("{name}.txt"))]),
            targets: vec![crate::outbox::Authority {
                device: id(1),
                generation: 7,
                fingerprint: pneu_proto::Hash::new("ab".repeat(32)).unwrap(),
            }],
            clipboard: false,
            targeted: false,
            error: None,
        };
        status["pending_send"] = json!(request("first"));
        let visible = AtomicBool::new(true);
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        worker.refresh(status.clone(), &visible).await?;
        flush_ui();
        assert!(worker.picker_active);
        let first_origin = worker.picker_origin.clone();
        assert_eq!(
            mock.requests.lock().unwrap()[0]["command"],
            "acknowledge_send"
        );
        status["pending_send"] = json!(request("second"));
        status["window_request"] = json!(2);
        worker.refresh(status.clone(), &visible).await?;
        flush_ui();
        while let Ok(action) = rx.try_recv() {
            worker.act(action).await;
        }
        assert_eq!(worker.picker_origin, first_origin);
        assert_eq!(worker.view(), first_origin);
        assert!(worker.picker_active);
        worker.refresh(status.clone(), &visible).await?;
        flush_ui();
        assert_eq!(worker.picker_files, vec![dir.path().join("first.txt")]);
        assert_eq!(
            mock.requests.lock().unwrap().len(),
            1,
            "defer next handoff until current picker finishes"
        );
        worker.act(Action::PickerCancel).await;
        worker.refresh(status.clone(), &visible).await?;
        flush_ui();
        assert_eq!(worker.picker_files, vec![dir.path().join("second.txt")]);
        assert_eq!(mock.requests.lock().unwrap()[1]["request"], "second");
        // A lost acknowledgement may replay the status payload. Never re-admit it.
        worker.refresh(status, &visible).await?;
        assert!(
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| r["command"] == "acknowledge_send")
        );
        Ok(())
    }

    #[tokio::test]
    async fn desktop_send_errors_and_unwatched_admission_failures_are_visible() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::desktop_send_errors_and_unwatched_admission_failures_are_visible",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _rx) = model_worker(&window, dir.path());
        worker
            .receive_send(SendRequest {
                id: "error".into(),
                paths: None,
                targets: vec![],
                clipboard: false,
                targeted: false,
                error: Some("send needs 1 to 128 regular files".into()),
            })
            .await;
        flush_ui();
        assert_eq!(worker.action_error, "Couldn't send files.");
        worker
            .receive_send(SendRequest {
                id: "empty".into(),
                paths: Some(vec![dir.path().join("report.pdf")]),
                targets: vec![],
                clipboard: false,
                targeted: false,
                error: None,
            })
            .await;
        flush_ui();
        assert_eq!(worker.action_error, "Couldn't send files.");
        let mock = mock_ipc(&worker.paths.socket, |_| {
            (false, json!("file admission is busy"))
        })?;
        worker
            .receive_send(SendRequest {
                id: "busy".into(),
                paths: Some(vec![dir.path().join("report.pdf")]),
                targets: vec![crate::outbox::Authority {
                    device: id(1),
                    generation: 7,
                    fingerprint: pneu_proto::Hash::new("ab".repeat(32))?,
                }],
                clipboard: false,
                targeted: false,
                error: None,
            })
            .await;
        flush_ui();
        assert_eq!(worker.action_error, "Couldn't send files.");
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        // An operation can finish after the initiating CLI exits. Its failed
        // row is projected into Recent, with the admission reason and Remove.
        for reason in ["device outbox is full", "No such file or directory"] {
            let mut failed = transfer(id(9).as_str(), 0, "report.pdf", "failed", 0, 0);
            failed.transfer_id = failed.operation.clone();
            failed.error = Some(reason.into());
            let rows = recent(&peers(&worker.snapshot, &[]), &[failed]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].detail, reason);
            assert!(rows[0].can_dismiss);
            assert!(!rows[0].can_retry);
        }
        Ok(())
    }

    #[tokio::test]
    async fn navigation_rejects_stale_frames_actions_and_clipboard_previews() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::navigation_rejects_stale_frames_actions_and_clipboard_previews",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _) = model_worker(&window, dir.path());
        window.invoke_open_device(id(1).as_str().into());
        worker.origin = worker.view();
        worker.ui(|w| {
            w.global::<Ui>().set_device(DeviceItem {
                id: id(1).as_str().into(),
                ..Default::default()
            })
        });
        let obsolete = worker.view();
        window.invoke_open_device(id(2).as_str().into());
        flush_ui();
        assert_ne!(
            window.global::<Ui>().get_device().id.as_str(),
            id(1).as_str()
        );
        window.invoke_prepare_unpair();
        assert_eq!(window.global::<Ui>().get_dialog(), Dialog::None);
        worker
            .act(Action::Scoped(
                obsolete,
                Box::new(Action::SetPeer("clipboard".into(), false)),
            ))
            .await;
        assert!(worker.peer_settings.is_none());
        // The user navigates during parse-only Join. No offer or navigation may apply.
        window.invoke_open_add(1);
        worker.origin = worker.view();
        let view = worker.view.clone();
        let mock = mock_ipc(&worker.paths.socket, move |command| {
            if command["command"] == "join" {
                let mut v = view.lock().unwrap();
                v.page = Page::Settings;
                v.revision += 1;
                (
                    true,
                    json!({"state":"confirmation","attempt":"preview","name":"PC"}),
                )
            } else {
                (true, Value::Null)
            }
        })?;
        worker.paste(sample_uri(), true).await;
        flush_ui();
        assert!(worker.join.is_none());
        assert_eq!(worker.view().page, Page::Settings);
        assert!(
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .any(|c| c["command"] == "join_cancel")
        );
        Ok(())
    }

    #[tokio::test]
    async fn join_handle_survives_replacement_and_transient_errors_without_reconfirmation()
    -> Result<()> {
        if in_offscreen_child("ui::window::tests::join_handle_survives_replacement_and_transient_errors_without_reconfirmation", &[]).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _) = model_worker(&window, dir.path());
        window.invoke_open_add(1);
        worker.origin = worker.view();
        worker.join = Some(Join {
            attempt: "opaque-attempt".into(),
            uri: sample_uri(),
            name: "Duplicate name".into(),
            confirmed: false,
            cancel_requested: false,
            offer: false,
            link: true,
            origin: worker.view(),
            reported: String::new(),
        });
        let journal = worker.journal_path();
        let mut polls = 0;
        let mock = mock_ipc(&worker.paths.socket, move |command| {
            if command["command"] == "join_confirm_scoped" {
                assert!(journal.exists());
                return (true, json!({"state":"pairing"}));
            }
            if command["command"] == "join_status" {
                polls += 1;
                return if polls == 1 {
                    (true, json!({"mock_disconnect":true}))
                } else {
                    (true, json!({"state":"paired","device":id(2)}))
                };
            }
            (true, Value::Null)
        })?;
        worker.confirm_join().await;
        flush_ui();
        assert!(worker.join.as_ref().unwrap().confirmed);
        assert_eq!(window.global::<Ui>().get_dialog(), Dialog::None);
        // Reconstruct the worker as the supervisor does when replacing the child.
        let mut replacement = Worker::new(
            worker.paths.clone(),
            window.as_weak(),
            worker.view.clone(),
            0,
        );
        replacement.snapshot = snapshot(vec![
            device(1, "Duplicate name", false, "connected", 0),
            device(2, "Duplicate name", false, "connected", 0),
        ]);
        let value = status_value(&replacement);
        replacement
            .refresh(value.clone(), &AtomicBool::new(true))
            .await?;
        flush_ui();
        replacement.act(Action::Navigated).await;
        replacement
            .refresh(value.clone(), &AtomicBool::new(true))
            .await?;
        assert!(
            replacement
                .join
                .as_ref()
                .is_some_and(|j| j.attempt == "opaque-attempt")
        );
        replacement.refresh(value, &AtomicBool::new(true)).await?;
        assert_eq!(
            replacement.paired.as_ref().unwrap().device.as_deref(),
            Some(id(2).as_str())
        );
        assert!(!replacement.journal_path().exists());
        window.invoke_go(Page::Settings);
        replacement.act(Action::Navigated).await;
        flush_ui();
        assert!(replacement.paired.is_none());
        assert_eq!(window.global::<Ui>().get_page(), Page::Settings);
        assert_eq!(
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c["command"] == "join_confirm_scoped")
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn pairing_code_recovers_and_distinguishes_other_errors() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::pairing_code_recovers_and_distinguishes_other_errors",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        window.invoke_open_code();
        let destination = worker.view();
        assert_eq!(destination.page, Page::Code);
        assert!(
            matches!(actions.try_recv()?, Action::Scoped(view, action) if view == destination && matches!(*action, Action::Navigated))
        );
        assert!(
            matches!(actions.try_recv()?, Action::Scoped(view, action) if view == destination && matches!(*action, Action::ShowCode))
        );
        worker.origin = destination;
        worker.act(Action::Navigated).await;
        let mut attempts = 0;
        let mock = mock_ipc(&worker.paths.socket, move |_| {
            attempts += 1;
            match attempts {
                1 => (false, json!("no reachable interface for pairing")),
                2 => (
                    true,
                    serde_json::to_value(pneu_proto::PairingUri::parse(&sample_uri()).unwrap())
                        .unwrap(),
                ),
                _ => (false, json!("storage permission denied")),
            }
        })?;
        worker.show_code(false).await;
        assert!(worker.code.offline);
        assert!(worker.code.error.starts_with("Connect this PC"));
        worker.code.last_attempt = 0;
        let value = status_value(&worker);
        worker.refresh(value, &AtomicBool::new(true)).await?;
        assert!(worker.code.token.is_some());
        assert!(worker.code.error.is_empty());
        let invitation: pneu_proto::PairingUri =
            serde_json::from_value(worker.status["invitation"].clone())?;
        assert_eq!(
            worker
                .frame(
                    &Config::default(),
                    &[],
                    None,
                    Some(&invitation),
                    false,
                    crate::now_ms()
                )
                .code_state,
            CodeState::Showing
        );
        worker.show_code(false).await;
        assert!(!worker.code.offline);
        assert_eq!(worker.code.error, "Couldn't create a pairing code");
        assert_eq!(mock.requests.lock().unwrap().len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_join_handles_clear_on_restore_confirm_and_cancel() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::rejected_join_handles_clear_on_restore_confirm_and_cancel",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _) = model_worker(&window, dir.path());
        window.invoke_open_add(1);
        worker.origin = worker.view();
        let join = Join {
            attempt: "lost-with-daemon".into(),
            uri: sample_uri(),
            name: "Mac".into(),
            confirmed: true,
            cancel_requested: false,
            offer: false,
            link: true,
            origin: worker.view(),
            reported: String::new(),
        };
        worker.save_join(&join)?;
        worker.status = Value::Null;
        let mock = mock_ipc(&worker.paths.socket, |_| {
            (false, json!("unknown join attempt"))
        })?;
        worker
            .refresh(status_value(&worker), &AtomicBool::new(true))
            .await?;
        flush_ui();
        worker.act(Action::Navigated).await;
        worker
            .refresh(status_value(&worker), &AtomicBool::new(true))
            .await?;
        flush_ui();
        assert!(worker.join.is_none());
        assert!(!worker.journal_path().exists());
        assert_eq!(window.global::<Ui>().get_dialog(), Dialog::Message);
        assert!(
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r["command"] == "join_status")
        );
        worker.join = Some(Join {
            origin: worker.view(),
            confirmed: false,
            ..join.clone()
        });
        worker.confirm_join().await;
        assert!(worker.join.is_none());
        assert!(!worker.journal_path().exists());
        worker.cancel_join(&join).await;
        assert!(!worker.journal_path().exists());
        Ok(())
    }

    #[tokio::test]
    async fn recent_actions_are_device_scoped_deduplicated_and_dismiss_history() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::recent_actions_are_device_scoped_deduplicated_and_dismiss_history",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _) = model_worker(&window, dir.path());
        let op = id(9).as_str().to_owned();
        worker.transfers = vec![
            transfer(&op, 1, "one.pdf", "failed", 0, 10),
            transfer(&op, 1, "two.pdf", "failed", 0, 10),
            transfer(&op, 2, "one.pdf", "sending", 2, 10),
        ];
        let row = artifact_key(&worker.transfers[0]);
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        worker.act(Action::Retry(row.clone())).await;
        worker
            .act(Action::Recipient(row.clone(), id(2).as_str().into(), false))
            .await;
        worker
            .act(Action::Recipient(row.clone(), id(1).as_str().into(), true))
            .await;
        worker.act(Action::Dismiss(row.clone())).await;
        worker.act(Action::Cancel(row)).await;
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        for request in requests.iter() {
            assert_eq!(request["operation"], op);
            assert_eq!(request["artifact"], worker.transfers[0].transfer_id);
            assert_ne!(request["artifact"], worker.transfers[1].transfer_id);
        }
        assert_eq!(requests[4]["command"], "cancel_transfer");
        assert!(requests[4]["device"].is_null());
        let rows = recent(
            &peers(&worker.snapshot, &worker.transfers),
            &worker.transfers,
        );
        assert!(
            rows.iter()
                .find(|r| r.title == "two.pdf")
                .unwrap()
                .can_dismiss,
            "an active sibling cannot block removing this failed artifact"
        );
        assert_eq!(requests[0]["command"], "retry_transfer");
        assert_eq!(requests[0]["device"], id(1).as_str());
        assert_eq!(requests[1]["command"], "cancel_transfer");
        assert_eq!(requests[1]["device"], id(2).as_str());
        assert_eq!(requests[2]["command"], "retry_transfer");
        assert_eq!(requests[2]["device"], id(1).as_str());
        assert_eq!(requests[3]["command"], "dismiss_transfer");
        assert!(requests[3].get("device").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn settings_switches_write_only_their_field() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::settings_switches_write_only_their_field",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _) = model_worker(&window, dir.path());
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        for key in [
            "choose_devices_each_time",
            "queue_offline",
            "save_to_folder",
            "notification_preview",
            "copy_image_to_clipboard",
            "open_in_viewer",
            "clipboard_include_images",
        ] {
            worker.set_flag(key, false).await?;
        }
        worker.act(Action::SetMode(1)).await;
        let origin = worker.view();
        window.invoke_go(Page::Main);
        worker
            .act(Action::Scoped(
                origin,
                Box::new(Action::SetFlag("queue_offline".into(), true)),
            ))
            .await;
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 9);
        assert!(
            requests
                .iter()
                .all(|c| c["command"] == "set_setting" && c.get("config").is_none())
        );
        assert_eq!(requests[7]["key"], "clipboard_mode");
        assert_eq!(requests[7]["value"], "manual");
        Ok(())
    }

    #[tokio::test]
    async fn modal_keyboard_stays_inside_and_rename_rejects_whitespace() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::modal_keyboard_stays_inside_and_rename_rejects_whitespace",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (_worker, mut rx) = model_worker(&window, dir.path());
        let ui = window.global::<Ui>();
        ui.set_motion(false);
        ui.set_device_id(id(1).as_str().into());
        ui.set_device_revision(ui.get_navigation_revision());
        ui.set_device(DeviceItem {
            id: id(1).as_str().into(),
            name: "Phone".into(),
            ..Default::default()
        });
        window.show()?;
        surface.set_size(slint::PhysicalSize::new(400, 600));
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        for reverse in [false, true] {
            for tabs in 0..12 {
                ui.set_unpair_id(id(1).as_str().into());
                ui.set_unpair_name("Phone".into());
                ui.set_dialog(Dialog::Unpair);
                surface.request_redraw();
                surface.draw_if_needed(|renderer| {
                    let mut pixels = vec![
                            slint::platform::software_renderer::PremultipliedRgbaColor::default();
                            400 * 600
                        ];
                    renderer.render(&mut pixels, 400);
                });
                advance_ui();
                for _ in 0..tabs {
                    key(
                        &window,
                        if reverse {
                            slint::platform::Key::Backtab
                        } else {
                            slint::platform::Key::Tab
                        },
                    );
                }
                key(&window, slint::platform::Key::Return);
                assert_eq!(
                    ui.get_dialog(),
                    Dialog::None,
                    "tabs={tabs}, reverse={reverse}"
                );
                while let Ok(action) = rx.try_recv() {
                    match action {
                        Action::Scoped(_, action) => assert!(matches!(*action, Action::Unpair(_))),
                        _ => panic!("background action escaped modal"),
                    }
                }
                // Let the closed popup leave the component tree before reopening.
                surface.request_redraw();
                surface.draw_if_needed(|renderer| {
                    let mut pixels = vec![
                            slint::platform::software_renderer::PremultipliedRgbaColor::default();
                            400 * 600
                        ];
                    renderer.render(&mut pixels, 400);
                });
                flush_ui();
            }
        }
        ui.set_rename_text(" \t\u{2003}".into());
        ui.set_dialog(Dialog::Rename);
        surface.request_redraw();
        surface.draw_if_needed(|renderer| {
            let mut pixels = vec![
                    slint::platform::software_renderer::PremultipliedRgbaColor::default();
                    400 * 600
                ];
            renderer.render(&mut pixels, 400);
        });
        advance_ui();
        key(&window, slint::platform::Key::Return);
        assert_eq!(ui.get_dialog(), Dialog::Rename);
        assert!(!ui.invoke_valid_name(" \t\u{2003}".into()));
        ui.set_rename_text("Router".into());
        key(&window, slint::platform::Key::Return);
        assert_eq!(ui.get_dialog(), Dialog::None);
        ui.set_dialog(Dialog::Picker);
        advance_ui();
        key(&window, slint::platform::Key::Escape);
        assert_eq!(ui.get_dialog(), Dialog::None);
        Ok(())
    }

    #[tokio::test]
    async fn download_code_and_cant_connect_follow_their_interaction_rules() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::download_code_and_cant_connect_follow_their_interaction_rules",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (_worker, mut rx) = model_worker(&window, dir.path());
        let ui = window.global::<Ui>();
        ui.set_motion(false);
        window.show()?;
        surface.set_size(slint::PhysicalSize::new(400, 700));
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        let draw = || {
            surface.request_redraw();
            surface.draw_if_needed(|renderer| {
                let mut pixels = vec![
                    slint::platform::software_renderer::PremultipliedRgbaColor::default();
                    400 * 700
                ];
                renderer.render(&mut pixels, 400);
            });
            advance_ui();
        };
        let mut tailscale_requests = || {
            let mut count = 0;
            while let Ok(action) = rx.try_recv() {
                if let Action::Scoped(_, action) = action
                    && matches!(*action, Action::GetTailscale)
                {
                    count += 1;
                }
            }
            count
        };

        // Esc returns from the download code to the pairing code, then leaves.
        window.invoke_open_add(0);
        ui.set_download(true);
        draw();
        key(&window, slint::platform::Key::Escape);
        assert!(!ui.get_download());
        assert_eq!(ui.get_page(), Page::Add);
        key(&window, slint::platform::Key::Escape);
        assert_eq!(ui.get_page(), Page::Main);
        // Any navigation starts again on the pairing code.
        ui.set_download(true);
        window.invoke_open_add(0);
        assert!(!ui.get_download());
        // The header's Back leaves Add Device straight from the download code.
        ui.set_download(true);
        draw();
        key(&window, slint::platform::Key::Tab); // Back
        key(&window, slint::platform::Key::Return);
        assert_eq!(ui.get_page(), Page::Main);
        assert!(!ui.get_download());
        // The printed address copies the whole download page.
        window.invoke_open_add(0);
        ui.set_download(true);
        draw();
        key(&window, slint::platform::Key::Tab); // Back
        key(&window, slint::platform::Key::Tab); // Phone / Computer
        key(&window, slint::platform::Key::Tab); // stantonmatt.github.io/pneu
        key(&window, slint::platform::Key::Return);
        assert_eq!(*COPIED.lock().unwrap(), DOWNLOAD_PAGE);
        assert_eq!(ui.get_page(), Page::Add);
        assert!(ui.get_download());

        // Get Tailscale… asks the worker to open the page and closes the popup.
        window.invoke_go(Page::Main);
        ui.set_unreachable(true);
        ui.set_dialog(Dialog::Help);
        draw();
        key(&window, slint::platform::Key::Tab);
        key(&window, slint::platform::Key::Return);
        assert_eq!(ui.get_dialog(), Dialog::None);
        assert_eq!(tailscale_requests(), 1);
        // Close and Esc only dismiss it.
        for close in [slint::platform::Key::Return, slint::platform::Key::Escape] {
            draw();
            ui.set_dialog(Dialog::Help);
            draw();
            key(&window, close);
            assert_eq!(ui.get_dialog(), Dialog::None);
        }
        // With Tailscale running here, Close is the only button.
        ui.set_tailscale(true);
        draw();
        ui.set_dialog(Dialog::Help);
        draw();
        key(&window, slint::platform::Key::Tab);
        key(&window, slint::platform::Key::Return);
        assert_eq!(ui.get_dialog(), Dialog::None);
        assert_eq!(tailscale_requests(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn download_code_resets_when_leaving_the_phone_tab() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::download_code_resets_when_leaving_the_phone_tab",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (_worker, _rx) = model_worker(&window, dir.path());
        let ui = window.global::<Ui>();
        ui.set_motion(false);
        ui.set_first_run(true);
        let pairing_qr = slint::Image::from_rgba8(qr_pixels(&sample_uri())?);
        ui.set_qr(pairing_qr.clone());
        window.show()?;
        surface.set_size(slint::PhysicalSize::new(400, 700));
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        let draw = || {
            surface.request_redraw();
            surface.draw_if_needed(|renderer| {
                let mut pixels = vec![
                    slint::platform::software_renderer::PremultipliedRgbaColor::default();
                    400 * 700
                ];
                renderer.render(&mut pixels, 400);
            });
            advance_ui();
        };

        window.invoke_open_add(0);
        draw();
        key(&window, slint::platform::Key::Tab); // Phone / Computer
        key(&window, slint::platform::Key::Tab); // Get Pneu for Android
        key(&window, slint::platform::Key::Return);
        assert!(ui.get_download());
        draw();
        key(&window, slint::platform::Key::Tab);
        key(&window, slint::platform::Key::RightArrow); // Computer
        assert_eq!(ui.get_add_tab(), 1);
        draw();
        key(&window, slint::platform::Key::LeftArrow); // Phone
        draw();
        assert_eq!(ui.get_add_tab(), 0);
        assert!(!ui.get_download(), "Phone must return to the pairing QR");
        assert_eq!(ui.get_qr(), pairing_qr);

        key(&window, slint::platform::Key::Tab); // Get Pneu for Android
        key(&window, slint::platform::Key::Return);
        assert!(ui.get_download());
        draw();
        key(&window, slint::platform::Key::Tab);
        key(&window, slint::platform::Key::RightArrow); // Computer
        assert_eq!(ui.get_add_tab(), 1);
        ui.set_first_run(false);
        draw();
        key(&window, slint::platform::Key::Escape);
        assert_eq!(ui.get_page(), Page::Main, "Esc must leave Add Device");
        assert!(!ui.get_download());
        Ok(())
    }

    // A native test fixture for private KWin/AT-SPI checks. Normal cargo tests
    // return immediately; no daemon, clipboard, session bus, or user data is used.
    #[test]
    fn isolated_native_window_fixture() -> Result<()> {
        let Some(scene_name) = std::env::var_os("PNEU_NATIVE_SCENE") else {
            return Ok(());
        };
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("private runtime")?);
        let root = PathBuf::from(
            std::env::var_os("PNEU_NATIVE_PRIVATE_ROOT").context("private fixture root")?,
        );
        anyhow::ensure!(
            root.is_absolute()
                && root.components().any(|c| c.as_os_str() == "agent-scratch")
                && runtime.starts_with(&root)
                && std::env::var("DBUS_SESSION_BUS_ADDRESS")?
                    .contains(root.to_str().context("fixture root encoding")?),
            "native fixture requires private KWin and D-Bus"
        );
        slint::BackendSelector::new().select()?;
        slint::set_xdg_app_id("pneu-ui-test")?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _rx) = model_worker(&window, dir.path());
        let scene = scenes()
            .into_iter()
            .find(|s| s.name == scene_name.to_string_lossy())
            .context("fixture scene")?;
        (scene.setup)(&mut worker);
        let ps = peers(&worker.snapshot, &worker.transfers);
        let config: Config =
            serde_json::from_value(worker.status["config"].clone()).unwrap_or_default();
        let device = ps.iter().find(|p| p.id == worker.view().device);
        let invitation = serde_json::from_value::<Option<pneu_proto::PairingUri>>(
            worker.status["invitation"].clone(),
        )
        .unwrap_or(None);
        apply(
            &window,
            worker.frame(
                &config,
                &ps,
                device,
                invitation.as_ref(),
                ps.is_empty(),
                crate::now_ms(),
            ),
        );
        (scene.ui)(&window);
        window.global::<Ui>().set_motion(false);
        window.show()?;
        slint::run_event_loop_until_quit()?;
        Ok(())
    }

    #[tokio::test]
    async fn confirmation_keeps_displayed_device_when_status_changes() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::confirmation_keeps_displayed_device_when_status_changes",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let ui = window.global::<Ui>();
        let displayed = id(1);
        let replacement = id(2);
        ui.set_device(DeviceItem {
            id: displayed.as_str().into(),
            name: "Same phone name".into(),
            ..Default::default()
        });
        ui.set_device_id(displayed.as_str().into());
        ui.set_device_revision(ui.get_navigation_revision());
        window.invoke_prepare_unpair();
        // Even equal labels must not allow a changed device ID to replace the target.
        ui.set_device(DeviceItem {
            id: replacement.as_str().into(),
            name: "Same phone name".into(),
            ..Default::default()
        });
        let submitted = std::rc::Rc::new(std::cell::RefCell::new(None));
        let target = submitted.clone();
        ui.on_unpair(move |device| {
            *target.borrow_mut() = Some(device);
        });
        window.invoke_submit_unpair();
        assert_eq!(
            submitted.borrow().as_ref().unwrap().as_str(),
            displayed.as_str()
        );
        Ok(())
    }

    fn linked_status(worker: &Worker, setup: &Value) -> Value {
        let mut value = status_value(worker);
        value["setup"] = setup.clone();
        value["self_id"] = json!(id(9));
        value
    }
    async fn drain(
        worker: &mut Worker,
        actions: &mut tokio::sync::mpsc::UnboundedReceiver<Action>,
    ) {
        flush_ui();
        while let Ok(action) = actions.try_recv() {
            act_with_ui(worker, action).await;
        }
    }
    /// Draw once so popups exist, then let them focus their default button.
    fn settle(surface: &slint::platform::software_renderer::MinimalSoftwareWindow) {
        surface.request_redraw();
        surface.draw_if_needed(|renderer| {
            let mut pixels = vec![
                    slint::platform::software_renderer::PremultipliedRgbaColor::default();
                    400 * 720
                ];
            renderer.render(&mut pixels, 400);
        });
        advance_ui();
    }
    fn sent(mock: &MockIpc, command: &str) -> Vec<Value> {
        mock.requests.lock().unwrap().iter().flat_map(|request| {
            if command == "setup_decision" && request["command"] == "setup_decisions" {
                request["requests"].as_array().unwrap().iter().map(|r| json!({
                    "command":"setup_decision", "device":r["device"],
                    "setup_generation":r["setup_generation"], "binding_generation":r["binding_generation"], "allow":request["allow"],
                })).collect()
            } else if request["command"] == command { vec![request.clone()] }
            else { vec![] }
        }).collect()
    }

    #[tokio::test]
    async fn pairing_switch_is_frozen_into_the_confirmation() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::pairing_switch_is_frozen_into_the_confirmation",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        use slint::platform::Key;
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let mock = mock_ipc(&worker.paths.socket, |command| {
            let reply = match command["command"].as_str() {
                Some("join") => {
                    json!({"state":"confirmation", "attempt":"popup", "name":"MJStanton-Desktop"})
                }
                Some("join_confirm_scoped") => json!({"state":"pairing"}),
                Some("join_status") => json!({"state":"confirmation"}),
                _ => Value::Null,
            };
            (true, reply)
        })?;
        let ui = window.global::<Ui>();
        ui.set_motion(false);
        window.show()?;
        surface.set_size(slint::PhysicalSize::new(400, 720));
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        window.invoke_open_add(1);
        drain(&mut worker, &mut actions).await;

        // Paste Link: the popup asks, with the switch on.
        worker.paste(sample_uri(), false).await;
        flush_ui();
        assert_eq!(ui.get_dialog(), Dialog::Message);
        assert_eq!(ui.get_message().action, "join-confirm");
        assert!(ui.get_link_on());
        settle(&surface);
        // Cancel has focus. Shift+Tab reaches the switch; Return turns it off,
        // then Pair confirms with the switch as shown.
        key(&window, Key::Backtab);
        key(&window, Key::Return);
        assert!(!ui.get_link_on());
        key(&window, Key::Tab);
        key(&window, Key::Tab);
        key(&window, Key::Return);
        assert_eq!(ui.get_dialog(), Dialog::None);
        drain(&mut worker, &mut actions).await;
        let confirmed = sent(&mock, "join_confirm_scoped");
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0]["attempt"], "popup");
        assert_eq!(confirmed[0]["enabled"], false);
        assert!(!worker.load_join().unwrap().link);

        // Turning the switch on afterwards changes nothing: asked again, the
        // popup shows the choice that was frozen.
        ui.set_link_on(true);
        worker.poll_join().await;
        flush_ui();
        assert_eq!(ui.get_dialog(), Dialog::Message);
        assert!(!ui.get_link_on());
        ui.set_dialog(Dialog::None);
        ui.invoke_message_action("join-confirm".into(), false);
        drain(&mut worker, &mut actions).await;
        settle(&surface);

        // At the limit the switch is off and disabled; Pair makes a plain pairing.
        worker.paste(sample_uri(), false).await;
        flush_ui();
        ui.set_link_full(true);
        settle(&surface);
        key(&window, Key::Tab);
        key(&window, Key::Return);
        drain(&mut worker, &mut actions).await;
        let confirmed = sent(&mock, "join_confirm_scoped");
        assert_eq!(confirmed.len(), 2);
        assert_eq!(confirmed[1]["enabled"], false);
        assert!(
            ui.get_link_on(),
            "the limit doesn't change the user's choice"
        );
        ui.set_link_full(false);
        worker.join = None;

        // Link on the clipboard pairs without the popup, with its own switch.
        worker.paste(sample_uri(), true).await;
        flush_ui();
        assert!(worker.join.as_ref().is_some_and(|j| j.offer));
        assert!(ui.get_link_on());
        ui.set_link_on(false);
        ui.invoke_offer_pair(ui.get_link_on() && !ui.get_link_full());
        drain(&mut worker, &mut actions).await;
        let confirmed = sent(&mock, "join_confirm_scoped");
        assert_eq!(confirmed.len(), 3);
        assert_eq!(confirmed[2]["enabled"], false);
        Ok(())
    }

    #[tokio::test]
    async fn requests_offer_and_device_rows_answer_and_link() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::requests_offer_and_device_rows_answer_and_link",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        std::fs::create_dir_all(&worker.paths.data)?;
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        let ui = window.global::<Ui>();
        let visible = AtomicBool::new(true);
        let mut devices = trio();
        devices.push(device(4, "Office-PC", false, "connected", 0));
        state(&mut worker, devices, Vec::new());
        let asked = |n| with(link_of(n, "ask"), "prompt", json!(true));

        // One device asked: Allow answers it and nothing else.
        let setup = setup_json(vec![asked(1), link_of(2, "ask"), link_of(4, "ask")], &[]);
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        flush_ui();
        let top = ui.get_top();
        assert_eq!(top.kind, TopKind::Request);
        assert_eq!(top.title, "Galaxy S25+ wants to link your devices");
        assert!(top.phone);
        ui.invoke_top_action(true);
        drain(&mut worker, &mut actions).await;
        let answers = sent(&mock, "setup_decision");
        assert_eq!(answers.len(), 1);
        assert_eq!(
            answers[0],
            json!({"command":"setup_decision", "device": id(1), "setup_generation": 2,
                "binding_generation": 3, "allow": true})
        );

        // Two asked: Don't Allow answers both, and only them.
        let setup = setup_json(vec![asked(1), asked(2), link_of(4, "ask")], &[]);
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        flush_ui();
        assert_eq!(
            ui.get_top().title,
            "Galaxy S25+ and MacBook Air want to link your devices"
        );
        ui.invoke_top_action(false);
        drain(&mut worker, &mut actions).await;
        let answers = sent(&mock, "setup_decision");
        assert_eq!(answers.len(), 3);
        assert!(answers[1..].iter().all(|a| a["allow"] == false));
        assert_eq!(answers[1]["device"], json!(id(1)));
        assert_eq!(answers[2]["device"], json!(id(2)));

        // ✕ leaves them waiting; a third device asking shows the card again.
        ui.invoke_top_close();
        drain(&mut worker, &mut actions).await;
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_top().kind, TopKind::None);
        let three = setup_json(vec![asked(1), asked(2), asked(4)], &[]);
        worker
            .refresh(linked_status(&worker, &three), &visible)
            .await?;
        flush_ui();
        assert_eq!(
            ui.get_top().title,
            "Galaxy S25+ and 2 other devices want to link your devices"
        );
        // Closing is kept until Pneu restarts, even after the window does.
        ui.invoke_top_close();
        drain(&mut worker, &mut actions).await;
        let reopened = Worker::new(
            worker.paths.clone(),
            window.as_weak(),
            worker.view.clone(),
            0,
        );
        assert_eq!(
            setup::top(
                &Setup::parse(&json!({"setup": three})),
                &peers(&worker.snapshot, &[]),
                &reopened.remembered
            ),
            Top::None
        );
        let mut restarted = reopened.remembered.clone();
        restarted.daemon = "an earlier run".into();
        assert!(matches!(
            setup::top(
                &Setup::parse(&json!({"setup": three})),
                &peers(&worker.snapshot, &[]),
                &restarted
            ),
            Top::Request { .. }
        ));

        // The offer: Link covers every undecided pairing, then says where to finish.
        let undecided = setup_json(
            vec![link_of(1, "ask"), link_of(2, "ask"), link_of(4, "ask")],
            &[],
        );
        worker
            .refresh(linked_status(&worker, &undecided), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_top().kind, TopKind::Offer);
        assert_eq!(ui.get_top().title, "Link your devices");
        ui.invoke_top_action(true);
        drain(&mut worker, &mut actions).await;
        assert_eq!(
            sent(&mock, "link_existing_device"),
            vec![json!({"command":"link_existing_device", "device": id(1),
                "binding_generation": 3, "expected_scope": "ask"})]
        );
        assert_eq!(worker.remembered.offer.as_deref(), Some(setup::LINKED));
        let waiting = |n: u8| with(link_of(n, "enabled"), "participating", json!(false));
        let linking_now = setup_json(
            vec![link_of(1, "enabled"), waiting(2), waiting(4)],
            &[(1, "Galaxy S25+")],
        );
        worker
            .refresh(linked_status(&worker, &linking_now), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_top().kind, TopKind::Waiting);
        assert_eq!(
            ui.get_top().title,
            "Allow linking on MacBook Air and Office-PC"
        );
        // When the last one answers, the card goes for good.
        let linked = setup_json(
            vec![
                link_of(1, "enabled"),
                link_of(2, "enabled"),
                link_of(4, "enabled"),
            ],
            &[(1, "Galaxy S25+"), (2, "MacBook Air"), (4, "Office-PC")],
        );
        worker
            .refresh(linked_status(&worker, &linked), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_top().kind, TopKind::None);
        assert_eq!(worker.remembered.offer.as_deref(), Some(setup::DONE));

        // ✕ on a fresh offer hides it for good, across windows.
        worker.remembered = Remembered::default();
        worker
            .refresh(linked_status(&worker, &undecided), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_top().kind, TopKind::Offer);
        ui.invoke_top_close();
        drain(&mut worker, &mut actions).await;
        let reopened = Worker::new(
            worker.paths.clone(),
            window.as_weak(),
            worker.view.clone(),
            0,
        );
        assert_eq!(reopened.remembered.offer.as_deref(), Some(setup::DISMISSED));

        // A device page links or answers just that pairing.
        ui.invoke_link_device(id(2).as_str().into());
        drain(&mut worker, &mut actions).await;
        assert_eq!(
            sent(&mock, "link_existing_device")[1]["device"],
            json!(id(2))
        );
        worker
            .refresh(linked_status(&worker, &three), &visible)
            .await?;
        flush_ui();
        ui.invoke_answer(id(4).as_str().into(), true);
        drain(&mut worker, &mut actions).await;
        let answers = sent(&mock, "setup_decision");
        assert_eq!(answers.last().unwrap()["device"], json!(id(4)));
        assert_eq!(answers.last().unwrap()["allow"], true);
        Ok(())
    }

    #[tokio::test]
    async fn remove_from_all_devices_confirms_what_it_showed() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::remove_from_all_devices_confirms_what_it_showed",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        use slint::platform::Key;
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let refuse = Arc::new(AtomicBool::new(false));
        let refusing = refuse.clone();
        let mock = mock_ipc(&worker.paths.socket, move |command| {
            if command["command"] == "remove_from_setup" && refusing.load(Ordering::SeqCst) {
                (false, json!("stale setup removal confirmation"))
            } else {
                (true, Value::Null)
            }
        })?;
        let ui = window.global::<Ui>();
        ui.set_motion(false);
        window.show()?;
        surface.set_size(slint::PhysicalSize::new(400, 720));
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        let visible = AtomicBool::new(true);
        state(&mut worker, trio(), Vec::new());
        let via = with(
            link_of(2, "enabled"),
            "introducer_name",
            json!("Galaxy S25+"),
        );
        let setup = setup_json(
            vec![link_of(1, "enabled"), via],
            &[(1, "Galaxy S25+"), (2, "MacBook Air")],
        );
        window.invoke_open_device(id(2).as_str().into());
        drain(&mut worker, &mut actions).await;
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        flush_ui();
        let link = ui.get_device_link();
        assert!(link.member);
        assert_eq!(link.via, "Galaxy S25+");
        assert_eq!(link.row, LinkRow::None);

        // Cancel has focus: Return keeps the device.
        window.invoke_prepare_remove();
        settle(&surface);
        assert_eq!(ui.get_dialog(), Dialog::Remove);
        assert_eq!(ui.get_remove_name(), "MacBook Air");
        key(&window, Key::Return);
        assert_eq!(ui.get_dialog(), Dialog::None);
        drain(&mut worker, &mut actions).await;
        assert!(sent(&mock, "remove_from_setup").is_empty());
        // Let the closed popup leave the component tree before reopening.
        settle(&surface);

        // Couldn't remove: nothing changed, and it says so.
        refuse.store(true, Ordering::SeqCst);
        window.invoke_prepare_remove();
        settle(&surface);
        key(&window, Key::Tab);
        key(&window, Key::Return);
        drain(&mut worker, &mut actions).await;
        assert_eq!(ui.get_dialog(), Dialog::Message);
        let message = ui.get_message();
        assert_eq!(message.title, "Couldn't remove MacBook Air");
        assert_eq!(message.text, "Nothing was changed. Try again.");
        assert_eq!(message.primary, "OK");
        assert_eq!(worker.view().page, Page::Device);
        ui.set_dialog(Dialog::None);
        settle(&surface);

        // Remove sends the membership the confirmation showed, even when the
        // status changes before even the queued popup-open action is processed.
        refuse.store(false, Ordering::SeqCst);
        window.invoke_prepare_remove();
        let mut newer = setup.clone();
        newer["generation"] = json!(5);
        newer["binding_generations"][id(2).as_str()] = json!(9);
        newer["roster"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|m| m["device_id"] == json!(id(2)))
            .unwrap()["fingerprint"] = json!("09".repeat(32));
        worker
            .refresh(linked_status(&worker, &newer), &visible)
            .await?;
        settle(&surface);
        key(&window, Key::Tab);
        key(&window, Key::Return);
        drain(&mut worker, &mut actions).await;
        let removed = sent(&mock, "remove_from_setup");
        assert_eq!(removed.len(), 2);
        assert_eq!(
            removed[1],
            json!({"command":"remove_from_setup", "device": id(2), "setup_generation": 2,
                "binding_generation": 3, "fingerprint": "02".repeat(32)})
        );
        assert_eq!(worker.view().page, Page::Main);
        assert_eq!(ui.get_dialog(), Dialog::None);
        // Plain pairings keep Unpair….
        assert!(sent(&mock, "unpair").is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn top_card_queued_actions_keep_displayed_askers_and_fences() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::top_card_queued_actions_keep_displayed_askers_and_fences",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        std::fs::create_dir_all(&worker.paths.data)?;
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        let ui = window.global::<Ui>();
        let visible = AtomicBool::new(true);
        state(&mut worker, trio(), vec![]);
        let asked = |n| with(link_of(n, "ask"), "prompt", json!(true));
        let shown = setup_json(vec![asked(1), link_of(2, "ask")], &[]);
        let mut newer = setup_json(vec![asked(1), asked(2)], &[]);
        newer["generation"] = json!(5);
        newer["binding_generations"][id(1).as_str()] = json!(9);
        for allow in [false, true] {
            worker
                .refresh(linked_status(&worker, &shown), &visible)
                .await?;
            flush_ui();
            worker
                .refresh(linked_status(&worker, &newer), &visible)
                .await?;
            // The worker refreshed; its new frame has not reached the UI yet.
            ui.invoke_top_action(allow);
            drain(&mut worker, &mut actions).await;
        }
        let decisions = sent(&mock, "setup_decision");
        assert_eq!(decisions.len(), 2);
        for (decision, allow) in decisions.iter().zip([false, true]) {
            assert_eq!(
                *decision,
                json!({"command":"setup_decision", "device":id(1), "setup_generation":2, "binding_generation":3, "allow":allow})
            );
        }
        worker
            .refresh(linked_status(&worker, &shown), &visible)
            .await?;
        flush_ui();
        ui.invoke_top_close();
        worker
            .refresh(linked_status(&worker, &newer), &visible)
            .await?;
        drain(&mut worker, &mut actions).await;
        assert!(worker.remembered.closed(&format!("{}/2/3", id(1))));
        assert!(!worker.remembered.closed(&format!("{}/5/9", id(1))));
        assert!(!worker.remembered.closed(&format!("{}/5/3", id(2))));
        worker
            .refresh(linked_status(&worker, &newer), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_top().kind, TopKind::Request);

        // An offer click must not become an implicit Allow of a new asker.
        worker.remembered = Remembered::default();
        let offer = setup_json(vec![link_of(1, "ask"), link_of(2, "ask")], &[]);
        worker
            .refresh(linked_status(&worker, &offer), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_top().kind, TopKind::Offer);
        ui.invoke_top_action(true);
        worker
            .refresh(linked_status(&worker, &newer), &visible)
            .await?;
        drain(&mut worker, &mut actions).await;
        assert!(sent(&mock, "link_existing_device").is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn device_page_queued_answer_keeps_displayed_prompt_fences() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::device_page_queued_answer_keeps_displayed_prompt_fences",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        let ui = window.global::<Ui>();
        let visible = AtomicBool::new(true);
        state(&mut worker, trio(), vec![]);
        window.invoke_open_device(id(1).as_str().into());
        drain(&mut worker, &mut actions).await;
        let shown = setup_json(vec![with(link_of(1, "ask"), "prompt", json!(true))], &[]);
        for (generation, binding) in [(2, 9), (5, 3)] {
            for allow in [false, true] {
                worker
                    .refresh(linked_status(&worker, &shown), &visible)
                    .await?;
                flush_ui();
                assert_eq!(ui.get_device_link().row, LinkRow::Asked);
                let mut newer = shown.clone();
                newer["generation"] = json!(generation);
                newer["binding_generations"][id(1).as_str()] = json!(binding);
                worker
                    .refresh(linked_status(&worker, &newer), &visible)
                    .await?;
                // The refreshed frame has not yet reached the device page.
                ui.invoke_answer(id(1).as_str().into(), allow);
                drain(&mut worker, &mut actions).await;
            }
        }
        let decisions = sent(&mock, "setup_decision");
        assert_eq!(decisions.len(), 4);
        for (decision, allow) in decisions.iter().zip([false, true, false, true]) {
            assert_eq!(
                *decision,
                json!({"command":"setup_decision", "device":id(1),
                "setup_generation":2, "binding_generation":3, "allow":allow})
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn queued_link_actions_keep_displayed_binding_and_scope() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::queued_link_actions_keep_displayed_binding_and_scope",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        std::fs::create_dir_all(&worker.paths.data)?;
        let mock = mock_ipc(&worker.paths.socket, |_| (true, json!(false)))?;
        let ui = window.global::<Ui>();
        let visible = AtomicBool::new(true);
        state(&mut worker, trio(), vec![]);
        let shown = setup_json(vec![link_of(1, "ask"), link_of(2, "ask")], &[]);
        for top in [true, false] {
            if !top {
                window.invoke_open_device(id(1).as_str().into());
                drain(&mut worker, &mut actions).await;
            }
            worker
                .refresh(linked_status(&worker, &shown), &visible)
                .await?;
            flush_ui();
            let mut newer = shown.clone();
            newer["binding_generations"][id(1).as_str()] = json!(9);
            newer["links"][0]["scope"] = json!("disabled");
            worker
                .refresh(linked_status(&worker, &newer), &visible)
                .await?;
            if top {
                ui.invoke_top_action(true);
            } else {
                ui.invoke_link_device(id(1).as_str().into());
            }
            drain(&mut worker, &mut actions).await;
            assert_ne!(worker.remembered.offer.as_deref(), Some(setup::LINKED));
            assert_eq!(ui.get_dialog(), Dialog::None);
        }
        let links = sent(&mock, "link_existing_device");
        assert_eq!(links.len(), 2);
        for link in links {
            assert_eq!(
                link,
                json!({"command":"link_existing_device", "device":id(1),
                "binding_generation":3, "expected_scope":"ask"})
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn consumed_invitation_completes_existing_device_and_focus_reprobes_clipboard()
    -> Result<()> {
        if in_offscreen_child("ui::window::tests::consumed_invitation_completes_existing_device_and_focus_reprobes_clipboard", &[]).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let _mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        state(&mut worker, trio(), vec![]);
        window.invoke_open_add(0);
        drain(&mut worker, &mut actions).await;
        let token = pneu_proto::PairingUri::parse(&sample_uri())?.token;
        worker.phone_token = Some(token.clone());
        worker.snapshot.pairing_completion = Some(super::super::PairingCompletion {
            token,
            device: id(2),
        });
        let probes = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = probes.clone();
        window
            .global::<Ui>()
            .on_probed(move |_| count.set(count.get() + 1));
        worker.act(Action::Focused).await;
        flush_ui();
        assert_eq!(probes.get(), 1);
        worker
            .refresh(status_value(&worker), &AtomicBool::new(true))
            .await?;
        flush_ui();
        assert_eq!(
            worker.paired.as_ref().and_then(|p| p.device.as_deref()),
            Some(id(2).as_str())
        );
        assert_eq!(window.global::<Ui>().get_paired(), "MacBook Air");
        let mut lost = peers(&worker.snapshot, &[]);
        for peer in &mut lost {
            peer.health = Health::Lost;
        }
        let frame = worker.frame(
            &Config::default(),
            &lost,
            None,
            None,
            false,
            crate::now_ms(),
        );
        assert!(frame.pair_again);
        apply(&window, frame);
        assert!(window.global::<Ui>().get_pair_again());
        let never = peers(
            &snapshot(vec![device(2, "MacBook Air", false, "disconnected", 0)]),
            &[],
        );
        assert_eq!(device_summary(&never[0]).0.title, "Not connected yet");
        Ok(())
    }

    #[test]
    fn scope_refresh_preserves_connection_presentation_and_lost_devices_offer_pair_again() {
        let mut snap = snapshot(vec![device(1, "Phone", true, "disconnected", 123)]);
        snap.refreshing_devices = vec![id(1)];
        let refreshed = peers(&snap, &[]);
        assert!(refreshed[0].connected());
        assert!(!unreachable(&refreshed));
        assert_eq!(device_summary(&refreshed[0]).0.title, "Connected");
        snap.refreshing_devices.clear();
        assert!(!peers(&snap, &[])[0].connected());
        snap.devices[0].state = "unauthorized".into();
        snap.refreshing_devices = vec![id(1)];
        assert!(
            peers(&snap, &[])[0].lost(),
            "a refresh never hides pairing loss"
        );
    }

    #[tokio::test]
    async fn first_run_phone_completion_survives_renewal_and_linked_result_stays_readable()
    -> Result<()> {
        if in_offscreen_child("ui::window::tests::first_run_phone_completion_survives_renewal_and_linked_result_stays_readable", &[]).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        state(&mut worker, vec![], vec![]);
        window.invoke_open_add(0);
        drain(&mut worker, &mut actions).await;
        let invitation = pneu_proto::PairingUri::parse(&sample_uri())?;
        let mut initial = status_value(&worker);
        initial["invitation"] = serde_json::to_value(&invitation)?;
        let visible = AtomicBool::new(true);
        worker.refresh(initial, &visible).await?;
        flush_ui();
        assert_eq!(worker.phone_token.as_ref(), Some(&invitation.token));
        assert!(
            window.global::<Ui>().get_link_on(),
            "first run defaults to linking"
        );
        state(&mut worker, trio(), vec![]);
        worker.snapshot.pairing_completion = Some(super::super::PairingCompletion {
            token: invitation.token.clone(),
            device: id(1),
        });
        let introduced = with(
            with(link_of(2, "enabled"), "introduced_by", json!(id(1))),
            "ready",
            json!(false),
        );
        let mut setup = setup_json(
            vec![link_of(1, "enabled"), introduced],
            &[(1, "Galaxy S25+"), (2, "MacBook Air")],
        );
        let mut next = linked_status(&worker, &setup);
        let mut renewed = invitation.clone();
        renewed.token = pneu_proto::Hash::new("ef".repeat(32))?;
        next["invitation"] = serde_json::to_value(&renewed)?;
        worker.refresh(next, &visible).await?;
        flush_ui();
        assert_eq!(window.global::<Ui>().get_paired(), "Galaxy S25+");
        assert!(worker.paired.is_some());
        assert!(
            sent(&mock, "pairing_scope")
                .iter()
                .all(|r| r["enabled"] == true)
        );
        // No READY yet: the old 1.5-second deadline must not dismiss the page.
        worker.paired.as_mut().unwrap().at = Instant::now() - Duration::from_secs(2);
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        assert!(worker.paired.is_some());
        // READY in separate batches updates in place and keeps the full list readable.
        worker.paired.as_mut().unwrap().at = Instant::now() - Duration::from_secs(4);
        setup["links"][1]["ready"] = json!(true);
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        assert_eq!(worker.paired_detail().0, "Also linked: MacBook Air");
        assert!(
            worker.paired.as_ref().unwrap().linked_at.unwrap().elapsed()
                < Duration::from_millis(500)
        );
        let second = with(
            with(link_of(3, "enabled"), "introduced_by", json!(id(1))),
            "ready",
            json!(false),
        );
        setup["links"].as_array_mut().unwrap().push(second);
        setup["roster"]
            .as_array_mut()
            .unwrap()
            .push(json!({"device_id": id(3), "fingerprint": "03".repeat(32), "name": "Office-PC", "status": "member"}));
        worker.paired.as_mut().unwrap().linked_at =
            Some(Instant::now() - Duration::from_millis(3100));
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        assert!(
            worker.paired.is_some(),
            "a partial list waits for late introductions"
        );
        worker.paired.as_mut().unwrap().at = Instant::now() - Duration::from_millis(4900);
        setup["links"][2]["ready"] = json!(true);
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        assert_eq!(
            worker.paired_detail().0,
            "Also linked: MacBook Air and Office-PC"
        );
        worker.paired.as_mut().unwrap().at = Instant::now() - Duration::from_millis(5100);
        worker.paired.as_mut().unwrap().linked_at =
            Some(Instant::now() - Duration::from_millis(2900));
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        assert!(
            worker.paired.is_some(),
            "Also linked is readable for three seconds"
        );
        worker.paired.as_mut().unwrap().linked_at =
            Some(Instant::now() - Duration::from_millis(3100));
        worker
            .refresh(linked_status(&worker, &setup), &visible)
            .await?;
        flush_ui();
        assert!(worker.paired.is_none());
        assert_eq!(worker.view().page, Page::Device);
        Ok(())
    }

    #[tokio::test]
    async fn setup_shared_readmission_silent_keeper_vector() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::setup_shared_readmission_silent_keeper_vector",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _actions) = model_worker(&window, dir.path());
        let vectors: Value = serde_json::from_str(include_str!(
            "../../../../protocol-vectors/readmission.json"
        ))?;
        let scenario = vectors["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "silent-keeper-neutral-status")
            .unwrap();
        state(
            &mut worker,
            vec![device(1, "Phone", true, "disconnected", 0)],
            vec![],
        );
        linking(
            &mut worker,
            vec![with(
                with(link_of(1, "enabled"), "introduced_by", json!(id(4))),
                "hello_seen",
                json!(false),
            )],
            &[(1, "Phone")],
        );
        worker.setup.links[0].ready = false;
        for step in scenario["steps"].as_array().unwrap() {
            if let Some(action) = step["local_action"].as_str() {
                match action {
                    "arrange-introduced-phone-keeps-B-removed-and-stays-silent"
                    | "observe-without-authenticated-refusal" => {}
                    "receive-authenticated-unauthorized-from-A" => {
                        worker.snapshot.devices[0].state = "unauthorized".into()
                    }
                    "authenticate-A-ready" => {
                        worker.snapshot.devices[0].state = "connected".into();
                        worker.setup.links[0].ready = true;
                        worker.setup.links[0].hello_seen = true;
                    }
                    _ => panic!("unhandled action {action}"),
                }
            } else {
                assert_eq!(step["receive"], "later-introduction-claimed");
            }
            let peers = peers(&worker.snapshot, &[]);
            let frame = worker.frame(
                &Config::default(),
                &peers,
                Some(&peers[0]),
                None,
                false,
                crate::now_ms(),
            );
            for (key, expected) in step["expect"].as_object().into_iter().flatten() {
                match key.as_str() {
                    "remote_refusal_claimed" => {
                        assert_eq!(peers[0].lost(), expected.as_bool().unwrap())
                    }
                    "remote_keep_removed_claimed" => {
                        assert_eq!(expected, false);
                        assert!(!frame.device_summary.detail.contains("Keep removed"));
                    }
                    "status" => {
                        assert_eq!(expected, "neutral");
                        assert_eq!(frame.devices[0].status_tone, Tone::Neutral);
                    }
                    "status_example" => {
                        assert_eq!(frame.device_summary.title, "Waiting for Phone");
                        assert_eq!(
                            frame.device_summary.detail,
                            "Open Pneu on Phone, or pair with it directly"
                        );
                    }
                    "also_linked" => {
                        assert_eq!(worker.setup.links[0].ready, expected.as_bool().unwrap())
                    }
                    "diagnostic" => {
                        assert_eq!(
                            frame.device_summary.detail,
                            "Allow it on Phone, or pair with it directly"
                        );
                        assert!(
                            expected
                                .as_str()
                                .unwrap()
                                .ends_with("Pair with it directly")
                        );
                    }
                    "trust_A_retained" => assert_eq!(
                        !worker.snapshot.devices.is_empty(),
                        expected.as_bool().unwrap()
                    ),
                    "refusal_status_cleared" => {
                        assert_eq!(expected, true);
                        assert!(peers[0].connected());
                        assert!(!frame.device_summary.detail.contains("doesn't recognize"));
                    }
                    _ => panic!("unhandled assertion {key}"),
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn removed_self_with_offline_phone_and_ambiguous_refusal_have_distinct_presentations()
    -> Result<()> {
        if in_offscreen_child("ui::window::tests::removed_self_with_offline_phone_and_ambiguous_refusal_have_distinct_presentations", &[]).await? { return Ok(()); }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _actions) = model_worker(&window, dir.path());
        state(
            &mut worker,
            vec![
                device(1, "Phone", true, "disconnected", 100),
                device(2, "MacBook Air", false, "unauthorized", 100),
            ],
            vec![],
        );
        worker.snapshot.setup_refusal_since = Some(200);
        linking(
            &mut worker,
            vec![
                link_of(1, "enabled"),
                with(link_of(2, "enabled"), "introduced_by", json!(id(1))),
            ],
            &[(1, "Phone"), (2, "MacBook Air")],
        );
        let frame = |w: &Worker| {
            let peers = peers(&w.snapshot, &[]);
            w.frame(
                &Config::default(),
                &peers,
                peers.iter().find(|p| p.id == id(2).as_str()),
                None,
                false,
                crate::now_ms(),
            )
        };
        let removed = frame(&worker);
        assert!(removed.pair_again);
        assert_eq!(removed.summary.title, "Pairing lost");
        // The phone has connected since the refusal: the computer may still be deciding.
        worker.snapshot.devices[0].last_seen = 300;
        let waiting = frame(&worker);
        assert!(!waiting.pair_again);
        let row = waiting
            .devices
            .iter()
            .find(|d| d.id == id(2).as_str())
            .unwrap();
        assert_eq!(row.status, "Not allowed on MacBook Air yet");
        assert_eq!(row.tone, Tone::Warn);
        assert_eq!(row.status_tone, Tone::Warn);
        assert_eq!(waiting.device.status, row.status);
        assert_eq!(waiting.device_summary.link.tiles[0].tone, Tone::Warn);
        assert_eq!(waiting.device_summary.link.tube, Tube::Dashed);
        assert_eq!(waiting.device_action, "pair-warning");
        assert_eq!(
            waiting.device_summary.title,
            "MacBook Air hasn't allowed this computer"
        );
        assert_eq!(
            waiting.device_summary.detail,
            "Allow it on MacBook Air, or pair with it directly"
        );
        assert_eq!(waiting.summary.title, "Waiting for your devices");
        worker.snapshot.devices.retain(|d| d.role != "phone");
        worker.snapshot.setup_refusal_since = None;
        let ambiguous = frame(&worker);
        assert!(!ambiguous.pair_again);
        assert_eq!(ambiguous.summary.title, "Waiting for your devices");
        worker.setup.links[1].introduced_by = None;
        let direct = frame(&worker);
        assert_eq!(direct.device_summary.title, "Pairing lost");
        assert_eq!(direct.device_action, "pair");
        assert_eq!(direct.device.status_tone, Tone::Danger);
        worker.setup.links[1].introduced_by = Some(id(1).to_string());
        worker.snapshot.devices[0].state = "fingerprint_mismatch".into();
        let mismatch = frame(&worker);
        assert_eq!(mismatch.device_summary.title, "Pairing lost");
        assert_eq!(mismatch.device_action, "pair");
        assert_eq!(mismatch.device.status_tone, Tone::Danger);
        Ok(())
    }

    #[tokio::test]
    async fn introduced_waiting_page_offers_direct_pairing() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::introduced_waiting_page_offers_direct_pairing",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, _actions) = model_worker(&window, dir.path());
        state(
            &mut worker,
            vec![device(1, "sdk_gphone16k_x86_64", true, "disconnected", 0)],
            vec![],
        );
        linking(
            &mut worker,
            vec![with(
                with(link_of(1, "enabled"), "introduced_by", json!(id(2))),
                "ready",
                json!(false),
            )],
            &[(1, "sdk_gphone16k_x86_64")],
        );
        let peers = peers(&worker.snapshot, &[]);
        let frame = worker.frame(
            &Config::default(),
            &peers,
            Some(&peers[0]),
            None,
            false,
            crate::now_ms(),
        );
        assert_eq!(
            frame.device_summary.detail,
            "Open Pneu on sdk_gphone16k_x86_64, or pair with it directly"
        );
        assert_eq!(frame.device_action, "pair-warning");
        window.invoke_open_add(if frame.device.phone { 0 } else { 1 });
        assert_eq!(window.global::<Ui>().get_page(), Page::Add);
        assert_eq!(window.global::<Ui>().get_add_tab(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn dont_allow_submits_every_displayed_asker_in_one_batch() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::dont_allow_submits_every_displayed_asker_in_one_batch",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        state(&mut worker, trio(), vec![]);
        let setup = setup_json(
            vec![
                with(link_of(1, "ask"), "prompt", json!(true)),
                with(link_of(2, "ask"), "prompt", json!(true)),
            ],
            &[],
        );
        worker
            .refresh(linked_status(&worker, &setup), &AtomicBool::new(true))
            .await?;
        flush_ui();
        window.global::<Ui>().invoke_top_action(false);
        drain(&mut worker, &mut actions).await;
        let batches = sent(&mock, "setup_decisions");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0]["allow"], false);
        assert_eq!(batches[0]["requests"].as_array().unwrap().len(), 2);
        assert!(
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| r["command"] != "setup_decision")
        );
        Ok(())
    }

    #[tokio::test]
    async fn introduced_peer_does_not_complete_pairing_but_is_also_linked() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::introduced_peer_does_not_complete_pairing_but_is_also_linked",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        let _mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        let visible = AtomicBool::new(true);
        state(
            &mut worker,
            vec![device(1, "Galaxy S25+", true, "connected", 0)],
            vec![],
        );
        let initial = setup_json(vec![link_of(1, "enabled")], &[(1, "Galaxy S25+")]);
        worker
            .refresh(linked_status(&worker, &initial), &visible)
            .await?;
        flush_ui();
        window.invoke_open_add(1);
        drain(&mut worker, &mut actions).await;
        let introduced = with(
            with(
                with(link_of(2, "enabled"), "introduced_by", json!(id(1))),
                "ready",
                json!(false),
            ),
            "hello_seen",
            json!(false),
        );
        let mut roster = setup_json(
            vec![link_of(1, "enabled"), introduced.clone()],
            &[(1, "Galaxy S25+"), (2, "MacBook Air")],
        );
        state(&mut worker, trio(), vec![]);
        worker
            .refresh(linked_status(&worker, &roster), &visible)
            .await?;
        flush_ui();
        assert!(
            worker.paired.is_none(),
            "introduction is not a pairing result"
        );
        let mut devices = trio();
        devices.push(device(3, "New computer", false, "connected", 0));
        state(&mut worker, devices, vec![]);
        let token = pneu_proto::PairingUri::parse(&sample_uri())?.token;
        worker.code.token = Some(token.clone());
        worker.snapshot.pairing_completion = Some(super::super::PairingCompletion {
            token,
            device: id(3),
        });
        roster = setup_json(
            vec![link_of(1, "enabled"), introduced, link_of(3, "enabled")],
            &[(1, "Galaxy S25+"), (2, "MacBook Air"), (3, "New computer")],
        );
        worker
            .refresh(linked_status(&worker, &roster), &visible)
            .await?;
        flush_ui();
        assert_eq!(
            worker.paired.as_ref().and_then(|p| p.device.as_deref()),
            Some(id(3).as_str())
        );
        assert_eq!(worker.paired_detail(), (String::new(), false));
        roster["links"][1]["hello_seen"] = json!(true);
        worker
            .refresh(linked_status(&worker, &roster), &visible)
            .await?;
        assert_eq!(worker.paired_detail(), (String::new(), false));
        roster["links"][1]["ready"] = json!(true);
        worker
            .refresh(linked_status(&worker, &roster), &visible)
            .await?;
        assert_eq!(
            worker.paired_detail(),
            ("Also linked: MacBook Air".into(), false)
        );
        assert!(
            worker.setup.notices.is_empty(),
            "done screen uses the roster"
        );
        Ok(())
    }

    #[tokio::test]
    async fn removed_computer_and_joined_remove_request() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::removed_computer_and_joined_remove_request",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        std::fs::create_dir_all(&worker.paths.data)?;
        let _mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        let ui = window.global::<Ui>();
        let visible = AtomicBool::new(true);

        // Removed with nothing left: back on Add Device, saying so once.
        state(&mut worker, Vec::new(), Vec::new());
        let mut withdrawn = setup_json(vec![], &[]);
        withdrawn["withdrawn"] = json!(true);
        withdrawn["generation"] = json!(4);
        for _ in 0..2 {
            worker
                .refresh(linked_status(&worker, &withdrawn), &visible)
                .await?;
            drain(&mut worker, &mut actions).await;
        }
        assert_eq!(worker.view().page, Page::Add);
        assert_eq!(ui.get_dialog(), Dialog::Message);
        let message = ui.get_message();
        assert_eq!(
            message.title,
            if cfg!(target_os = "macos") {
                "This Mac was removed from your devices"
            } else {
                "This computer was removed from your devices"
            }
        );
        assert_eq!(
            (message.text.as_str(), message.primary.as_str()),
            ("", "OK")
        );
        ui.set_dialog(Dialog::None);
        worker
            .refresh(linked_status(&worker, &withdrawn), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_dialog(), Dialog::None, "said once per window");
        ui.invoke_message_action("removed-ok".into(), true);
        drain(&mut worker, &mut actions).await;
        let mut reopened = Worker::new(
            worker.paths.clone(),
            window.as_weak(),
            worker.view.clone(),
            0,
        );
        assert_eq!(reopened.remembered.removed, Some(4));
        state(&mut reopened, Vec::new(), Vec::new());
        reopened
            .refresh(linked_status(&reopened, &withdrawn), &visible)
            .await?;
        flush_ui();
        assert_eq!(ui.get_dialog(), Dialog::None, "said once across windows");

        // A joined notification's Remove… opens that page with its confirmation.
        state(&mut worker, trio(), Vec::new());
        let setup = setup_json(
            vec![
                link_of(1, "enabled"),
                with(
                    link_of(2, "enabled"),
                    "introducer_name",
                    json!("Galaxy S25+"),
                ),
            ],
            &[(1, "Galaxy S25+"), (2, "MacBook Air")],
        );
        let mut status = linked_status(&worker, &setup);
        status["window_request"] = json!(2);
        status["remove_device"] = json!(id(2));
        for fresh in [false, true] {
            if fresh {
                ui.set_dialog(Dialog::None);
                window.invoke_go(Page::Main);
                drain(&mut worker, &mut actions).await;
                worker = Worker::new(
                    worker.paths.clone(),
                    window.as_weak(),
                    worker.view.clone(),
                    2,
                );
                state(&mut worker, trio(), vec![]);
                worker.status = Value::Null;
            }
            worker.refresh(status.clone(), &visible).await?;
            drain(&mut worker, &mut actions).await;
            assert_eq!(worker.view().page, Page::Device, "fresh={fresh}");
            assert_eq!(worker.view().device, id(2).as_str());
            worker.refresh(status.clone(), &visible).await?;
            drain(&mut worker, &mut actions).await;
            assert_eq!(ui.get_dialog(), Dialog::Remove);
            assert_eq!(ui.get_remove_name(), "MacBook Air");
            assert_eq!(
                worker.removal.as_ref().map(|r| r.device.as_str()),
                Some(id(2).as_str())
            );
        }
        Ok(())
    }

    /// Every page and popup, rendered with the software renderer. Each must
    /// stay 400 px wide and at most 720 px tall. With PNEU_WINDOW_SHOTS set to
    /// a folder, the renders are also saved there as PNG files.
    #[tokio::test]
    async fn every_window_state_renders_within_bounds() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::every_window_state_renders_within_bounds",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        let surface = headless()?;
        let window = PneuWindow::new()?;
        connect(
            &window,
            &tokio::sync::mpsc::unbounded_channel().0,
            &Arc::new(Mutex::new(View::default())),
        );
        let scale = std::env::var("PNEU_WINDOW_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0f32);
        surface.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        window.show()?;
        let shots = std::env::var_os("PNEU_WINDOW_SHOTS").map(PathBuf::from);
        let theme = std::env::var("PNEU_WINDOW_THEME").unwrap_or_else(|_| "default".into());
        match theme.as_str() {
            "light" => window
                .global::<Palette>()
                .set_color_scheme(slint::language::ColorScheme::Light),
            "dark" => window
                .global::<Palette>()
                .set_color_scheme(slint::language::ColorScheme::Dark),
            "default" => (),
            _ => anyhow::bail!("unknown window theme: {theme}"),
        }
        // Qt's NativePalette reads QPalette colors independently of its scheme.
        // Supply both colors for the headless theme fixture, never the session.
        if theme != "default" {
            let dark = theme == "dark";
            window.global::<Pneu>().set_text(if dark {
                slint::Color::from_rgb_u8(239, 240, 241)
            } else {
                slint::Color::from_rgb_u8(35, 38, 41)
            });
            window.global::<Pneu>().set_background(if dark {
                slint::Color::from_rgb_u8(27, 30, 32)
            } else {
                slint::Color::from_rgb_u8(239, 240, 241)
            });
        }
        let mut rendered = 0;
        for scene in scenes() {
            let paths = Paths {
                data: "/nonexistent".into(),
                cache: "/nonexistent".into(),
                config: "/nonexistent".into(),
                socket: "/nonexistent".into(),
            };
            let mut worker = Worker::new(
                paths,
                window.as_weak(),
                Arc::new(Mutex::new(View::default())),
                0,
            );
            let ui = window.global::<Ui>();
            ui.set_dialog(Dialog::None);
            ui.set_page(Page::Main);
            // Tear down the previous scene before constructing the next one's
            // autofocus controls; captures must not inherit pending focus timers.
            let _ = window.get_content_height();
            advance_ui();
            ui.set_is_mac(false);
            ui.set_history_copied("".into());
            ui.set_history_query("".into());
            ui.set_download(false);
            ui.set_qr(slint::Image::from_rgba8(qr_pixels(&sample_uri())?));
            ui.set_code_qr(slint::Image::from_rgba8(qr_pixels(&sample_uri())?));
            (scene.setup)(&mut worker);
            let peers = peers(&worker.snapshot, &worker.transfers);
            let config: Config =
                serde_json::from_value(worker.status["config"].clone()).unwrap_or_default();
            let device = peers.iter().find(|p| p.id == worker.view().device).cloned();
            let invitation = serde_json::from_value::<Option<pneu_proto::PairingUri>>(
                worker.status["invitation"].clone(),
            )
            .unwrap_or(None);
            let frame = worker.frame(
                &config,
                &peers,
                device.as_ref(),
                invitation.as_ref(),
                peers.is_empty(),
                1_800_000_000_000 - 288_000,
            );
            apply(&window, frame);
            (scene.ui)(&window);
            // Evaluate the layout before advancing timers: conditional controls
            // must exist before their popup/search autofocus timers can run.
            let _ = window.get_content_height();
            // Popups focus their default button on a short timer.
            slint::platform::update_timers_and_animations();
            std::thread::sleep(Duration::from_millis(5));
            slint::platform::update_timers_and_animations();
            let height = window.get_content_height();
            assert!(
                height <= 720.0 && height > 100.0,
                "{}: {height}px",
                scene.name
            );
            if scene.name.contains("-download") {
                // Swapping the codes must not resize the window.
                ui.set_download(false);
                let pairing = window.get_content_height();
                ui.set_download(true);
                assert_eq!(pairing, height, "{}: pairing code height", scene.name);
            }
            let size = slint::LogicalSize::new(400.0, height).to_physical(scale);
            surface.set_size(size);
            if scene.name.ends_with("-copied") && !scene.name.contains("history") {
                // Click the address, which sits just above Pairing Code.
                let position =
                    slint::LogicalPosition::new(200.0, height - 16.0 - 24.0 - 4.0 - 12.0);
                let button = slint::platform::PointerEventButton::Left;
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                        position,
                        button,
                    });
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                        position,
                        button,
                    });
                assert_eq!(*COPIED.lock().unwrap(), DOWNLOAD_PAGE, "{}", scene.name);
            }
            surface.request_redraw();
            let mut pixels = vec![
                    slint::platform::software_renderer::PremultipliedRgbaColor::default();
                    (size.width * size.height) as usize
                ];
            surface.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, size.width as usize);
            });
            // The first draw creates root-level popup controls. Capture their
            // settled autofocus state, as the next real event-loop frame does.
            advance_ui();
            surface.request_redraw();
            surface.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, size.width as usize);
            });
            if scene.name == "89-history-searching" {
                assert_eq!(ui.get_history_query(), "pneu");
                assert_eq!(ui.get_pinned_history().row_count(), 1);
                assert_eq!(ui.get_recent_history().row_count(), 0);
            }
            if scene.name == "90-history-no-matches" {
                assert_eq!(ui.get_history_query(), "invoice");
                assert_eq!(
                    ui.get_pinned_history().row_count() + ui.get_recent_history().row_count(),
                    0
                );
            }
            if matches!(ui.get_dialog(), Dialog::HistoryClear | Dialog::HistoryOff) {
                assert!(
                    pixels
                        .iter()
                        .any(|p| (p.red, p.green, p.blue) == (79, 91, 255)),
                    "{}: Cancel must show its accent focus ring",
                    scene.name
                );
            }
            if scene.name.starts_with("17-add-") {
                // At the default size, the Phone page must keep its bottom
                // padding and must not paint a scrollbar in the right gutter.
                let right = size.width as usize - (5.0 * scale) as usize;
                let stride = size.width as usize;
                let rgb = |p: &slint::platform::software_renderer::PremultipliedRgbaColor| {
                    (p.red, p.green, p.blue)
                };
                assert!(
                    pixels
                        .chunks_exact(stride)
                        .all(|row| rgb(&row[0]) == rgb(&row[right])),
                    "{}: unexpected scrollbar at {scale}x",
                    scene.name
                );
                let bottom = (16.0 * scale) as usize;
                assert!(
                    pixels
                        .chunks_exact(stride)
                        .rev()
                        .take(bottom)
                        .all(|row| { row.iter().all(|pixel| rgb(pixel) == rgb(&row[0])) }),
                    "{}: missing 16px bottom padding at {scale}x",
                    scene.name
                );
            }
            let segment_count = match scene.name {
                "17-add-phone" => Some(2),
                "38-settings" | "38-settings-manual" | "39-settings-off" => Some(3),
                _ => None,
            };
            if let Some(count) = segment_count {
                // Measure the painted selection, so label/font metrics cannot
                // silently widen one segment or shift the others.
                let fill = window.global::<Pneu>().get_accent_soft();
                let columns: Vec<_> = pixels
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        (p.red, p.green, p.blue) == (fill.red(), fill.green(), fill.blue())
                    })
                    .map(|(i, _)| i % size.width as usize)
                    .collect();
                let painted = columns.iter().max().context("selected segment")?
                    - columns.iter().min().context("selected segment")?
                    + 1;
                let inset = if count == 3 { 28.0 } else { 0.0 };
                let expected =
                    (400.0 - 32.0 - inset - 4.0 - 2.0 * (count - 1) as f32) / count as f32 * scale;
                assert!(
                    (painted as f32 - expected).abs() <= 2.0 * scale,
                    "{}: selection is {painted}px, expected {expected}px at {scale}x",
                    scene.name
                );
            }
            rendered += 1;
            if let Some(folder) = &shots {
                let bytes: Vec<u8> = pixels
                    .iter()
                    .flat_map(|p| [p.red, p.green, p.blue, 255])
                    .collect();
                image::RgbaImage::from_raw(size.width, size.height, bytes)
                    .context("frame")?
                    .save(folder.join(format!("{}-{theme}.png", scene.name)))?;
            }
        }
        assert_eq!(rendered, scenes().len());
        Ok(())
    }

    fn sample_uri() -> String {
        pneu_proto::PairingUri {
            version: 1,
            name: "MJStanton-Desktop".into(),
            fingerprint: pneu_proto::Hash::new("ab".repeat(32)).unwrap(),
            addresses: vec!["100.67.144.28:47821".into(), "192.168.1.20:47821".into()],
            token: pneu_proto::Hash::new("cd".repeat(32)).unwrap(),
            expires_at: 1_800_000_000_000,
        }
        .build()
        .unwrap()
    }

    struct Scene {
        name: &'static str,
        setup: Box<dyn Fn(&mut Worker)>,
        ui: Box<dyn Fn(&PneuWindow)>,
    }
    fn scene(
        name: &'static str,
        setup: impl Fn(&mut Worker) + 'static,
        ui: impl Fn(&PneuWindow) + 'static,
    ) -> Scene {
        Scene {
            name,
            setup: Box::new(setup),
            ui: Box::new(ui),
        }
    }
    fn state(
        w: &mut Worker,
        devices: Vec<crate::core::mesh::DeviceSnapshot>,
        transfers: Vec<Transfer>,
    ) {
        w.snapshot = snapshot(devices);
        if w.snapshot
            .devices
            .first()
            .is_some_and(|d| d.role == "phone" && d.state == "connected")
        {
            w.snapshot.device = Some(id(1));
            w.snapshot.phone.route = "Local network".into();
        }
        w.transfers = transfers;
        let config = Config {
            name: "MJStanton-Desktop".into(),
            save_path: PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join("Pictures/Pneu"),
            ..Default::default()
        };
        w.status = json!({"config": config});
    }
    fn open(w: &mut Worker, page: Page, device: u8, tab: i32) {
        *w.view.lock().unwrap() = View {
            page,
            device: if device == 0 {
                String::new()
            } else {
                id(device).as_str().into()
            },
            add_tab: tab,
            ..Default::default()
        };
    }
    fn go(page: Page, device: u8, tab: i32) -> impl Fn(&PneuWindow) {
        move |w: &PneuWindow| {
            let ui = w.global::<Ui>();
            ui.set_page(page);
            ui.set_add_tab(tab);
            ui.set_device_id(if device == 0 {
                SharedString::new()
            } else {
                id(device).as_str().into()
            });
            ui.set_motion(false);
        }
    }
    fn message(w: &PneuWindow, title: &str, text: &str, tile: i32, secondary: &str, primary: &str) {
        let ui = w.global::<Ui>();
        ui.set_page(Page::Add);
        ui.set_add_tab(1);
        ui.set_motion(false);
        ui.set_message(Message {
            title: title.into(),
            text: text.into(),
            tile,
            primary: primary.into(),
            secondary: secondary.into(),
            action: "ok".into(),
        });
        ui.set_dialog(Dialog::Message);
    }
    fn recent_items() -> Vec<Transfer> {
        let mut shot = transfer("r1", 1, "Screenshot_20261022.png", "sent", 1, 1);
        shot.direction = Some("incoming".into());
        shot.finished_at = Some(crate::now_ms() - 60_000);
        let mut report = transfer("r2", 1, "report.pdf", "sent", 1, 1);
        report.finished_at = Some(crate::now_ms() - 360_000);
        let mut report2 = transfer("r2", 2, "report.pdf", "sent", 1, 1);
        report2.finished_at = report.finished_at;
        let mut mac = transfer(
            "r3",
            2,
            "Screenshot 2026-10-21 at 10.38.12.png",
            "sent",
            1,
            1,
        );
        mac.direction = Some("incoming".into());
        mac.finished_at = Some(crate::now_ms() - 180_000);
        vec![shot, report, report2, mac]
    }
    fn two() -> Vec<crate::core::mesh::DeviceSnapshot> {
        let mut mac = device(2, "MacBook Air", false, "connected", 0);
        mac.route = Some("Tailscale".into());
        vec![device(1, "Galaxy S25+", true, "connected", 0), mac]
    }
    fn many() -> Vec<crate::core::mesh::DeviceSnapshot> {
        let mut list = vec![
            device(1, "Galaxy S25+", true, "connected", 0),
            device(2, "MacBook Air", false, "connected", 0),
            device(
                3,
                "Matthew’s MacBook Pro (Studio, 16-inch) with a very long name",
                false,
                "connected",
                0,
            ),
            device(4, "Living-Room-HTPC", false, "connected", 0),
            device(
                5,
                "Office-PC",
                false,
                "disconnected",
                crate::now_ms() - 86_400_000,
            ),
            device(6, "Old Laptop", false, "update_required", 0),
            device(7, "Pixel 6a", true, "unauthorized", 0),
        ];
        list.extend(
            (8..=16).map(|n| device(n, &format!("Lab-PC-{n:02}"), false, "disconnected", 0)),
        );
        list
    }
    fn picks(w: &PneuWindow, devices: &[(u8, &str, bool, bool, bool, &str)]) {
        let picks: Vec<PickItem> = devices
            .iter()
            .map(|(n, name, phone, connected, eligible, status)| PickItem {
                id: id(*n).as_str().into(),
                name: (*name).into(),
                phone: *phone,
                tone: if *connected {
                    Tone::Accent
                } else if *eligible {
                    Tone::Neutral
                } else {
                    Tone::Danger
                },
                connected: *connected,
                eligible: *eligible,
                status: (*status).into(),
                checked: *connected && *eligible,
            })
            .collect();
        let ui = w.global::<Ui>();
        ui.set_picker_count(picks.iter().filter(|p| p.checked).count() as i32);
        ui.set_picks(ModelRc::new(VecModel::from(picks)));
        ui.set_dialog(Dialog::Picker);
    }
    fn offline_phone(w: &mut Worker) {
        let mut phone = device(1, "Galaxy S25+", true, "disconnected", 0);
        phone.last_seen = crate::now_ms() - 7_200_000;
        let waiting = transfer("w", 1, "notes.md", "waiting_for_device", 0, 9);
        state(w, vec![phone], vec![waiting]);
    }
    fn offline_devices(w: &mut Worker) {
        let mut list = vec![
            transfer("w", 1, "notes.md", "waiting_for_device", 0, 9),
            transfer("w", 2, "notes.md", "waiting_for_device", 0, 9),
        ];
        list.extend(recent_items()[..1].to_vec());
        state(
            w,
            vec![
                device(
                    1,
                    "Galaxy S25+",
                    true,
                    "disconnected",
                    crate::now_ms() - 7_200_000,
                ),
                device(
                    2,
                    "MacBook Air",
                    false,
                    "disconnected",
                    crate::now_ms() - 9_000_000,
                ),
            ],
            list,
        );
    }
    fn help(w: &PneuWindow) {
        go(Page::Main, 0, 0)(w);
        w.global::<Ui>().set_dialog(Dialog::Help);
    }
    fn download(tab: i32, mac: bool) -> impl Fn(&PneuWindow) {
        move |w: &PneuWindow| {
            go(Page::Add, 0, tab)(w);
            w.global::<Ui>().set_is_mac(mac);
            w.global::<Ui>().set_download(true);
        }
    }
    fn scenes() -> Vec<Scene> {
        let peer = |device: u8, pending: bool| {
            let (me, it) = (id(9), id(device));
            json!({"can_sync": true, "settings": [
                {"key": format!("{me}/{it}/clipboard"), "enabled": true, "pending": false},
                {"key": format!("{it}/{me}/clipboard"), "enabled": !pending, "pending": pending},
            ]})
        };
        let phone_settings = || {
            let (me, it) = (id(9), id(1));
            json!({"can_sync": true, "settings": [
                {"key": format!("{me}/{it}/clipboard"), "enabled": true, "pending": false},
                {"key": format!("{it}/{me}/clipboard"), "enabled": true, "pending": false},
                {"key": format!("{it}/{me}/automatic_media"), "enabled": true, "pending": false},
            ]})
        };
        let mut sending = vec![
            transfer("s", 1, "report.pdf", "sending", 12_000_000, 40_000_000),
            transfer("s", 2, "report.pdf", "sending", 31_000_000, 40_000_000),
        ];
        sending.extend(recent_items());
        let mut list = vec![
            scene(
                "polish-one-device-recent",
                |w| {
                    let mut incoming =
                        transfer("received", 1, "Screenshot_20261101.png", "sent", 10, 10);
                    incoming.direction = Some("incoming".into());
                    incoming.finished_at = Some(crate::now_ms() - 60_000);
                    let mut outgoing =
                        transfer("sent", 1, "Quarterly report final v3.pdf", "sent", 10, 10);
                    outgoing.finished_at = Some(crate::now_ms() - 120_000);
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 0)],
                        vec![incoming, outgoing],
                    );
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "polish-share-off-device",
                move |w| {
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 0)],
                        Vec::new(),
                    );
                    w.status["config"]["clipboard_mode"] = json!("off");
                    w.peer_settings =
                        Some((id(1).as_str().into(), phone_settings(), Instant::now()));
                    open(w, Page::Device, 1, 0);
                },
                go(Page::Device, 1, 0),
            ),
            scene(
                "polish-image-too-large-banner",
                |w| {
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 0)],
                        Vec::new(),
                    );
                    w.status["config"]["clipboard_mode"] = json!("manual");
                    w.action_error = "Couldn't send the clipboard. The image is over 20 MB.".into();
                },
                go(Page::Main, 0, 0),
            ),
            device(1, "Galaxy S25+", true, "connected", 0),
            device(2, "MacBook Air", false, "connected", 0),
            device(
                3,
                "Matthew’s MacBook Pro (Studio, 16-inch) with a very long name",
                false,
                "connected",
                0,
            ),
            device(4, "Living-Room-HTPC", false, "connected", 0),
            device(
                5,
                "Office-PC",
                false,
                "disconnected",
                crate::now_ms() - 86_400_000,
            ),
            device(6, "Old Laptop", false, "update_required", 0),
            device(7, "Pixel 6a", true, "unauthorized", 0),
        ];
        list.extend(
            (8..=16).map(|n| device(n, &format!("Lab-PC-{n:02}"), false, "disconnected", 0)),
        );
        list
    }
    fn picks(w: &PneuWindow, devices: &[(u8, &str, bool, bool, bool, &str)]) {
        let picks: Vec<PickItem> = devices
            .iter()
            .map(|(n, name, phone, connected, eligible, status)| PickItem {
                id: id(*n).as_str().into(),
                name: (*name).into(),
                phone: *phone,
                tone: if *connected {
                    Tone::Accent
                } else if *eligible {
                    Tone::Neutral
                } else {
                    Tone::Danger
                },
                connected: *connected,
                eligible: *eligible,
                status: (*status).into(),
                checked: *connected && *eligible,
            })
            .collect();
        let ui = w.global::<Ui>();
        ui.set_picker_count(picks.iter().filter(|p| p.checked).count() as i32);
        ui.set_picks(ModelRc::new(VecModel::from(picks)));
        ui.set_dialog(Dialog::Picker);
    }
    fn offline_phone(w: &mut Worker) {
        let mut phone = device(1, "Galaxy S25+", true, "disconnected", 0);
        phone.last_seen = crate::now_ms() - 7_200_000;
        let waiting = transfer("w", 1, "notes.md", "waiting_for_device", 0, 9);
        state(w, vec![phone], vec![waiting]);
    }
    fn offline_devices(w: &mut Worker) {
        let mut list = vec![
            transfer("w", 1, "notes.md", "waiting_for_device", 0, 9),
            transfer("w", 2, "notes.md", "waiting_for_device", 0, 9),
        ];
        list.extend(recent_items()[..1].to_vec());
        state(
            w,
            vec![
                device(
                    1,
                    "Galaxy S25+",
                    true,
                    "disconnected",
                    crate::now_ms() - 7_200_000,
                ),
                device(
                    2,
                    "MacBook Air",
                    false,
                    "disconnected",
                    crate::now_ms() - 9_000_000,
                ),
            ],
            list,
        );
    }
    fn help(w: &PneuWindow) {
        go(Page::Main, 0, 0)(w);
        w.global::<Ui>().set_dialog(Dialog::Help);
    }
    fn download(tab: i32, mac: bool) -> impl Fn(&PneuWindow) {
        move |w: &PneuWindow| {
            go(Page::Add, 0, tab)(w);
            w.global::<Ui>().set_is_mac(mac);
            w.global::<Ui>().set_download(true);
        }
    }
    fn scenes() -> Vec<Scene> {
        let peer = |device: u8, pending: bool| {
            let (me, it) = (id(9), id(device));
            json!({"can_sync": true, "settings": [
                {"key": format!("{me}/{it}/clipboard"), "enabled": true, "pending": false},
                {"key": format!("{it}/{me}/clipboard"), "enabled": !pending, "pending": pending},
            ]})
        };
        let phone_settings = || {
            let (me, it) = (id(9), id(1));
            json!({"can_sync": true, "settings": [
                {"key": format!("{me}/{it}/clipboard"), "enabled": true, "pending": false},
                {"key": format!("{it}/{me}/clipboard"), "enabled": true, "pending": false},
                {"key": format!("{it}/{me}/automatic_media"), "enabled": true, "pending": false},
            ]})
        };
        let mut sending = vec![
            transfer("s", 1, "report.pdf", "sending", 12_000_000, 40_000_000),
            transfer("s", 2, "report.pdf", "sending", 31_000_000, 40_000_000),
        ];
        sending.extend(recent_items());
        let mut list = vec![
            scene(
                "resume-failed",
                |w| {
                    let mut row = transfer(
                        "resume",
                        1,
                        "Recording.mp4",
                        "failed",
                        1_200_000_000,
                        4_000_000_000,
                    );
                    row.error = Some("Connection lost".into());
                    state(w, two(), vec![row]);
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "resume-retry",
                |w| {
                    state(
                        w,
                        two(),
                        vec![transfer(
                            "resume",
                            1,
                            "Recording.mp4",
                            "sending",
                            1_200_000_000,
                            4_000_000_000,
                        )],
                    )
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "resume-legacy-failed",
                |w| {
                    let mut row =
                        transfer("legacy", 1, "Recording.mp4", "failed", 0, 4_000_000_000);
                    row.error = Some("Connection lost".into());
                    state(w, two(), vec![row]);
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "40-main-status-unavailable",
                |w| {
                    state(w, two(), recent_items());
                    w.status_failed(anyhow::anyhow!("IPC frame too large"));
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "41-main-file-missing",
                |w| {
                    state(w, two(), recent_items());
                    w.action_error = "The file was moved or deleted.".into();
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "01-main-connected",
                |w| state(w, two(), recent_items()),
                go(Page::Main, 0, 0),
            ),
            scene(
                "01-main-save-folder-off",
                |w| {
                    state(w, two(), recent_items());
                    w.status["config"]["save_to_folder"] = json!(false);
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "01-main-no-recent",
                |w| state(w, two(), Vec::new()),
                go(Page::Main, 0, 0),
            ),
            scene(
                "55-main-received-video",
                |w| {
                    let mut list = recent_items();
                    list[0].file_name = "Screen_Recording_20261027.MP4".into();
                    state(w, two(), list);
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "02-main-manual-clipboard",
                |w| {
                    state(w, two(), recent_items()[..2].to_vec());
                    w.status["config"]["clipboard_mode"] = json!("manual");
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "03-main-sending",
                move |w| state(w, two(), sending.clone()),
                go(Page::Main, 0, 0),
            ),
            scene(
                "04-main-receiving",
                |w| {
                    let mut incoming = transfer("in", 1, "Screen recording", "sending", 46, 100);
                    incoming.direction = Some("incoming".into());
                    let mut list = vec![incoming];
                    list.extend(recent_items()[..2].to_vec());
                    state(w, two(), list);
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "05-main-partly-failed",
                |w| {
                    let mut failed = transfer("f", 1, "budget-2027.xlsx", "failed", 0, 9);
                    failed.error = Some("DiskFull".into());
                    let mut list = vec![
                        transfer("w", 2, "report.pdf", "waiting_for_device", 0, 9),
                        failed,
                    ];
                    list.extend(recent_items()[..1].to_vec());
                    state(
                        w,
                        vec![
                            device(1, "Galaxy S25+", true, "connected", 0),
                            device(
                                2,
                                "MacBook Air",
                                false,
                                "disconnected",
                                crate::now_ms() - 3_600_000,
                            ),
                        ],
                        list,
                    );
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "06-main-nothing-connected",
                |w| {
                    let mut list = vec![
                        transfer("w", 1, "notes.md", "waiting_for_device", 0, 9),
                        transfer("w", 2, "notes.md", "waiting_for_device", 0, 9),
                    ];
                    list.extend(recent_items()[..1].to_vec());
                    state(
                        w,
                        vec![
                            device(
                                1,
                                "Galaxy S25+",
                                true,
                                "disconnected",
                                crate::now_ms() - 7_200_000,
                            ),
                            device(
                                2,
                                "MacBook Air",
                                false,
                                "disconnected",
                                crate::now_ms() - 9_000_000,
                            ),
                        ],
                        list,
                    );
                },
                go(Page::Main, 0, 0),
            ),
            scene("06-main-cant-connect", offline_devices, help),
            scene(
                "06-main-cant-connect-tailscale",
                |w| {
                    offline_devices(w);
                    w.tailscale = Some((true, Instant::now()));
                },
                help,
            ),
            scene(
                "09-main-one-device-unreachable",
                offline_phone,
                go(Page::Main, 0, 0),
            ),
            scene("09-main-cant-connect", offline_phone, help),
            scene(
                "09-main-cant-connect-tailscale",
                |w| {
                    offline_phone(w);
                    w.tailscale = Some((true, Instant::now()));
                },
                help,
            ),
            scene(
                "07-main-many-devices",
                |w| {
                    state(w, many(), recent_items());
                    w.snapshot.paused_devices = vec![id(1)];
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "08-main-cant-save",
                |w| {
                    state(w, two(), Vec::new());
                    w.snapshot.error = Some(super::super::ReceiveError::Storage {
                        destination: super::super::ReceiveDestination::SaveFolder,
                        path: "~/Pictures/Pneu".into(),
                        disk_full: false,
                    });
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "09-main-one-device",
                |w| {
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 0)],
                        Vec::new(),
                    )
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "10-main-all-lost",
                |w| {
                    state(
                        w,
                        vec![
                            device(2, "MacBook Air", false, "unauthorized", 0),
                            device(3, "Office-PC", false, "pairing_lost", 0),
                        ],
                        Vec::new(),
                    )
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "11-device-phone",
                move |w| {
                    state(w, two(), Vec::new());
                    open(w, Page::Device, 1, 0);
                    w.peer_settings =
                        Some((id(1).as_str().into(), phone_settings(), Instant::now()));
                },
                go(Page::Device, 1, 0),
            ),
            scene(
                "12-device-computer-offline",
                move |w| {
                    state(
                        w,
                        vec![
                            device(1, "Galaxy S25+", true, "connected", 0),
                            device(
                                2,
                                "MacBook Air",
                                false,
                                "disconnected",
                                crate::now_ms() - 3_600_000,
                            ),
                        ],
                        Vec::new(),
                    );
                    open(w, Page::Device, 2, 0);
                    w.peer_settings = Some((id(2).as_str().into(), peer(2, true), Instant::now()));
                },
                go(Page::Device, 2, 0),
            ),
            scene(
                "13-device-pairing-lost",
                |w| {
                    state(
                        w,
                        vec![
                            device(1, "Galaxy S25+", true, "unauthorized", 0),
                            device(2, "MacBook Air", false, "connected", 0),
                        ],
                        Vec::new(),
                    );
                    open(w, Page::Device, 1, 0);
                },
                go(Page::Device, 1, 0),
            ),
            scene(
                "14-device-update-required",
                |w| {
                    state(
                        w,
                        vec![device(2, "Old Laptop", false, "update_required", 0)],
                        Vec::new(),
                    );
                    open(w, Page::Device, 2, 0);
                    w.peer_settings = Some((
                        id(2).as_str().into(),
                        json!({"can_sync": false}),
                        Instant::now(),
                    ));
                },
                go(Page::Device, 2, 0),
            ),
            scene(
                "15-device-unpair",
                move |w| {
                    state(w, two(), Vec::new());
                    open(w, Page::Device, 2, 0);
                    w.peer_settings = Some((id(2).as_str().into(), peer(2, false), Instant::now()));
                },
                |w| {
                    go(Page::Device, 2, 0)(w);
                    w.invoke_prepare_unpair();
                },
            ),
            scene(
                "16-device-long-name",
                move |w| {
                    state(w, many(), Vec::new());
                    open(w, Page::Device, 3, 0);
                    w.peer_settings = Some((id(3).as_str().into(), peer(3, false), Instant::now()));
                },
                go(Page::Device, 3, 0),
            ),
            scene(
                "17-add-first-run-phone",
                |w| state(w, Vec::new(), Vec::new()),
                go(Page::Add, 0, 0),
            ),
            scene(
                "17-add-phone",
                |w| state(w, two(), Vec::new()),
                go(Page::Add, 0, 0),
            ),
            scene(
                "17-add-phone-loading",
                |w| state(w, two(), Vec::new()),
                |w| {
                    go(Page::Add, 0, 0)(w);
                    w.global::<Ui>().set_qr(slint::Image::default());
                },
            ),
            scene(
                "17-add-phone-large-qr",
                |w| state(w, two(), Vec::new()),
                |w| {
                    go(Page::Add, 0, 0)(w);
                    let uri = sample_uri().replace(
                        "MJStanton-Desktop",
                        "A longer desktop name for checking QR layout sizing",
                    );
                    w.global::<Ui>()
                        .set_qr(slint::Image::from_rgba8(qr_pixels(&uri).unwrap()));
                },
            ),
            scene(
                "17-add-first-run-download",
                |w| state(w, Vec::new(), Vec::new()),
                download(0, false),
            ),
            scene(
                "17-add-phone-download",
                |w| state(w, two(), Vec::new()),
                download(0, false),
            ),
            scene(
                "17-add-phone-large-qr-download",
                |w| state(w, two(), Vec::new()),
                |w| {
                    download(0, false)(w);
                    let uri = sample_uri().replace(
                        "MJStanton-Desktop",
                        "A longer desktop name for checking QR layout sizing",
                    );
                    w.global::<Ui>()
                        .set_qr(slint::Image::from_rgba8(qr_pixels(&uri).unwrap()));
                },
            ),
            scene(
                "17-add-first-run-mac",
                |w| state(w, Vec::new(), Vec::new()),
                |w| {
                    go(Page::Add, 0, 0)(w);
                    w.global::<Ui>().set_is_mac(true);
                },
            ),
            scene(
                "17-add-first-run-mac-download",
                |w| state(w, Vec::new(), Vec::new()),
                download(0, true),
            ),
            // Clicked after sizing; the 2 s Copied feedback outlasts the
            // remaining scenes, so no later one shows the download code.
            scene(
                "17-add-phone-download-copied",
                |w| state(w, two(), Vec::new()),
                download(0, false),
            ),
            scene(
                "18-add-phone-pairing",
                |w| {
                    state(w, two(), Vec::new());
                    w.snapshot.pairing = Some("Pixel 9".into());
                },
                go(Page::Add, 0, 0),
            ),
            scene(
                "19-add-phone-no-network",
                |w| {
                    state(w, two(), Vec::new());
                    w.phone_failed = true;
                },
                go(Page::Add, 0, 0),
            ),
            scene(
                "20-add-computer",
                |w| state(w, two(), Vec::new()),
                go(Page::Add, 0, 1),
            ),
            scene(
                "21-add-computer-offer",
                |w| {
                    state(w, two(), Vec::new());
                    w.join = Some(Join {
                        attempt: "a".into(),
                        uri: String::new(),
                        name: "Office-PC".into(),
                        confirmed: false,
                        cancel_requested: false,
                        offer: true,
                        link: true,
                        origin: View::default(),
                        reported: String::new(),
                    });
                },
                go(Page::Add, 0, 1),
            ),
            scene(
                "22-add-computer-joining",
                |w| {
                    state(w, two(), Vec::new());
                    w.join = Some(Join {
                        attempt: "a".into(),
                        uri: String::new(),
                        name: "Office-PC".into(),
                        confirmed: true,
                        cancel_requested: false,
                        offer: false,
                        link: true,
                        origin: View::default(),
                        reported: String::new(),
                    });
                },
                go(Page::Add, 0, 1),
            ),
            scene(
                "23-add-paired",
                |w| {
                    state(w, two(), Vec::new());
                    w.paired = Some(Paired {
                        origin: w.view(),
                        name: "MacBook Air".into(),
                        device: None,
                        at: Instant::now(),
                        linked_at: None,
                        linked: String::new(),
                    });
                },
                go(Page::Add, 0, 1),
            ),
            scene(
                "24-join-confirm",
                |w| state(w, two(), Vec::new()),
                |w| message(w, "Pair with Office-PC?", "", 1, "Cancel", "Pair"),
            ),
            scene(
                "25-join-expired",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "This code expired",
                        "Show a new one on Office-PC and try again.",
                        0,
                        "",
                        "OK",
                    )
                },
            ),
            scene(
                "26-join-already-used",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "This code was already used",
                        "Each code works once. Show a new one on Office-PC.",
                        0,
                        "",
                        "OK",
                    )
                },
            ),
            scene(
                "27-join-no-link",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "No pairing link on the clipboard",
                        "Copy the link on the other computer first.",
                        0,
                        "",
                        "OK",
                    )
                },
            ),
            scene(
                "28-join-incomplete",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "This pairing link is incomplete",
                        "Copy it again on the other computer.",
                        0,
                        "",
                        "OK",
                    )
                },
            ),
            scene(
                "29-join-own-code",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "That's this computer's own code",
                        "Use it on the other computer.",
                        0,
                        "",
                        "OK",
                    )
                },
            ),
            scene(
                "30-join-already-paired",
                |w| state(w, two(), Vec::new()),
                |w| message(w, "Already paired with MacBook Air", "", 1, "", "OK"),
            ),
            scene(
                "31-join-unreachable",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "Can't reach Office-PC",
                        "Make sure both are on the same network, or that Tailscale is connected.",
                        0,
                        "Cancel",
                        "Try Again",
                    )
                },
            ),
            scene(
                "32-join-unverified",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "Couldn't verify Office-PC",
                        "A different device answered at its address, so nothing was paired.",
                        3,
                        "",
                        "OK",
                    )
                },
            ),
            scene(
                "33-join-update-required",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "Update required",
                        "Update Pneu on both computers, then try again.",
                        0,
                        "",
                        "OK",
                    )
                },
            ),
            scene(
                "34-code-showing",
                |w| {
                    state(w, two(), Vec::new());
                    w.code.token = Some(pneu_proto::Hash::new("cd".repeat(32)).unwrap());
                    w.code.expires_at = 1_800_000_000_000;
                    w.code.uri = sample_uri();
                    w.status["invitation"] =
                        serde_json::to_value(pneu_proto::PairingUri::parse(&sample_uri()).unwrap())
                            .unwrap();
                },
                go(Page::Code, 0, 1),
            ),
            scene(
                "35-code-expired",
                |w| {
                    state(w, two(), Vec::new());
                    w.code.token = Some(pneu_proto::Hash::new("cd".repeat(32)).unwrap());
                },
                go(Page::Code, 0, 1),
            ),
            scene(
                "36-code-pairing",
                |w| {
                    state(w, two(), Vec::new());
                    w.snapshot.pairing = Some("MacBook Air".into());
                },
                go(Page::Code, 0, 1),
            ),
            scene(
                "37-code-paired",
                |w| {
                    state(w, two(), Vec::new());
                    w.paired = Some(Paired {
                        origin: w.view(),
                        name: "MacBook Air".into(),
                        device: None,
                        at: Instant::now(),
                        linked_at: None,
                        linked: String::new(),
                    });
                },
                go(Page::Code, 0, 1),
            ),
            scene(
                "38-settings",
                |w| state(w, two(), Vec::new()),
                go(Page::Settings, 0, 0),
            ),
            scene(
                "38-settings-save-folder-off",
                |w| {
                    state(w, two(), Vec::new());
                    w.status["config"]["save_to_folder"] = json!(false);
                    w.status["config"]["save_path"] = json!(
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                            .join("Pictures/A very long received-files folder/Pneu")
                    );
                },
                go(Page::Settings, 0, 0),
            ),
            scene(
                "38-settings-open-folder-error",
                |w| {
                    state(w, two(), Vec::new());
                    w.action_error = "Couldn't open ~/Pictures/Received files/Pneu".into();
                },
                go(Page::Settings, 0, 0),
            ),
            scene(
                "38-settings-manual",
                |w| {
                    state(w, two(), Vec::new());
                    w.status["config"]["clipboard_mode"] = json!("manual");
                },
                go(Page::Settings, 0, 0),
            ),
            scene(
                "39-settings-off",
                |w| {
                    state(w, two(), Vec::new());
                    w.status["config"]["clipboard_mode"] = json!("off");
                    w.status["config"]["choose_devices_each_time"] = json!(true);
                },
                go(Page::Settings, 0, 0),
            ),
            scene(
                "40-settings-rename",
                |w| state(w, two(), Vec::new()),
                |w| {
                    go(Page::Settings, 0, 0)(w);
                    let ui = w.global::<Ui>();
                    ui.set_rename_text("MJStanton-Desktop".into());
                    ui.set_dialog(Dialog::Rename);
                },
            ),
            scene(
                "41-picker",
                |w| state(w, two(), recent_items()),
                |w| {
                    go(Page::Main, 0, 0)(w);
                    w.global::<Ui>()
                        .set_picker_title("Send report.pdf to".into());
                    picks(
                        w,
                        &[
                            (
                                1,
                                "Galaxy S25+",
                                true,
                                true,
                                true,
                                "Connected over local network",
                            ),
                            (2, "MacBook Air", false, true, true, "Connected"),
                            (3, "Office-PC", false, false, true, "Not connected"),
                            (4, "Old Laptop", false, false, false, "Update required"),
                        ],
                    );
                },
            ),
            scene(
                "41-picker-clipboard",
                |w| {
                    state(w, two(), recent_items());
                    w.status["config"]["clipboard_mode"] = json!("manual");
                },
                |w| {
                    go(Page::Main, 0, 0)(w);
                    w.global::<Ui>()
                        .set_picker_title("Send clipboard to".into());
                    picks(
                        w,
                        &[
                            (
                                1,
                                "Galaxy S25+",
                                true,
                                true,
                                true,
                                "Connected over local network",
                            ),
                            (2, "MacBook Air", false, true, true, "Connected"),
                            (3, "Office-PC", false, false, false, "Not connected"),
                        ],
                    );
                },
            ),
            scene(
                "41-desktop-send-failed",
                |w| {
                    let mut failed = transfer(id(9).as_str(), 0, "report.pdf", "failed", 0, 0);
                    failed.transfer_id = failed.operation.clone();
                    failed.error = Some("device outbox is full".into());
                    state(w, two(), vec![failed]);
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "42-picker-many",
                |w| state(w, many(), Vec::new()),
                |w| {
                    go(Page::Main, 0, 0)(w);
                    w.global::<Ui>().set_picker_title("Send 3 files to".into());
                    let names: Vec<String> = (1..=16)
                        .map(|n| format!("Device {n:02} with a rather long name"))
                        .collect();
                    let rows: Vec<(u8, &str, bool, bool, bool, &str)> = names
                        .iter()
                        .enumerate()
                        .map(|(i, n)| {
                            (
                                i as u8 + 1,
                                n.as_str(),
                                i == 0,
                                i < 4,
                                i != 6,
                                if i < 4 { "Connected" } else { "Not connected" },
                            )
                        })
                        .collect();
                    picks(w, &rows);
                    let ui = w.global::<Ui>();
                    ui.set_picks({
                        let model = ui.get_picks();
                        let mut row = model.row_data(5).unwrap();
                        row.checked = true;
                        model.set_row_data(5, row);
                        model
                    });
                },
            ),
            scene(
                "44-recipient-details",
                |w| state(w, many(), vec![]),
                |w| {
                    go(Page::Main, 0, 0)(w);
                    let mut transfers = vec![
                        transfer("details", 1, "report.pdf", "sent", 40_000_000, 40_000_000),
                        transfer(
                            "details",
                            2,
                            "report.pdf",
                            "sending",
                            12_000_000,
                            40_000_000,
                        ),
                        transfer(
                            "details",
                            3,
                            "report.pdf",
                            "waiting_for_device",
                            0,
                            40_000_000,
                        ),
                        transfer("details", 4, "report.pdf", "failed", 0, 40_000_000),
                    ];
                    transfers[3].error = Some("UnsupportedKind".into());
                    let ps = peers(&snapshot(many()), &transfers);
                    let ui = w.global::<Ui>();
                    ui.set_details_title("report.pdf".into());
                    sync(
                        ui.get_recipients(),
                        &recipient_details(&ps, &transfers, &artifact_key(&transfers[0])),
                    );
                    ui.set_dialog(Dialog::Details);
                },
            ),
            scene(
                "45-recipient-details-many",
                |w| state(w, many(), vec![]),
                |w| {
                    go(Page::Main, 0, 0)(w);
                    let transfers: Vec<_> = (1..=16)
                        .map(|n| {
                            transfer(
                                "details",
                                n,
                                "A long report name for every recipient.pdf",
                                "waiting_for_device",
                                0,
                                10,
                            )
                        })
                        .collect();
                    let ps = peers(&snapshot(many()), &transfers);
                    let ui = w.global::<Ui>();
                    ui.set_details_title("A long report name for every recipient.pdf".into());
                    sync(
                        ui.get_recipients(),
                        &recipient_details(&ps, &transfers, &artifact_key(&transfers[0])),
                    );
                    ui.set_dialog(Dialog::Details);
                },
            ),
            scene(
                "46-rename-invalid",
                |w| state(w, two(), Vec::new()),
                |w| {
                    go(Page::Settings, 0, 0)(w);
                    let ui = w.global::<Ui>();
                    ui.set_rename_text(" \t\u{2003}".into());
                    ui.set_dialog(Dialog::Rename);
                },
            ),
            scene(
                "47-main-batch-artifacts",
                |w| {
                    state(
                        w,
                        two(),
                        vec![
                            transfer("batch", 1, "budget-2027.xlsx", "failed", 0, 9),
                            transfer("batch", 1, "report.pdf", "sending", 12_000_000, 40_000_000),
                        ],
                    )
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "48-join-rejected",
                |w| state(w, two(), Vec::new()),
                |w| {
                    message(
                        w,
                        "Couldn't pair",
                        "Paste the pairing link again to try again.",
                        0,
                        "",
                        "OK",
                    )
                },
            ),
            scene(
                "49-add-computer-camera",
                |w| state(w, Vec::new(), Vec::new()),
                |w| {
                    go(Page::Add, 0, 1)(w);
                    w.global::<Ui>().set_is_mac(true);
                    w.global::<Ui>().set_camera_available(true);
                },
            ),
            scene(
                "50-scan-preview",
                |w| state(w, Vec::new(), Vec::new()),
                |w| {
                    go(Page::Scan, 0, 1)(w);
                    w.global::<Ui>().set_is_mac(true);
                    w.global::<Ui>().set_scan_state(ScanState::Scanning);
                    w.global::<Ui>()
                        .set_camera_preview(slint::Image::from_rgba8(
                            qr_pixels(&sample_uri()).unwrap(),
                        ));
                },
            ),
            scene(
                "51-scan-denied",
                |w| state(w, Vec::new(), Vec::new()),
                |w| {
                    go(Page::Scan, 0, 1)(w);
                    w.global::<Ui>().set_is_mac(true);
                    w.global::<Ui>().set_scan_state(ScanState::Denied);
                },
            ),
            scene(
                "52-scan-unavailable",
                |w| state(w, Vec::new(), Vec::new()),
                |w| {
                    go(Page::Scan, 0, 1)(w);
                    w.global::<Ui>().set_is_mac(true);
                    w.global::<Ui>().set_scan_state(ScanState::Unavailable);
                },
            ),
            scene(
                "53-scan-failed",
                |w| state(w, Vec::new(), Vec::new()),
                |w| {
                    go(Page::Scan, 0, 1)(w);
                    w.global::<Ui>().set_is_mac(true);
                    w.global::<Ui>().set_scan_state(ScanState::Failed);
                },
            ),
            scene(
                "54-scan-paired",
                |w| {
                    state(
                        w,
                        vec![device(2, "Other Mac", false, "connected", 0)],
                        Vec::new(),
                    );
                    w.paired = Some(Paired {
                        origin: w.view(),
                        name: "Other Mac".into(),
                        device: Some(id(2).as_str().into()),
                        at: Instant::now(),
                        linked_at: None,
                        linked: String::new(),
                    });
                },
                |w| {
                    go(Page::Scan, 0, 1)(w);
                    w.global::<Ui>().set_is_mac(true);
                    w.global::<Ui>().set_scan_state(ScanState::Scanning);
                },
            ),
        ];
        list.extend(history_scenes());
        list.extend(linking_scenes());
        list
    }

    #[test]
    fn history_pin_changes_preserve_other_rows_frozen_copy_order() -> Result<()> {
        let item = |id: &str, pin| -> Result<crate::store::history::Item> {
            Ok(serde_json::from_value(
                json!({"id":id,"title":id,"pinned":pin,"url":false,"image":false,"origin":null,"at":1,"thumbnail":null}),
            )?)
        };
        let old = vec![item("pin", Some(1))?, item("b", None)?, item("a", None)?];
        let mut order = vec!["pin".into(), "a".into(), "b".into()];
        update_history_order(&mut order, &old, &old, true);
        assert_eq!(order, ["pin", "a", "b"]);
        let unpinned = vec![item("b", None)?, item("pin", None)?, item("a", None)?];
        update_history_order(&mut order, &old, &unpinned, true);
        assert_eq!(order, ["pin", "a", "b"], "unpin does not move a and b");
        update_history_order(&mut order, &unpinned, &unpinned, false);
        assert_eq!(
            order,
            ["b", "pin", "a"],
            "reopening takes the latest copy order"
        );
        Ok(())
    }

    #[tokio::test]
    async fn history_search_focus_enter_escape_and_shortcut() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::history_search_focus_enter_escape_and_shortcut",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (_worker, mut rx) = model_worker(&window, dir.path());
        let ui = window.global::<Ui>();
        ui.set_keep_history(true);
        ui.set_is_mac(cfg!(target_os = "macos"));
        ui.set_history_pins(1);
        let row = |id: &str, text: &str, pinned, picture| HistoryItem {
            id: id.into(),
            title: text.into(),
            search: crate::store::history::search_key(text).into(),
            pinned,
            picture,
            ..Default::default()
        };
        sync(
            ui.get_history(),
            &[
                row("recent", "Café mañana", false, false),
                row("pin", "CAFÉ au lait", true, false),
                row("image", "Image", false, true),
                row(
                    "long",
                    &format!("{} tail https://example.org", "x".repeat(600)),
                    false,
                    false,
                ),
            ],
        );
        window.invoke_go(Page::History);
        window.show()?;
        surface.set_size(slint::PhysicalSize::new(400, 600));
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        let draw = || {
            surface.request_redraw();
            surface.draw_if_needed(|renderer| {
                let mut pixels = vec![
                        slint::platform::software_renderer::PremultipliedRgbaColor::default();
                        400 * 600
                    ];
                renderer.render(&mut pixels, 400);
            });
            advance_ui();
        };
        let text = |text: &str| {
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.into() });
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: text.into() });
            flush_ui();
        };
        draw();
        while rx.try_recv().is_ok() {}
        text("cafe");
        assert_eq!(ui.get_history_query(), "cafe", "search takes focus on open");
        assert_eq!(ui.get_pinned_history().row_count(), 1);
        assert_eq!(ui.get_recent_history().row_count(), 1);
        let model = ui.get_history();
        let rows = model
            .as_any()
            .downcast_ref::<VecModel<HistoryItem>>()
            .unwrap();
        rows.push(row("arrival", "CAFÉ from a new copy", false, false));
        assert_eq!(
            ui.get_recent_history().row_count(),
            2,
            "matching arrivals update an active search"
        );
        rows.remove(4);
        let mut pin = rows.row_data(1).unwrap();
        pin.pinned = false;
        rows.set_row_data(1, pin.clone());
        assert_eq!(
            ui.get_pinned_history().row_count(),
            0,
            "unpin updates the active groups"
        );
        assert_eq!(ui.get_recent_history().row_count(), 2);
        pin.pinned = true;
        rows.set_row_data(1, pin);
        key(&window, slint::platform::Key::Return);
        assert!(
            matches!(rx.try_recv()?, Action::Scoped(_, action) if matches!(*action, Action::CopyHistory(ref id) if id == "pin"))
        );
        key(&window, slint::platform::Key::Escape);
        assert_eq!(ui.get_history_query(), "");
        assert_eq!(ui.get_page(), Page::History);
        text("invoice");
        assert_eq!(
            ui.get_pinned_history().row_count() + ui.get_recent_history().row_count(),
            0
        );
        assert!(
            window.get_content_height() >= 256.0,
            "No matches keeps its 132px card"
        );
        key(&window, slint::platform::Key::Return);
        assert!(rx.try_recv().is_err(), "Enter with no results does nothing");
        key(&window, slint::platform::Key::Escape);
        text("tail HTTPS://");
        assert_eq!(
            ui.get_recent_history().row_count(),
            1,
            "full text and URL scheme match"
        );
        key(&window, slint::platform::Key::Tab);
        let modifier = if cfg!(target_os = "macos") {
            slint::platform::Key::Meta
        } else {
            slint::platform::Key::Control
        };
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: modifier.into(),
            });
        text("f");
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                text: modifier.into(),
            });
        text("mañana");
        assert_eq!(
            ui.get_history_query(),
            "mañana",
            "shortcut focuses and selects all text"
        );
        window.invoke_go(Page::Main);
        assert_eq!(ui.get_history_query(), "");
        window.invoke_go(Page::History);
        draw();
        key(&window, slint::platform::Key::Escape);
        assert_eq!(ui.get_page(), Page::Main);
        window.invoke_go(Page::Settings);
        ui.set_dialog(Dialog::HistoryOff);
        draw();
        window.invoke_go(Page::History); // Show All also closes an open popup.
        draw();
        text("cafe");
        assert_eq!(
            ui.get_history_query(),
            "cafe",
            "Show All focuses search after dismissing a popup"
        );
        key(&window, slint::platform::Key::Escape);
        ui.set_dialog(Dialog::HistoryClear);
        draw();
        key(&window, slint::platform::Key::Return);
        assert_eq!(
            ui.get_dialog(),
            Dialog::None,
            "Cancel owns Enter when search previously had focus"
        );
        while let Ok(action) = rx.try_recv() {
            assert!(
                !matches!(action, Action::Scoped(_, action) if matches!(*action, Action::CopyHistory(_)))
            );
        }
        window.hide()?;
        Ok(())
    }

    #[tokio::test]
    async fn history_keyboard_copies_deletes_and_confirmations_default_to_cancel() -> Result<()> {
        if in_offscreen_child("ui::window::tests::history_keyboard_copies_deletes_and_confirmations_default_to_cancel",&[]).await? {return Ok(());}
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut rx) = model_worker(&window, dir.path());
        let ui = window.global::<Ui>();
        ui.set_keep_history(true);
        ui.set_motion(false);
        let row = HistoryItem {
            id: "row".into(),
            title: "A saved copy".into(),
            detail: "This computer · 10:41".into(),
            ..Default::default()
        };
        sync(ui.get_history(), &[row]);
        window.invoke_go(Page::History);
        window.show()?;
        surface.set_size(slint::PhysicalSize::new(400, 400));
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        let draw = || {
            surface.request_redraw();
            surface.draw_if_needed(|renderer| {
                let mut pixels = vec![
                        slint::platform::software_renderer::PremultipliedRgbaColor::default();
                        400 * 400
                    ];
                renderer.render(&mut pixels, 400);
            });
            advance_ui();
        };
        draw();
        key(&window, slint::platform::Key::Tab);
        key(&window, slint::platform::Key::Return);
        key(&window, slint::platform::Key::Delete);
        let mut copied = 0;
        let mut deleted = 0;
        while let Ok(Action::Scoped(_, action)) = rx.try_recv() {
            match *action {
                Action::CopyHistory(ref id) if id == "row" => copied += 1,
                Action::DeleteHistory(ref id) if id == "row" => deleted += 1,
                _ => {}
            }
        }
        assert_eq!((copied, deleted), (1, 1));
        for dialog in [Dialog::HistoryClear, Dialog::HistoryOff] {
            ui.set_dialog(dialog);
            draw();
            key(&window, slint::platform::Key::Return);
            assert_eq!(ui.get_dialog(), Dialog::None);
            assert!(ui.get_keep_history());
            ui.set_dialog(dialog);
            draw();
            key(&window, slint::platform::Key::Escape);
            assert_eq!(ui.get_dialog(), Dialog::None);
            assert!(ui.get_keep_history());
        }
        assert!(rx.try_recv().is_err());
        window.invoke_history_copied("row".into());
        assert_eq!(ui.get_history_copied(), "row");
        flush_ui();
        std::thread::sleep(Duration::from_millis(1200));
        flush_ui();
        window.invoke_history_copied("another row".into());
        flush_ui();
        std::thread::sleep(Duration::from_millis(1100));
        flush_ui();
        assert_eq!(
            ui.get_history_copied(),
            "another row",
            "second copy did not restart timer"
        );
        std::thread::sleep(Duration::from_millis(1000));
        flush_ui();
        assert_eq!(ui.get_history_copied(), "");
        key(&window, slint::platform::Key::Escape);
        assert_eq!(ui.get_page(), Page::Main);
        // The empty window model must still ask the daemon before disabling.
        sync(ui.get_history(), &[]);
        let mock = mock_ipc(&worker.paths.socket, |command| {
            assert_eq!(command["command"], "disable_empty_clipboard_history");
            (true, json!(2))
        })?;
        ui.invoke_disable_empty_history();
        while let Ok(action) = rx.try_recv() {
            if matches!(&action, Action::Scoped(_, action) if matches!(**action, Action::DisableEmptyHistory))
            {
                act_with_ui(&mut worker, action).await;
            }
        }
        assert_eq!(ui.get_dialog(), Dialog::HistoryOff);
        assert_eq!(ui.get_history_off_count(), 2);
        assert!(ui.get_keep_history());
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        Ok(())
    }

    #[test]
    fn history_title_search_keeps_graphemes_and_measures_the_ellipsis() {
        use slint::private_unstable_api::re_exports::UnicodeSegmentation;
        let fits = |text: &str| text.graphemes(true).count() <= 6;
        assert_eq!(fit_history_title("", fits), "");
        assert_eq!(fit_history_title("Short", fits), "Short");
        assert_eq!(fit_history_title("Exact!", fits), "Exact!");
        assert_eq!(fit_history_title("A e\u{301}👩‍💻XYZ", fits), "A e\u{301}👩‍💻X…");
        assert_eq!(fit_history_title("abcd efghi", fits), "abcd…");
        assert_eq!(
            fit_history_title("WWWiiii", |text| {
                text.chars()
                    .map(|c| match c {
                        'W' => 4,
                        '…' => 2,
                        _ => 1,
                    })
                    .sum::<usize>()
                    <= 10
            }),
            "WW…"
        );
    }

    #[tokio::test]
    async fn history_titles_fit_two_lines_after_width_and_scale_changes() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::history_titles_fit_two_lines_after_width_and_scale_changes",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        let surface = headless()?;
        let window = PneuWindow::new()?;
        connect(
            &window,
            &tokio::sync::mpsc::unbounded_channel().0,
            &Arc::new(Mutex::new(View::default())),
        );
        window.show()?;
        for scale in [1.0, 2.0] {
            surface.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
            for width in [240.0, 480.0, 120.0, 240.0] {
                let line_height = history_text_size(&window, "Ag", width).1.ceil();
                for text in [
                    "The meeting has moved to Thursday. Please send the updated notes before lunch. ".repeat(6),
                    "W".repeat(160),
                    "é👩‍💻界".repeat(100),
                ] {
                    let result = window.global::<Ui>().invoke_elide_history_title(text.clone().into(), width, line_height * 2.0);
                    assert!(result.ends_with('…'), "{width}px at {scale}x: {result}");
                    assert!(text.starts_with(result.trim_end_matches('…')));
                    let (w, h) = history_text_size(&window, &result, width);
                    assert!(w <= width && h.ceil() <= line_height * 2.0,
                        "{width}px at {scale}x: {w}x{h}: {result}");
                    assert!(h > line_height, "long text should use its second line: {result}");
                }
                assert_eq!(
                    window.global::<Ui>().invoke_elide_history_title(
                        "Short".into(),
                        width,
                        line_height * 2.0
                    ),
                    "Short"
                );
            }
        }
        Ok(())
    }

    fn history_fixture(w: &mut Worker, empty: bool) {
        state(w, trio(), recent_items());
        w.status["config"]["clipboard_history"] = json!(true);
        let at = crate::now_ms() - 60_000;
        w.snapshot.clipboard_history = if empty {
            vec![]
        } else {
            [
            ("Remember to bring the keys",None,false),
            ("example.org/plans?view=week",Some("Galaxy S25+"),false),
            ("Image",Some("MacBook Air"),true),
            ("The meeting has moved to Thursday. Please send the updated notes before lunch so everyone has time to read them.",None,false),
            ("averylongwordwithoutanyspaces".repeat(8).as_str(),Some("The computer with an unusually long device name that must be elided"),false),
        ].into_iter().enumerate().map(|(i,(text,origin,image))|crate::store::history::Item {id:format!("{i:032x}"),title:text.into(),search:crate::store::history::search_key(text),pinned:None,url:i==1,image,origin:origin.map(str::to_owned),at,thumbnail:None}).collect()
        };
        w.history_order = w
            .snapshot
            .clipboard_history
            .iter()
            .map(|i| i.id.clone())
            .collect();
    }
    fn pinned_history_fixture(w: &mut Worker) {
        history_fixture(w, false);
        let pins = [
            "Calle Los Aromos 1840, Depto 52, Providencia",
            "meet.example.com/pneu-weekly",
            "Guest Wi-Fi: lantern-orbit-42",
        ];
        for (n, text) in pins.into_iter().enumerate() {
            w.snapshot.clipboard_history.insert(
                n,
                crate::store::history::Item {
                    id: format!("{:032x}", 100 + n),
                    title: text.into(),
                    search: crate::store::history::search_key(text),
                    pinned: Some(3 - n as i64),
                    url: n == 1,
                    image: false,
                    origin: None,
                    at: crate::now_ms() - 86_400_000,
                    thumbnail: None,
                },
            );
        }
        w.history_order = w
            .snapshot
            .clipboard_history
            .iter()
            .map(|i| i.id.clone())
            .collect();
    }
    fn history_scenes() -> Vec<Scene> {
        vec![
            scene(
                "88-history-pinned-recent",
                pinned_history_fixture,
                go(Page::History, 0, 0),
            ),
            scene("89-history-searching", pinned_history_fixture, |w| {
                go(Page::History, 0, 0)(w);
                let _ = w.get_content_height();
                advance_ui();
                w.global::<Ui>().set_history_query("pneu".into());
            }),
            scene("90-history-no-matches", pinned_history_fixture, |w| {
                go(Page::History, 0, 0)(w);
                let _ = w.get_content_height();
                advance_ui();
                w.global::<Ui>().set_history_query("invoice".into());
            }),
            scene(
                "91-history-pin-limit",
                |w| {
                    pinned_history_fixture(w);
                    let template = w.snapshot.clipboard_history[0].clone();
                    for n in 3..25 {
                        let mut item = template.clone();
                        item.id = format!("{:032x}", 100 + n);
                        item.title = format!("Saved item {}", n + 1);
                        item.pinned = Some(3 - n);
                        w.snapshot.clipboard_history.push(item);
                    }
                    w.history_order = w
                        .snapshot
                        .clipboard_history
                        .iter()
                        .map(|i| i.id.clone())
                        .collect();
                    w.action_error = crate::store::history::PIN_LIMIT_ERROR.into();
                },
                go(Page::History, 0, 0),
            ),
            scene("92-history-clear-keeps-pins", pinned_history_fixture, |w| {
                go(Page::History, 0, 0)(w);
                w.global::<Ui>().set_dialog(Dialog::HistoryClear);
            }),
            scene(
                "93-history-off-includes-pins",
                pinned_history_fixture,
                |w| {
                    go(Page::Settings, 0, 0)(w);
                    w.global::<Ui>().set_history_off_count(8);
                    w.global::<Ui>().set_dialog(Dialog::HistoryOff);
                },
            ),
            scene(
                "94-history-only-pins",
                |w| {
                    pinned_history_fixture(w);
                    w.snapshot.clipboard_history.retain(|i| i.pinned.is_some());
                },
                go(Page::History, 0, 0),
            ),
            scene(
                "95-history-off-only-pins",
                |w| {
                    pinned_history_fixture(w);
                    w.snapshot.clipboard_history.retain(|i| i.pinned.is_some());
                },
                |w| {
                    go(Page::Settings, 0, 0)(w);
                    w.global::<Ui>().set_history_off_count(3);
                    w.global::<Ui>().set_dialog(Dialog::HistoryOff);
                },
            ),
            scene(
                "80-history-main",
                |w| history_fixture(w, false),
                go(Page::Main, 0, 0),
            ),
            scene(
                "81-history-items",
                |w| history_fixture(w, false),
                go(Page::History, 0, 0),
            ),
            scene(
                "82-history-empty",
                |w| history_fixture(w, true),
                go(Page::History, 0, 0),
            ),
            scene(
                "83-history-copied",
                |w| history_fixture(w, false),
                |w| {
                    go(Page::History, 0, 0)(w);
                    w.invoke_history_copied(format!("{:032x}", 1).into());
                },
            ),
            scene(
                "84-history-clear",
                |w| history_fixture(w, false),
                |w| {
                    go(Page::History, 0, 0)(w);
                    w.global::<Ui>().set_dialog(Dialog::HistoryClear);
                },
            ),
            scene(
                "85-history-turn-off",
                |w| history_fixture(w, false),
                |w| {
                    go(Page::Settings, 0, 0)(w);
                    w.global::<Ui>().set_history_off_count(5);
                    w.global::<Ui>().set_dialog(Dialog::HistoryOff);
                },
            ),
            scene(
                "86-history-setting",
                |w| history_fixture(w, true),
                go(Page::Settings, 0, 0),
            ),
            scene(
                "87-history-image",
                |w| history_fixture(w, false),
                |w| {
                    go(Page::History, 0, 0)(w);
                    let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(72, 72);
                    for (i, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
                        *pixel = slint::Rgba8Pixel {
                            r: (i % 72 * 3) as u8,
                            g: (i / 72 * 3) as u8,
                            b: 145,
                            a: 255,
                        };
                    }
                    let model = w.global::<Ui>().get_history();
                    if let Some(mut row) = model.row_data(2) {
                        row.thumbnail = slint::Image::from_rgba8(pixels);
                        model
                            .as_any()
                            .downcast_ref::<VecModel<HistoryItem>>()
                            .unwrap()
                            .set_row_data(2, row);
                    }
                },
            ),
        ]
    }

    // ---------- Pair once, join your devices ----------

    /// MJStanton-Desktop (this computer) with Galaxy S25+ and MacBook Air.
    fn trio() -> Vec<crate::core::mesh::DeviceSnapshot> {
        vec![
            device(1, "Galaxy S25+", true, "connected", 0),
            device(2, "MacBook Air", false, "connected", 0),
        ]
    }
    /// MacBook Air (this Mac) with Galaxy S25+ and MJStanton-Desktop.
    fn mac_trio() -> Vec<crate::core::mesh::DeviceSnapshot> {
        vec![
            device(1, "Galaxy S25+", true, "connected", 0),
            device(3, "MJStanton-Desktop", false, "connected", 0),
        ]
    }
    fn link_of(n: u8, scope: &str) -> Value {
        json!({"device": id(n), "scope": scope, "prompt": false, "introduced_by": null,
            "introducer_name": null, "outcome": null, "negotiated": true,
            "participating": scope == "enabled", "hello_seen": true, "ready": scope == "enabled"})
    }
    fn with(mut link: Value, key: &str, value: Value) -> Value {
        link[key] = value;
        link
    }
    /// The daemon's setup status (setup.rs). `members` are your other devices;
    /// this device (9) joins the roster when there are any.
    fn linking(w: &mut Worker, links: Vec<Value>, members: &[(u8, &str)]) {
        w.status["self_id"] = json!(id(9));
        w.status["setup"] = setup_json(links, members);
        w.setup = Setup::parse(&w.status);
    }
    fn setup_json(links: Vec<Value>, members: &[(u8, &str)]) -> Value {
        let member = |n: u8, name: &str, hlc: u64| {
            json!({"device_id": id(n), "fingerprint": format!("{n:02x}").repeat(32),
                "role": "computer", "name": name, "status": "member", "hlc": hlc,
                "writer_device": id(n)})
        };
        let mut roster = Vec::new();
        if !members.is_empty() {
            roster.push(member(9, "This computer", 10));
            roster.extend(
                members
                    .iter()
                    .map(|(n, name)| member(*n, name, 20 + u64::from(*n))),
            );
        }
        let generations: serde_json::Map<String, Value> = links
            .iter()
            .map(|l| (l["device"].as_str().unwrap().to_owned(), json!(3)))
            .collect();
        json!({"generation": 2, "withdrawn": false, "roster": roster,
            "links": links, "pending_link": false, "waiting_for": null,
            "binding_generations": generations, "notices": []})
    }
    fn mac(ui: impl Fn(&PneuWindow) + 'static) -> impl Fn(&PneuWindow) + 'static {
        move |w: &PneuWindow| {
            ui(w);
            w.global::<Ui>().set_is_mac(true);
        }
    }
    fn device_page(w: &mut Worker, n: u8) {
        let (me, it) = (id(9), id(n));
        open(w, Page::Device, n, 0);
        w.peer_settings = Some((
            it.as_str().into(),
            json!({"can_sync": true, "settings": [
                {"key": format!("{me}/{it}/clipboard"), "enabled": true, "pending": false},
                {"key": format!("{it}/{me}/clipboard"), "enabled": true, "pending": false},
            ]}),
            Instant::now(),
        ));
    }
    fn join_confirm(link: bool) -> impl Fn(&PneuWindow) {
        move |w: &PneuWindow| {
            message(w, "Pair with MJStanton-Desktop?", "", 1, "Cancel", "Pair");
            let ui = w.global::<Ui>();
            let mut m = ui.get_message();
            m.action = "join-confirm".into();
            ui.set_message(m);
            ui.set_link_on(link);
        }
    }
    fn offer_card(w: &mut Worker) {
        w.join = Some(Join {
            attempt: "a".into(),
            uri: String::new(),
            name: "MJStanton-Desktop".into(),
            confirmed: false,
            cancel_requested: false,
            offer: true,
            link: true,
            origin: View::default(),
            reported: String::new(),
        });
    }
    fn removed(w: &PneuWindow) {
        let ui = w.global::<Ui>();
        go(Page::Add, 0, 0)(w);
        let title = if ui.get_is_mac() {
            "This Mac was removed from your devices"
        } else {
            "This computer was removed from your devices"
        };
        ui.set_message(Message {
            title: title.into(),
            text: "".into(),
            tile: 0,
            primary: "OK".into(),
            secondary: "".into(),
            action: "removed-ok".into(),
        });
        ui.set_dialog(Dialog::Message);
    }
    fn full(w: &mut Worker) {
        let others: Vec<(u8, String)> = (20..36).map(|n| (n, format!("Lab-PC-{n}"))).collect();
        let others: Vec<(u8, &str)> = others.iter().map(|(n, s)| (*n, s.as_str())).collect();
        linking(w, vec![], &others);
    }
    fn paired_with(w: &mut Worker, n: u8, name: &str) {
        w.paired = Some(Paired {
            origin: w.view(),
            name: name.into(),
            device: Some(id(n).as_str().into()),
            at: Instant::now(),
            linked_at: None,
            linked: String::new(),
        });
    }
    fn readmission_record() -> crate::store::members::Readmission {
        serde_json::from_value(json!({
            "key": {"device":id(2), "fingerprint":"02".repeat(32), "removal_hlc":10,
                "removal_writer":id(1), "setup_generation":2},
            "removed_name":"MacBook Air", "value": {"device_id":id(2), "fingerprint":"02".repeat(32),
                "role":"computer", "name":"MacBook Air", "status":"member", "hlc":20,"writer_device":id(1)},
            "introducer":id(1), "introducer_name":"Galaxy S25+", "decision":"pending"
        })).unwrap()
    }

    #[tokio::test]
    async fn readmission_card_keeps_focus_and_captured_authority() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::readmission_card_keeps_focus_and_captured_authority",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        let surface = headless()?;
        let window = PneuWindow::new()?;
        let dir = tempfile::tempdir()?;
        let (mut worker, mut actions) = model_worker(&window, dir.path());
        std::fs::create_dir_all(&worker.paths.data)?;
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        let visible = AtomicBool::new(true);
        state(&mut worker, trio(), vec![]);
        let record = readmission_record();
        let mut status = setup_json(vec![link_of(1, "enabled")], &[(1, "Galaxy S25+")]);
        status["readmissions"] = json!([record]);
        window.show()?;
        surface.set_size(slint::PhysicalSize::new(400, 720));
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        worker
            .refresh(linked_status(&worker, &status), &visible)
            .await?;
        flush_ui();
        assert_eq!(window.global::<Ui>().get_page(), Page::Main);
        settle(&surface);
        assert_eq!(window.global::<Ui>().get_top().kind, TopKind::Readmit);
        // The native keyboard path must choose Keep removed by default.
        slint::platform::update_timers_and_animations();
        key(&window, slint::platform::Key::Return);
        drain(&mut worker, &mut actions).await;
        let commands = sent(&mock, "readmission_decision");
        assert_eq!(
            commands,
            vec![json!({"command":"readmission_decision","key":record.key,"allow":false})]
        );
        // A click already queued from this frame keeps its old barrier/generation.
        worker
            .refresh(linked_status(&worker, &status), &visible)
            .await?;
        flush_ui();
        window.global::<Ui>().invoke_top_action(true);
        let mut next = status.clone();
        next["readmissions"][0]["key"]["removal_hlc"] = json!(30);
        next["readmissions"][0]["key"]["setup_generation"] = json!(9);
        worker
            .refresh(linked_status(&worker, &next), &visible)
            .await?;
        drain(&mut worker, &mut actions).await;
        assert_eq!(
            sent(&mock, "readmission_decision")[1],
            json!({"command":"readmission_decision","key":record.key,"allow":true})
        );
        worker
            .refresh(linked_status(&worker, &next), &visible)
            .await?;
        flush_ui();
        key(&window, slint::platform::Key::Tab); // leave Keep removed on the old card
        next["readmissions"][0]["key"]["removal_hlc"] = json!(40);
        worker
            .refresh(linked_status(&worker, &next), &visible)
            .await?;
        flush_ui();
        settle(&surface);
        key(&window, slint::platform::Key::Return);
        drain(&mut worker, &mut actions).await;
        assert_eq!(
            sent(&mock, "readmission_decision")[2],
            json!({"command":"readmission_decision","key":next["readmissions"][0]["key"],"allow":false})
        );
        // Dismissal is window-local, and does not decide or consume the prompt.
        worker
            .refresh(linked_status(&worker, &status), &visible)
            .await?;
        flush_ui();
        window.global::<Ui>().invoke_top_close();
        drain(&mut worker, &mut actions).await;
        assert_eq!(sent(&mock, "readmission_decision").len(), 3);
        assert!(matches!(
            setup::top(
                &worker.setup,
                &peers(&worker.snapshot, &[]),
                &worker.remembered
            ),
            Top::None
        ));
        assert!(matches!(
            setup::top(
                &worker.setup,
                &peers(&worker.snapshot, &[]),
                &Remembered::default()
            ),
            Top::Readmit { .. }
        ));
        Ok(())
    }

    fn linking_scenes() -> Vec<Scene> {
        let both = || vec![link_of(1, "ask"), link_of(2, "ask")];
        let member = |link: Value| {
            move |w: &mut Worker| {
                state(w, trio(), Vec::new());
                linking(
                    w,
                    vec![link_of(1, "enabled"), link.clone()],
                    &[(1, "Galaxy S25+"), (2, "MacBook Air")],
                );
                device_page(w, 2);
            }
        };
        let plain = |link: Value| {
            move |w: &mut Worker| {
                state(w, trio(), Vec::new());
                linking(w, vec![link_of(1, "ask"), link.clone()], &[]);
                device_page(w, 2);
            }
        };
        let via = with(
            link_of(2, "enabled"),
            "introducer_name",
            json!("Galaxy S25+"),
        );
        let separate = with(link_of(2, "disabled"), "outcome", json!(setup::SEPARATE));
        vec![
            scene(
                "72-readmission",
                |w| {
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 1)],
                        vec![],
                    );
                    linking(w, vec![link_of(1, "enabled")], &[(1, "Galaxy S25+")]);
                    w.setup.readmissions = vec![readmission_record()];
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "73-removed-self-offline-phone",
                |w| {
                    state(
                        w,
                        vec![
                            device(1, "Galaxy S25+", true, "disconnected", 100),
                            device(2, "MacBook Air", false, "unauthorized", 100),
                        ],
                        vec![],
                    );
                    w.snapshot.setup_refusal_since = Some(200);
                    linking(
                        w,
                        vec![link_of(1, "enabled"), link_of(2, "enabled")],
                        &[(1, "Galaxy S25+"), (2, "MacBook Air")],
                    );
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "73-introduced-refused-row",
                |w| {
                    state(
                        w,
                        vec![device(2, "MacBook Air", false, "unauthorized", 0)],
                        vec![],
                    );
                    linking(
                        w,
                        vec![with(
                            with(link_of(2, "enabled"), "introduced_by", json!(id(1))),
                            "ready",
                            json!(false),
                        )],
                        &[(2, "MacBook Air")],
                    );
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "73-introduced-refused",
                |w| {
                    state(
                        w,
                        vec![device(2, "MacBook Air", false, "unauthorized", 0)],
                        vec![],
                    );
                    linking(
                        w,
                        vec![with(
                            with(link_of(2, "enabled"), "introduced_by", json!(id(1))),
                            "ready",
                            json!(false),
                        )],
                        &[(2, "MacBook Air")],
                    );
                    device_page(w, 2);
                },
                go(Page::Device, 2, 0),
            ),
            scene(
                "73-introduced-waiting-long-name",
                |w| {
                    state(
                        w,
                        vec![device(2, "sdk_gphone16k_x86_64", true, "disconnected", 0)],
                        vec![],
                    );
                    linking(
                        w,
                        vec![with(
                            with(link_of(2, "enabled"), "introduced_by", json!(id(1))),
                            "ready",
                            json!(false),
                        )],
                        &[(2, "sdk_gphone16k_x86_64")],
                    );
                    device_page(w, 2);
                },
                go(Page::Device, 2, 0),
            ),
            scene(
                "73-introduced-waiting-max-name",
                |w| {
                    let name = "Very long device name ".repeat(6);
                    assert_eq!(name.len(), 132);
                    let name = &name[..128];
                    state(w, vec![device(2, name, true, "disconnected", 0)], vec![]);
                    linking(
                        w,
                        vec![with(
                            with(link_of(2, "enabled"), "introduced_by", json!(id(1))),
                            "ready",
                            json!(false),
                        )],
                        &[(2, name)],
                    );
                    device_page(w, 2);
                },
                go(Page::Device, 2, 0),
            ),
            scene(
                "73-connected-settings-pending",
                |w| {
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 1)],
                        vec![],
                    );
                    device_page(w, 1);
                    if let Some((_, reply, _)) = &mut w.peer_settings {
                        for setting in reply["settings"].as_array_mut().unwrap() {
                            setting["pending"] = json!(true);
                        }
                    }
                },
                go(Page::Device, 1, 0),
            ),
            scene(
                "74-paired-before-ready",
                |w| {
                    state(
                        w,
                        vec![
                            device(1, "Galaxy S25+", true, "connected", 1),
                            device(2, "MacBook Air", false, "connecting", 0),
                        ],
                        vec![],
                    );
                    linking(
                        w,
                        vec![
                            link_of(1, "enabled"),
                            with(
                                with(
                                    with(link_of(2, "enabled"), "introduced_by", json!(id(1))),
                                    "ready",
                                    json!(false),
                                ),
                                "hello_seen",
                                json!(false),
                            ),
                        ],
                        &[(1, "Galaxy S25+"), (2, "MacBook Air")],
                    );
                    paired_with(w, 1, "Galaxy S25+");
                },
                go(Page::Add, 0, 0),
            ),
            scene(
                "61-main-link-refresh",
                |w| {
                    state(
                        w,
                        vec![
                            device(1, "Galaxy S25+", true, "disconnected", 123),
                            device(2, "MacBook Air", false, "disconnected", 123),
                        ],
                        vec![],
                    );
                    w.snapshot.refreshing_devices = vec![id(1), id(2)];
                    linking(
                        w,
                        vec![
                            with(
                                with(link_of(1, "ask"), "prompt", json!(true)),
                                "outcome",
                                json!("allow_pending_refresh"),
                            ),
                            link_of(2, "ask"),
                        ],
                        &[],
                    );
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "65-device-not-connected-yet",
                |w| {
                    state(
                        w,
                        vec![device(2, "MacBook Air", false, "disconnected", 0)],
                        vec![],
                    );
                    linking(
                        w,
                        vec![with(
                            link_of(2, "enabled"),
                            "introducer_name",
                            json!("Phone"),
                        )],
                        &[(2, "MacBook Air")],
                    );
                    device_page(w, 2);
                    w.peer_settings = Some((
                        id(2).to_string(),
                        json!({"can_sync": false}),
                        Instant::now(),
                    ));
                },
                go(Page::Device, 2, 0),
            ),
            scene(
                "60-main-link-offer",
                move |w| {
                    state(w, trio(), Vec::new());
                    linking(w, both(), &[]);
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "60-main-link-offer-mac",
                |w| {
                    state(w, mac_trio(), Vec::new());
                    linking(w, vec![link_of(1, "ask"), link_of(3, "ask")], &[]);
                },
                mac(go(Page::Main, 0, 0)),
            ),
            scene(
                "61-main-link-waiting",
                |w| {
                    state(w, trio(), Vec::new());
                    linking(w, vec![link_of(1, "enabled"), link_of(2, "enabled")], &[]);
                    for link in &mut w.setup.links {
                        link.participating = false;
                    }
                    w.remembered.offer = Some(setup::LINKED.into());
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "62-main-link-request",
                |w| {
                    state(w, trio(), Vec::new());
                    linking(
                        w,
                        vec![
                            with(link_of(1, "ask"), "prompt", json!(true)),
                            link_of(2, "ask"),
                        ],
                        &[],
                    );
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "62-main-link-request-mac",
                |w| {
                    state(w, mac_trio(), Vec::new());
                    let asked = |n| with(link_of(n, "ask"), "prompt", json!(true));
                    linking(w, vec![asked(1), asked(3)], &[]);
                },
                mac(go(Page::Main, 0, 0)),
            ),
            scene(
                "62-main-link-request-many",
                |w| {
                    let mut devices = trio();
                    devices.push(device(4, "Office-PC", false, "connected", 0));
                    state(w, devices, Vec::new());
                    let asked = |n| with(link_of(n, "ask"), "prompt", json!(true));
                    linking(w, vec![asked(1), asked(2), asked(4)], &[]);
                },
                go(Page::Main, 0, 0),
            ),
            scene(
                "60-main-new-device-connecting",
                |w| {
                    let mut desktop = device(3, "MJStanton-Desktop", false, "connecting", 0);
                    desktop.route = None;
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 0), desktop],
                        Vec::new(),
                    );
                    let introduced = with(
                        link_of(3, "enabled"),
                        "introducer_name",
                        json!("Galaxy S25+"),
                    );
                    linking(
                        w,
                        vec![link_of(1, "enabled"), introduced],
                        &[(1, "Galaxy S25+"), (3, "MJStanton-Desktop")],
                    );
                },
                mac(go(Page::Main, 0, 0)),
            ),
            scene(
                "60-main-new-device-not-yet",
                |w| {
                    let desktop = device(3, "MJStanton-Desktop", false, "disconnected", 0);
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 0), desktop],
                        Vec::new(),
                    );
                    let introduced = with(
                        link_of(3, "enabled"),
                        "introducer_name",
                        json!("Galaxy S25+"),
                    );
                    linking(
                        w,
                        vec![link_of(1, "enabled"), introduced],
                        &[(1, "Galaxy S25+"), (3, "MJStanton-Desktop")],
                    );
                },
                mac(go(Page::Main, 0, 0)),
            ),
            scene(
                "63-device-linked",
                member(link_of(2, "enabled")),
                go(Page::Device, 2, 0),
            ),
            scene(
                "63-device-linked-via",
                member(via.clone()),
                go(Page::Device, 2, 0),
            ),
            scene(
                "64-device-member-link-mac",
                |w| {
                    state(w, mac_trio(), Vec::new());
                    linking(
                        w,
                        vec![link_of(1, "enabled"), link_of(3, "disabled")],
                        &[(1, "Galaxy S25+"), (3, "MJStanton-Desktop")],
                    );
                    device_page(w, 3);
                },
                mac(go(Page::Device, 3, 0)),
            ),
            scene(
                "64-device-asked-mac",
                |w| {
                    state(w, mac_trio(), Vec::new());
                    linking(
                        w,
                        vec![
                            link_of(1, "ask"),
                            with(link_of(3, "ask"), "prompt", json!(true)),
                        ],
                        &[],
                    );
                    device_page(w, 3);
                },
                mac(go(Page::Device, 3, 0)),
            ),
            scene(
                "65-device-link",
                plain(link_of(2, "ask")),
                go(Page::Device, 2, 0),
            ),
            scene(
                "65-device-waiting",
                plain(with(link_of(2, "enabled"), "participating", json!(false))),
                go(Page::Device, 2, 0),
            ),
            scene(
                "65-device-theirs",
                plain(with(
                    with(link_of(2, "enabled"), "participating", json!(false)),
                    "remote_scope",
                    json!("disabled"),
                )),
                go(Page::Device, 2, 0),
            ),
            scene(
                "65-device-update",
                plain(with(link_of(2, "ask"), "negotiated", json!(false))),
                go(Page::Device, 2, 0),
            ),
            scene(
                "65-device-separate",
                plain(separate.clone()),
                go(Page::Device, 2, 0),
            ),
            scene(
                "65-device-separate-mac",
                |w| {
                    let mut devices = mac_trio();
                    devices.push(device(4, "Office-PC", false, "connected", 0));
                    state(w, devices, Vec::new());
                    let separate = with(link_of(4, "disabled"), "outcome", json!(setup::SEPARATE));
                    linking(
                        w,
                        vec![link_of(1, "enabled"), link_of(3, "enabled"), separate],
                        &[(1, "Galaxy S25+"), (3, "MJStanton-Desktop")],
                    );
                    device_page(w, 4);
                },
                mac(go(Page::Device, 4, 0)),
            ),
            scene("66-remove-confirm", member(via.clone()), |w| {
                go(Page::Device, 2, 0)(w);
                w.invoke_prepare_remove();
            }),
            scene(
                "66-remove-confirm-waiting",
                {
                    let via = via.clone();
                    move |w: &mut Worker| {
                        member(via.clone())(w);
                        w.transfers = vec![
                            transfer("w1", 2, "notes.md", "waiting_for_device", 0, 9),
                            transfer("w2", 2, "report.pdf", "waiting_for_device", 0, 9),
                            transfer("w3", 2, "Screenshot.png", "waiting_for_device", 0, 9),
                        ];
                    }
                },
                |w| {
                    go(Page::Device, 2, 0)(w);
                    w.invoke_prepare_remove();
                },
            ),
            scene(
                "66-remove-confirm-mac",
                |w| {
                    state(w, mac_trio(), Vec::new());
                    let via = with(
                        link_of(3, "enabled"),
                        "introducer_name",
                        json!("Galaxy S25+"),
                    );
                    linking(
                        w,
                        vec![link_of(1, "enabled"), via],
                        &[(1, "Galaxy S25+"), (3, "MJStanton-Desktop")],
                    );
                    device_page(w, 3);
                },
                mac(|w| {
                    go(Page::Device, 3, 0)(w);
                    w.invoke_prepare_remove();
                }),
            ),
            scene("66-remove-failed", member(via.clone()), |w| {
                go(Page::Device, 2, 0)(w);
                let ui = w.global::<Ui>();
                ui.set_message(Message {
                    title: "Couldn't remove MacBook Air".into(),
                    text: "Nothing was changed. Try again.".into(),
                    tile: 0,
                    primary: "OK".into(),
                    secondary: "".into(),
                    action: "ok".into(),
                });
                ui.set_dialog(Dialog::Message);
            }),
            scene("67-removed", |w| state(w, Vec::new(), Vec::new()), removed),
            scene(
                "67-removed-mac",
                |w| state(w, Vec::new(), Vec::new()),
                |w| {
                    w.global::<Ui>().set_is_mac(true);
                    removed(w);
                },
            ),
            scene(
                "68-join-confirm-link",
                |w| state(w, two(), Vec::new()),
                join_confirm(true),
            ),
            scene(
                "68-join-confirm-plain",
                |w| state(w, two(), Vec::new()),
                join_confirm(false),
            ),
            scene(
                "68-join-confirm-limit",
                |w| {
                    state(w, two(), Vec::new());
                    full(w);
                },
                join_confirm(true),
            ),
            scene(
                "69-add-computer-offer-mac",
                |w| {
                    state(w, two(), Vec::new());
                    offer_card(w);
                },
                mac(|w| {
                    go(Page::Add, 0, 1)(w);
                    w.global::<Ui>().set_camera_available(true);
                }),
            ),
            scene(
                "69-add-computer-offer-limit",
                |w| {
                    state(w, two(), Vec::new());
                    full(w);
                    offer_card(w);
                },
                go(Page::Add, 0, 1),
            ),
            scene(
                "70-paired-also-one-more",
                |w| {
                    let names = [
                        (1, "MacBook Air"),
                        (2, "MJStanton-Desktop"),
                        (3, "sdk_gphone16k_x86_64"),
                        (4, "Den-PC"),
                    ];
                    state(
                        w,
                        names
                            .iter()
                            .map(|(n, name)| device(*n, name, false, "connected", 0))
                            .collect(),
                        vec![],
                    );
                    linking(w, (1..=4).map(|n| link_of(n, "enabled")).collect(), &names);
                    paired_with(w, 1, "MacBook Air");
                },
                go(Page::Add, 0, 1),
            ),
            scene(
                "70-paired-also",
                |w| {
                    let mut desktop = device(3, "MJStanton-Desktop", false, "connecting", 0);
                    desktop.route = None;
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 0), desktop],
                        Vec::new(),
                    );
                    linking(
                        w,
                        vec![link_of(1, "enabled"), link_of(3, "enabled")],
                        &[(1, "Galaxy S25+"), (3, "MJStanton-Desktop")],
                    );
                    paired_with(w, 1, "Galaxy S25+");
                },
                mac(go(Page::Add, 0, 0)),
            ),
            scene(
                "70-paired-also-many",
                |w| {
                    state(w, trio(), Vec::new());
                    linking(
                        w,
                        [1, 2, 4, 5, 6]
                            .into_iter()
                            .map(|n| link_of(n, "enabled"))
                            .collect(),
                        &[
                            (1, "Galaxy S25+"),
                            (2, "MacBook Air"),
                            (4, "Office-PC"),
                            (5, "Living-Room-HTPC"),
                            (6, "Studio Mac mini"),
                        ],
                    );
                    paired_with(w, 2, "MacBook Air");
                },
                go(Page::Add, 0, 1),
            ),
            scene(
                "71-paired-only",
                |w| {
                    state(
                        w,
                        vec![device(1, "Galaxy S25+", true, "connected", 0)],
                        Vec::new(),
                    );
                    let separate = with(link_of(1, "disabled"), "outcome", json!(setup::SEPARATE));
                    linking(w, vec![separate], &[]);
                    paired_with(w, 1, "Galaxy S25+");
                },
                go(Page::Add, 0, 0),
            ),
        ]
    }
    #[cfg(target_os = "linux")]
    struct Chooser(
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<std::sync::atomic::AtomicU32>,
    );
    #[cfg(target_os = "linux")]
    struct Request(Arc<tokio::sync::Notify>);
    #[cfg(target_os = "linux")]
    #[zbus::interface(name = "org.freedesktop.portal.Request")]
    impl Request {
        fn close(&self) {
            self.0.notify_one();
        }
    }
    #[cfg(target_os = "linux")]
    #[zbus::interface(name = "org.freedesktop.portal.FileChooser")]
    impl Chooser {
        async fn open_file(
            &self,
            parent: &str,
            _title: &str,
            options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
            assert!(parent.is_empty());
            assert_eq!(options.len(), 4);
            for key in options.keys() {
                assert!(
                    matches!(
                        key.as_str(),
                        "handle_token" | "directory" | "multiple" | "current_folder"
                    ),
                    "unsupported FileChooser option: {key}"
                );
            }
            let token = String::try_from(options["handle_token"].try_clone().unwrap()).unwrap();
            let sender = header
                .sender()
                .unwrap()
                .as_str()
                .trim_start_matches(':')
                .replace('.', "_");
            let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
            connection
                .object_server()
                .at(path.clone(), Request(self.1.clone()))
                .await
                .unwrap();
            self.0.notify_one();
            let code = self.2.load(Ordering::SeqCst);
            if code != u32::MAX {
                let values = std::collections::HashMap::from([(
                    "uris",
                    zbus::zvariant::Value::from(vec!["file:///chosen-folder"]),
                )]);
                connection
                    .emit_signal(
                        None::<&str>,
                        path.as_str(),
                        "org.freedesktop.portal.Request",
                        "Response",
                        &(code, values),
                    )
                    .await
                    .unwrap();
            }
            Ok(path.try_into().unwrap())
        }
    }
    #[cfg(target_os = "linux")]
    struct OpenPortal {
        entered: Arc<tokio::sync::Notify>,
        closed: Arc<tokio::sync::Notify>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        observed: Arc<Mutex<(u64, Vec<u8>)>>,
    }
    #[cfg(target_os = "linux")]
    #[zbus::interface(name = "org.freedesktop.portal.OpenURI")]
    impl OpenPortal {
        async fn open_file(
            &self,
            _parent: &str,
            fd: zbus::zvariant::Fd<'_>,
            options: HashMap<String, zbus::zvariant::OwnedValue>,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
            use std::os::{fd::AsFd, unix::fs::FileExt};
            let file = std::fs::File::from(fd.as_fd().try_clone_to_owned().unwrap());
            let mut bytes = [0; 64];
            let count = file.read_at(&mut bytes, 0).unwrap();
            *self.observed.lock().unwrap() =
                (file.metadata().unwrap().len(), bytes[..count].to_vec());
            let token = String::try_from(options["handle_token"].try_clone().unwrap()).unwrap();
            let sender = header
                .sender()
                .unwrap()
                .as_str()
                .trim_start_matches(':')
                .replace('.', "_");
            let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
            connection
                .object_server()
                .at(path.clone(), Request(self.closed.clone()))
                .await?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            Ok(path.try_into().unwrap()) // Deliberately leave the chooser unanswered.
        }
    }
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn recent_open_keeps_polling_commands_and_close_responsive() -> Result<()> {
        if in_offscreen_child(
            "ui::window::tests::recent_open_keeps_polling_commands_and_close_responsive",
            &[],
        )
        .await?
        {
            return Ok(());
        }
        use tokio::io::AsyncBufReadExt;
        let dir = crate::test_support::scratch()?;
        let config = dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth><apparmor mode=\"disabled\"/><policy context=\"default\"><allow user=\"*\"/><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",
                dir.path().display()
            ),
        )?;
        let mut bus = tokio::process::Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(config)
            .args(["--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut address = String::new();
        tokio::io::BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .await?;
        // This is an isolated, single-test child process. No real desktop bus
        // or environment belonging to parallel tests is ever changed.
        unsafe {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address.trim());
        }
        let entered = Arc::new(tokio::sync::Notify::new());
        let closed = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::new(Mutex::new((0, Vec::new())));
        let _portal = zbus::connection::Builder::address(address.trim())?
            .name("org.freedesktop.portal.Desktop")?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                OpenPortal {
                    entered: entered.clone(),
                    closed: closed.clone(),
                    calls: calls.clone(),
                    observed: observed.clone(),
                },
            )?
            .build()
            .await?;
        headless()?;
        let window = PneuWindow::new()?;
        let (mut worker, _) = model_worker(&window, dir.path());
        // Exercise the actual Action::Open path for both Recent directions.
        // No original receive handle survives these edits/replacements.
        for direction in ["incoming", "outgoing"] {
            for scenario in ["edited", "replaced", "large", "deleted", "symlink"] {
                let folder = dir.path().join(format!("{direction}-{scenario}"));
                std::fs::create_dir(&folder)?;
                let path = folder.join("report.pdf");
                std::fs::write(&path, b"original")?;
                let mut row = transfer(id(9).as_str(), 1, "report.pdf", "sent", 8, 8);
                row.direction = Some(direction.into());
                row.path = Some(path.clone());
                let mut snapshot = serde_json::to_value(&worker.snapshot)?;
                snapshot["transfers"] = json!([{
                    "operation":row.operation,"transfer_id":row.transfer_id,"device":row.device,
                    "file_name":row.file_name,"state":row.state,"size":row.size,"sent":row.sent,
                    "direction":row.direction,"path":row.path
                }]);
                let snapshot: crate::ui::Snapshot = serde_json::from_value(snapshot)?;
                worker.transfers = snapshot.transfers.iter().map(Transfer::from).collect();
                match scenario {
                    "edited" => std::fs::write(&path, b"legitimate edit")?,
                    "replaced" => {
                        let replacement = folder.join("replacement");
                        std::fs::write(&replacement, b"replacement")?;
                        std::fs::rename(replacement, &path)?;
                    }
                    "large" => std::fs::File::options()
                        .write(true)
                        .open(&path)?
                        .set_len(crate::outbox::PEER_BYTES + 1)?,
                    "deleted" => std::fs::remove_file(&path)?,
                    "symlink" => {
                        let moved = folder.with_extension("moved");
                        std::fs::rename(&folder, &moved)?;
                        std::os::unix::fs::symlink(moved, &folder)?;
                    }
                    _ => unreachable!(),
                }
                let before = calls.load(Ordering::SeqCst);
                worker.act(Action::Open(artifact_key(&row))).await;
                if matches!(scenario, "deleted" | "symlink") {
                    let error = job_result(&mut worker.open_job).await.unwrap_err();
                    assert!(
                        error.is::<crate::actions::FileUnavailable>(),
                        "{direction} {scenario}: {error}"
                    );
                    assert_eq!(error.to_string(), "The file was moved or deleted");
                    assert_eq!(calls.load(Ordering::SeqCst), before);
                    worker.open_job.take();
                } else {
                    tokio::time::timeout(Duration::from_secs(3), async {
                        tokio::select! {
                            result = job_result(&mut worker.open_job) => panic!("unanswered portal completed: {result:?}"),
                            _ = entered.notified() => {}
                        }
                    }).await?;
                    let (size, bytes) = observed.lock().unwrap().clone();
                    match scenario {
                        "edited" => assert_eq!(bytes, b"legitimate edit"),
                        "replaced" => assert_eq!(bytes, b"replacement"),
                        "large" => assert_eq!(size, crate::outbox::PEER_BYTES + 1),
                        _ => unreachable!(),
                    }
                    assert_eq!(calls.load(Ordering::SeqCst), before + 1);
                    worker.open_job.take();
                    tokio::time::timeout(Duration::from_secs(2), closed.notified()).await?;
                }
            }
        }
        let previous_calls = calls.load(Ordering::SeqCst);
        let path = dir.path().join("report.pdf");
        std::fs::write(&path, b"test PDF")?;
        let mut row = transfer(id(9).as_str(), 1, "report.pdf", "sent", 10, 10);
        row.path = Some(path);
        let key = artifact_key(&row);
        worker.transfers = vec![row];
        tokio::time::timeout(
            Duration::from_millis(200),
            worker.act(Action::Open(key.clone())),
        )
        .await?;
        assert!(
            worker.open_job.is_some(),
            "Open must enqueue, not await, the interaction"
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                result = job_result(&mut worker.open_job) => { panic!("unanswered portal completed: {result:?}"); }
                _ = entered.notified() => {}
            }
        }).await?;
        assert!(futures_util::poll!(Box::pin(job_result(&mut worker.open_job))).is_pending());
        worker.act(Action::Open(key)).await; // Outstanding interaction has one bounded slot.
        let mock = mock_ipc(&worker.paths.socket, |_| (true, Value::Null))?;
        tokio::time::timeout(
            Duration::from_secs(1),
            worker.act(Action::SetFlag("queue_offline".into(), true)),
        )
        .await?;
        assert!(
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r["command"] == "set_setting")
        );
        let visible = AtomicBool::new(true);
        let status = status_value(&worker);
        tokio::time::timeout(Duration::from_secs(1), worker.refresh(status, &visible)).await??;
        assert!(worker.open_job.is_some());
        tokio::time::timeout(Duration::from_secs(1), worker.close(&visible)).await?;
        assert!(!visible.load(Ordering::SeqCst));
        assert!(worker.open_job.is_none());
        tokio::time::timeout(Duration::from_secs(2), closed.notified()).await?;
        assert_eq!(calls.load(Ordering::SeqCst), previous_calls + 1);
        bus.kill().await?;
        Ok(())
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn missing_folder_response_does_not_block_polling_and_cancel_closes_portal() -> Result<()>
    {
        use tokio::io::AsyncBufReadExt;
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth><apparmor mode=\"disabled\"/><policy context=\"default\"><allow user=\"*\"/><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",
                dir.path().display()
            ),
        )?;
        let mut bus = tokio::process::Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(config)
            .args(["--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut address = String::new();
        tokio::io::BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .await?;
        let entered = Arc::new(tokio::sync::Notify::new());
        let closed = Arc::new(tokio::sync::Notify::new());
        let code = Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
        let server = zbus::connection::Builder::address(address.trim())?
            .name("org.freedesktop.portal.Desktop")?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                Chooser(entered.clone(), closed.clone(), code.clone()),
            )?
            .build()
            .await?;
        let client = zbus::connection::Builder::address(address.trim())?
            .build()
            .await?;
        let mut job: Option<futures_util::future::BoxFuture<'_, Result<()>>> =
            Some(Box::pin(async {
                choose_folder_on(client.clone(), dir.path()).await?;
                Ok(())
            }));
        tokio::select! {result=job_result(&mut job)=>{result?;anyhow::bail!("missing response completed");}, _=entered.notified()=>{}}
        // Same select branch used by the live window: a missing modal response
        // must allow state/QR ticks and Close to cancel the outstanding request.
        assert!(
            futures_util::poll!(Box::pin(job_result(&mut job))).is_pending(),
            "missing response must return control to the window"
        );
        job.take();
        tokio::time::timeout(Duration::from_secs(2), closed.notified()).await?;
        for response in [0, 1, 2] {
            code.store(response, Ordering::SeqCst);
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                choose_folder_on(client.clone(), dir.path()),
            )
            .await?;
            match response {
                0 => assert_eq!(result?, Some(PathBuf::from("/chosen-folder"))),
                1 => assert_eq!(result?, None),
                _ => assert!(result.is_err()),
            }
        }
        // A disappeared/replaced portal must release the chooser slot promptly,
        // and a retry must use the new service instead of waiting on the old one.
        futures_util::FutureExt::now_or_never(entered.notified());
        code.store(u32::MAX, Ordering::SeqCst);
        job = Some(Box::pin(async {
            choose_folder_on(client.clone(), dir.path()).await?;
            Ok(())
        }));
        tokio::select! { result=job_result(&mut job)=>{result?;anyhow::bail!("missing response completed");}, _=entered.notified()=>{} }
        assert!(
            server
                .release_name("org.freedesktop.portal.Desktop")
                .await?
        );
        let replacement_closed = Arc::new(tokio::sync::Notify::new());
        let replacement_entered = Arc::new(tokio::sync::Notify::new());
        let replacement_code = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let replacement = zbus::connection::Builder::address(address.trim())?
            .name("org.freedesktop.portal.Desktop")?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                Chooser(
                    replacement_entered.clone(),
                    replacement_closed.clone(),
                    replacement_code.clone(),
                ),
            )?
            .build()
            .await?;
        let error = tokio::time::timeout(Duration::from_secs(2), job_result(&mut job))
            .await?
            .unwrap_err();
        assert!(error.to_string().contains("lost ownership"), "{error}");
        job.take();
        tokio::time::timeout(Duration::from_secs(2), closed.notified()).await?;
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                choose_folder_on(client.clone(), dir.path())
            )
            .await??,
            Some(PathBuf::from("/chosen-folder"))
        );
        assert!(
            futures_util::FutureExt::now_or_never(replacement_closed.notified()).is_none(),
            "old Close reached the replacement portal"
        );
        futures_util::FutureExt::now_or_never(replacement_entered.notified());
        replacement_code.store(u32::MAX, Ordering::SeqCst);
        job = Some(Box::pin(async {
            choose_folder_on(client.clone(), dir.path()).await?;
            Ok(())
        }));
        tokio::select! { result=job_result(&mut job)=>{result?;anyhow::bail!("missing response completed");}, _=replacement_entered.notified()=>{} }
        assert!(
            replacement
                .release_name("org.freedesktop.portal.Desktop")
                .await?
        );
        replacement
            .request_name("org.freedesktop.portal.Desktop")
            .await?;
        let error = tokio::time::timeout(Duration::from_secs(2), job_result(&mut job))
            .await?
            .unwrap_err();
        assert!(error.to_string().contains("lost ownership"), "{error}");
        job.take();
        tokio::time::timeout(Duration::from_secs(2), replacement_closed.notified()).await?;
        replacement_code.store(0, Ordering::SeqCst);
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                choose_folder_on(client.clone(), dir.path())
            )
            .await??,
            Some(PathBuf::from("/chosen-folder"))
        );
        drop(replacement);
        drop(server);
        bus.kill().await?;
        bus.wait().await?;
        Ok(())
    }
    #[test]
    fn qr_has_four_pixel_modules_and_four_module_white_quiet_zone() -> Result<()> {
        let invitation = pneu_proto::PairingUri {
            version: 1,
            name: "desktop-pc".into(),
            fingerprint: pneu_proto::Hash::new("ab".repeat(32))?,
            addresses: vec![
                "100.67.144.28:47821".into(),
                "192.168.1.20:47821".into(),
                "10.20.0.10:47821".into(),
            ],
            token: pneu_proto::Hash::new("cd".repeat(32))?,
            expires_at: 1_800_000_000_000,
        };
        for uri in ["pneu://pair?test".into(), invitation.build()?] {
            let code =
                qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M)?;
            let image = qr_pixels(&uri)?;
            let size = (code.width() + 8) * 4;
            assert_eq!((image.width(), image.height()), (size as u32, size as u32));
            for y in 0..size {
                for x in 0..size {
                    let in_code = (16..size - 16).contains(&x) && (16..size - 16).contains(&y);
                    let dark = in_code && code[((x - 16) / 4, (y - 16) / 4)] == qrcode::Color::Dark;
                    let index = (y * size + x) * 4;
                    let rgb = if dark { 0 } else { 255 };
                    assert_eq!(&image.as_bytes()[index..index + 4], &[rgb, rgb, rgb, 255]);
                }
            }
            if uri.starts_with("pneu://pair?v=") {
                assert!(
                    (260..=292).contains(&size),
                    "typical pairing QR is {size}px"
                );
                assert!(size + 60 <= 400, "QR must fit within the window padding");
            }
        }
        Ok(())
    }
    #[test]
    fn path_elision_preserves_both_ends() {
        assert_eq!(middle_elide("123456789", 6), "123…89");
        assert_eq!(middle_elide("short", 30), "short");
    }
}
