//! One supervised window process. Qt has a main-thread event loop, independent
//! from the daemon runtime; all settings and trust changes go through Core IPC.
use crate::{Config, Paths, ipc};
use anyhow::{Context, Result};
use slint::ComponentHandle;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

slint::slint! {
    import { Palette } from "std-widgets.slint";
    global Pneu {
        out property <bool> dark: Palette.color-scheme == ColorScheme.dark;
        out property <color> card: dark ? #2A2E32 : #FFFFFF;
        out property <color> card-border: dark ? #373B41 : #DCDEE1;
        out property <color> text: Palette.foreground;
        out property <color> text2: dark ? #A4A9B0 : #5E6369;
        out property <color> accent: #4F5BFF;
        out property <color> accent-soft: dark ? #2B305E : #E8EAFF;
        out property <color> on-accent-soft: dark ? #C7CCFF : #2F38C8;
        out property <color> button: dark ? #33373C : #F2F3F5;
        out property <color> button-border: dark ? #4A4F56 : #C8CBD0;
        out property <color> danger: dark ? #FF7A7A : #C9302F;
        out property <color> danger-fill: #D93D42;
        out property <color> danger-soft: dark ? #3A1F22 : #FBE9EA;
        out property <color> banner-border: #DA445359;
        out property <color> warn: dark ? #FFB547 : #B45309;
        out property <color> warn-soft: dark ? #3A2C14 : #FFF1DC;
        out property <color> dim: dark ? #00000073 : #14161A52;
    }
    enum ButtonKind { neutral, danger, row }
    component PneuButton inherits Rectangle {
        in property <string> text;
        in property <ButtonKind> kind: neutral;
        in property <bool> enabled: true;
        private property <bool> kb-focus;
        changed enabled => { if !root.enabled { root.kb-focus = false; } }
        callback clicked();
        min-width: label.preferred-width + 28px;
        max-width: kind == ButtonKind.row ? 10000px : self.min-width;
        horizontal-stretch: kind == ButtonKind.row ? 1 : 0;
        min-height: kind == ButtonKind.row ? 44px : 30px;
        max-height: self.min-height;
        border-radius: kind == ButtonKind.row ? 0px : 8px;
        background: kind == ButtonKind.row ? (touch.pressed ? Pneu.button : transparent)
            : (kind == ButtonKind.danger ? Pneu.danger-fill : Pneu.button);
        border-width: kind == ButtonKind.row ? 0px : 1px;
        border-color: fs.has-focus && root.kb-focus ? Pneu.accent : (kind == ButtonKind.danger ? Pneu.danger-fill : Pneu.button-border);
        accessible-role: button;
        accessible-enabled: root.enabled;
        accessible-label: text;
        accessible-action-default => { root.clicked(); }
        forward-focus: fs;
        // In-window dialogs do not get Slint's native popup focus event.
        public function focus-popup() {
            fs.focus();
            fs.focus-gained(FocusReason.popup-activation);
        }
        if fs.has-focus && root.kb-focus: Rectangle {
            x: -2px; y: -2px; width: parent.width + 4px; height: parent.height + 4px;
            border-radius: root.kind == ButtonKind.row ? 2px : 10px;
            border-width: 2px; border-color: #4F5BFF59;
        }
        Rectangle {
            border-radius: parent.border-radius;
            background: touch.pressed ? (Pneu.dark ? #FFFFFF14 : #00000014)
                : touch.has-hover ? (Pneu.dark ? #FFFFFF0A : #0000000A) : transparent;
        }
        label := Text {
            x: 14px; width: parent.width - 28px; height: parent.height;
            text: root.text; vertical-alignment: center;
            horizontal-alignment: root.kind == ButtonKind.row ? left : center;
            color: root.kind == ButtonKind.danger ? white : root.kind == ButtonKind.row ? Pneu.danger : Pneu.text;
            font-weight: root.kind == ButtonKind.row ? 600 : 400;
        }
        fs := FocusScope {
            enabled: root.enabled;
            focus-gained(reason) => { root.kb-focus = reason == FocusReason.tab-navigation || reason == FocusReason.popup-activation; }
            focus-lost(reason) => { root.kb-focus = false; }
            key-pressed(event) => {
                if event.text == Key.Return || event.text == " " { root.clicked(); return accept; }
                return reject;
            }
        }
        touch := TouchArea { enabled: root.enabled; clicked => { root.kb-focus = false; fs.focus(); root.clicked(); } }
    }
    component Card inherits Rectangle {
        background: Pneu.card; border-radius: 14px;
        border-width: 1px; border-color: Pneu.card-border;
    }
    component Row inherits HorizontalLayout {
        padding-top: 12px; padding-bottom: 12px; padding-left: 14px; padding-right: 14px;
        spacing: 12px;
    }
    component Capsule inherits Rectangle {
        in-out property <float> travel: 0.5;
        width: 20px; height: 9px; border-radius: 4.5px;
        background: Pneu.accent;
        in property <length> tube-width;
        x: (tube-width - self.width) * travel;
        y: (3px - self.height) / 2;
        opacity: min(1, min(travel * 8, (1 - travel) * 8));
        Rectangle { x: parent.width - 6px; width: 6px; height: parent.height; border-radius: 4.5px; background: #FFB547; }
    }
    component AnimatedCapsule inherits Capsule {
        travel: 0;
        init => { root.travel = 1; }
        animate travel { duration: 1400ms; easing: cubic-bezier(0.45, 0, 0.2, 1); iteration-count: -1; }
    }
    component PneuSpinner inherits Rectangle {
        in property <bool> motion;
        width: 30px; height: 30px; border-radius: 15px;
        border-width: 3px; border-color: Pneu.accent-soft;
        accessible-role: progress-indicator;
        Rectangle {
            transform-rotation: root.motion ? mod(animation-tick() / 800ms, 1) * 1turn : 0deg;
            Path {
                width: 30px; height: 30px; viewbox-width: 30; viewbox-height: 30;
                stroke: Pneu.accent; stroke-width: 3px; stroke-line-cap: round;
                commands: "M15 1.5 A13.5 13.5 0 0 1 28.5 15";
            }
        }
    }
    component Link inherits Rectangle {
        in property <bool> connected;
        in property <bool> receiving;
        in property <bool> paused;
        in property <bool> motion;
        height: 40px;
        Rectangle {
            x: 0px; y: 0px; width: 40px; height: 40px; border-radius: 12px;
            background: root.paused ? Pneu.warn-soft : root.connected ? Pneu.accent-soft : Pneu.button;
            Image { x: 9px; y: 9px; width: 22px; height: 22px;
                source: @image-url("../../../../assets/smartphone.svg");
                colorize: root.paused ? Pneu.warn : root.connected ? Pneu.on-accent-soft : Pneu.text2;
            }
        }
        tube := Rectangle {
            x: 52px; y: (parent.height - self.height) / 2; width: parent.width - 104px; height: 3px; border-radius: 1.5px;
            background: root.connected && !root.paused && !root.receiving
                ? @linear-gradient(90deg, #7381FF 0%, #4F5BFF 100%)
                : root.connected ? Pneu.card-border : transparent;
            if !root.connected: Rectangle {
                clip: true;
                for i in ceil(tube.width / 11px): Rectangle { x: i * 11px; y: 0px; width: 5px; height: 3px; background: Pneu.card-border; }
            }
            if root.receiving && !root.paused && root.motion: AnimatedCapsule { tube-width: tube.width; }
            if root.receiving && !root.paused && !root.motion: Capsule { tube-width: tube.width; }
            if root.paused: Rectangle {
                x: (parent.width - self.width) / 2; y: (parent.height - self.height) / 2;
                width: 28px; height: 28px; border-radius: 14px;
                border-width: 3px; border-color: Pneu.card; background: Pneu.warn-soft;
                Image { x: 7px; y: 7px; width: 14px; height: 14px; source: @image-url("../../../../assets/pause.svg"); colorize: Pneu.warn; }
            }
        }
        Rectangle {
            x: parent.width - self.width; y: 0px; width: 40px; height: 40px; border-radius: 12px;
            background: root.connected && !root.paused ? Pneu.accent-soft : Pneu.button;
            Image { x: 9px; y: 9px; width: 22px; height: 22px; source: @image-url("../../../../assets/computer.svg");
                colorize: root.connected && !root.paused ? Pneu.on-accent-soft : Pneu.text2;
            }
        }
    }
    export component PneuWindow inherits Window {
        title: "Pneu";
        icon: @image-url("../../../../assets/pneu.svg");
        // Keep the last real QR size across invitation/pairing transitions.
        private property <int> last-qr-size: 292;
        private property <length> qr-card-height: last-qr-size * 1px + 28px;
        private property <length> pairing-height: qr-card-height + 12px + qr-caption.preferred-height;
        private property <length> window-width: max(400px, last-qr-size * 1px + 60px);
        min-width: window-width; max-width: window-width;
        min-height: layout.preferred-height; max-height: layout.preferred-height;
        background: Palette.background;
        in property <bool> paired;
        in property <string> phone;
        in property <string> device;
        private property <string> confirmation-device;
        private property <string> confirmation-phone;
        in property <string> status;
        in property <bool> connected;
        in property <bool> receiving;
        in property <bool> paused;
        in property <bool> motion: true;
        in property <string> folder;
        in property <string> error;
        in property <image> qr;
        changed qr => { if root.qr.width > 0 { root.last-qr-size = root.qr.width; } }
        in property <string> pairing;
        in property <bool> network;
        in-out property <bool> confirming;
        callback close();
        callback choose-folder();
        callback unpair(string);
        public function prepare-unpair() {
            root.confirmation-device = root.device;
            root.confirmation-phone = root.phone;
            root.confirming = true;
        }
        public function submit-unpair() { root.unpair(root.confirmation-device); }
        qr-caption := Text { text: "Scan with Pneu on your phone"; font-size: 14px; visible: false; }
        name-line := Text { text: "Ag"; font-size: 15px; font-weight: 700; visible: false; }
        status-line := Text { text: "Ag"; visible: false; }
        forward-focus: keys;
        keys := FocusScope {
            focus-on-tab-navigation: false;
            forward-focus: change-button;
            key-pressed(event) => {
                if event.text == Key.Escape {
                    if root.confirming { root.confirming = false; change-button.focus(); } else { root.close(); }
                    return accept;
                }
                return reject;
            }
            layout := VerticalLayout {
                padding: 16px; spacing: 12px;
                if root.error != "": Rectangle {
                    border-radius: 10px; background: Pneu.danger-soft; border-color: Pneu.banner-border; border-width: 1px;
                    HorizontalLayout {
                        padding-top: 10px; padding-bottom: 10px; padding-left: 12px; padding-right: 12px; spacing: 8px;
                        Image { source: @image-url("../../../../assets/error.svg"); colorize: #DA4453; width: 16px; height: 16px; }
                        Text { text: root.error; color: Pneu.text; wrap: word-wrap; }
                    }
                }
                if !root.paired: VerticalLayout {
                    spacing: 12px;
                    if root.network && root.pairing == "": HorizontalLayout {
                        alignment: center;
                        Rectangle {
                            width: root.qr.width * 1px + 28px; height: root.qr.height * 1px + 28px;
                            border-radius: 16px; background: white;
                            border-width: Pneu.dark ? 1px : 0px; border-color: #FFFFFF0F;
                            drop-shadow-blur: Pneu.dark ? 0px : 12px; drop-shadow-offset-y: 3px; drop-shadow-color: #14161A0D;
                            Image { x: 14px; y: 14px; source: root.qr; width: root.qr.width * 1px; height: root.qr.height * 1px; image-rendering: pixelated; }
                        }
                    }
                    if root.pairing != "": VerticalLayout {
                        height: root.pairing-height; alignment: center; spacing: 16px;
                        HorizontalLayout { alignment: center; PneuSpinner { motion: root.motion; } }
                        Text { text: "Pairing with " + root.pairing + "…"; color: Pneu.text; horizontal-alignment: center; overflow: elide; max-height: status-line.preferred-height; }
                    }
                    if !root.network && root.pairing == "": VerticalLayout {
                        height: root.pairing-height; alignment: center; spacing: 16px;
                        HorizontalLayout { alignment: center; Image { source: @image-url("../../../../assets/monitor-off.svg"); colorize: Pneu.text2; width: 44px; height: 44px; } }
                        Text { text: "Connect this PC to a network to pair"; color: Pneu.text; horizontal-alignment: center; wrap: word-wrap; }
                    }
                    if root.network && root.pairing == "": Text { text: qr-caption.text; font-size: qr-caption.font-size; color: Pneu.text; horizontal-alignment: center; }
                }
                if root.paired: Card {
                    VerticalLayout {
                        padding-top: 16px; padding-bottom: 14px; padding-left: 14px; padding-right: 14px; spacing: 0px;
                        Link { connected: root.connected; receiving: root.receiving; paused: root.paused; motion: root.motion; }
                        Rectangle { height: 12px; }
                        Text { text: root.phone; font-size: 15px; font-weight: 700; color: Pneu.text; overflow: elide; max-height: name-line.preferred-height; horizontal-alignment: center; }
                        Text { text: root.status; color: Pneu.text2; overflow: elide; max-height: status-line.preferred-height; horizontal-alignment: center; }
                    }
                }
                Card {
                    VerticalLayout {
                        spacing: 0px;
                        Row {
                            VerticalLayout {
                                horizontal-stretch: 1; spacing: 2px;
                                Text { text: "Save to"; font-size: 12.5px; color: Pneu.text2; }
                                Text { text: root.folder; color: Pneu.text; horizontal-stretch: 1; overflow: elide; }
                            }
                            change-button := PneuButton { text: "Change…"; enabled: !root.confirming; clicked => { root.choose-folder(); } }
                        }
                        if root.paired: Rectangle { height: 1px; Rectangle { x: 14px; width: parent.width - 28px; height: 1px; background: Pneu.card-border; } }
                        if root.paired: PneuButton { text: "Unpair…"; kind: row; enabled: !root.confirming; clicked => { root.prepare-unpair(); } }
                    }
                }
            }
            if root.confirming: Rectangle {
                background: Pneu.dim;
                TouchArea {}
                popup-keys := FocusScope {
                    forward-focus: cancel;
                    focus-on-tab-navigation: false;
                    init => { cancel.focus-popup(); }
                    key-pressed(event) => {
                        if event.text == Key.Escape { root.confirming = false; change-button.focus(); return accept; }
                        return reject;
                    }
                    Card {
                        x: (parent.width - self.width) / 2; y: (parent.height - self.height) / 2;
                        width: 320px; height: max(0px, min(popup-layout.preferred-height, root.height - 32px));
                        drop-shadow-blur: 28px; drop-shadow-offset-y: 8px; drop-shadow-color: Pneu.dark ? #000000B3 : #14161A33;
                        popup-layout := VerticalLayout {
                            padding: 20px; spacing: 20px;
                            Text {
                                text: "Unpair " + root.confirmation-phone + "?";
                                font-size: 15px; font-weight: 700; color: Pneu.text;
                                overflow: elide;
                                // One elided line stays within the two-line limit. Slint's Qt
                                // renderer clips vertical wrap overflow without an ellipsis.
                                // Reserve padding, spacing and the action row even in a small window.
                                min-height: 0px;
                                max-height: max(0px, min(name-line.preferred-height, root.height - 122px));
                            }
                            HorizontalLayout {
                                min-height: 30px; max-height: 30px; spacing: 8px;
                                Rectangle { horizontal-stretch: 1; }
                                cancel := PneuButton { text: "Cancel"; clicked => { root.confirming = false; change-button.focus(); } }
                                PneuButton { text: "Unpair"; kind: danger; clicked => { root.confirming = false; change-button.focus(); root.submit-unpair(); } }
                            }
                        }
                    }
                }
            }
        }
    }
}
#[derive(Clone)]
enum Action {
    Close,
    Folder,
    Unpair(pneu_proto::Id),
}
pub fn run(paths: Paths) -> Result<()> {
    // Prefer Qt/native; selecting winit is an explicit fallback for Qt-less builds.
    use slint::winit_030::winit::{
        platform::startup_notify::WindowAttributesExtStartupNotify, window::ActivationToken,
    };
    // Slint applies this hook at adapter creation, not during later hide/show.
    // Notification activation therefore starts a fresh supervised window process.
    let startup_activation = std::cell::RefCell::new(std::env::var("XDG_ACTIVATION_TOKEN").ok());
    let startup_request = std::env::var("PNEU_WINDOW_REQUEST")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|request| request & 1 == 0)
        .unwrap_or(0);
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
    let window = PneuWindow::new()?;
    let visible = Arc::new(AtomicBool::new(true));
    let (commands, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let c = commands.clone();
    window.on_close(move || {
        let _ = c.send(Action::Close);
    });
    let c = commands.clone();
    window.on_choose_folder(move || {
        let _ = c.send(Action::Folder);
    });
    let c = commands.clone();
    window.on_unpair(move |device| {
        if let Ok(device) = pneu_proto::Id::new(device.as_str()) {
            let _ = c.send(Action::Unpair(device));
        }
    });
    let c = commands;
    window.window().on_close_requested(move || {
        let _ = c.send(Action::Close);
        slint::CloseRequestResponse::KeepWindowShown
    });
    let weak = window.as_weak();
    let worker = std::thread::Builder::new().name("pneu-window-ipc".into()).stack_size(8 * 1024 * 1024).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("UI runtime");
        runtime.block_on(Box::pin(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(500));
            let mut pairing_token: Option<pneu_proto::Hash> = None;
            let mut invitation_revision = 0;
            let mut invitation_job: Option<futures_util::future::BoxFuture<'static, Result<serde_json::Value>>> = None;
            let mut request = startup_request;
            let mut was_paired = false;
            let mut last_pair_attempt = 0;
            let mut failed = 0;
            let mut action_error = String::new();
            let mut folder_job: Option<futures_util::future::BoxFuture<'_, Result<()>>> = None;
            loop {
                if invitation_job.is_none() { let socket=paths.socket.clone(); let revision=invitation_revision; invitation_job=Some(Box::pin(async move {ipc::request(&socket, ipc::Command::WaitPairingChange {revision}).await})); }
                tokio::select! {
                    _ = async {invitation_job.as_mut().expect("invitation subscription").await} => {invitation_job=None;},
                    _ = timer.tick() => {},
                    result = folder_result(&mut folder_job) => { folder_job = None; if let Err(error) = result { action_error = error.to_string(); } },
                    command = rx.recv() => {
                        match command {
                            Some(Action::Close) => {
                                folder_job = None;
                                visible.store(false,Ordering::SeqCst);
                                if let Some(token)=pairing_token.take() {let _ = ipc::request(&paths.socket,ipc::Command::ClosePairingIf { token }).await;}
                                let _ = weak.upgrade_in_event_loop(|w| { let _ = w.hide(); });
                            }
                            Some(Action::Folder) => {
                                action_error.clear();
                                if folder_job.is_none() { folder_job = Some(Box::pin(change_folder(&paths))); }
                            }
                            Some(Action::Unpair(device)) => {
                                action_error.clear();
                                if let Err(error) = ipc::request(&paths.socket,ipc::Command::Unpair { device }).await { action_error = error.to_string(); }
                                pairing_token = None;
                            }
                            None => break,
                        }
                    }
                }
                let value = match ipc::request(&paths.socket,ipc::Command::UiStatus).await {
                    Ok(value) => { failed = 0; value },
                    Err(_) => { failed += 1; if failed >= 3 { break; } continue; }
                };
                let snapshot: super::Snapshot = match serde_json::from_value(value["snapshot"].clone()) { Ok(s) => s, Err(_) => break };
                invitation_revision = value["invitation_revision"].as_u64().unwrap_or(0);
                let current = value["window_request"].as_u64().unwrap_or(0);
                if current != request {
                    request = current; visible.store(true,Ordering::SeqCst);
                    let _ = weak.upgrade_in_event_loop(|w| { let _ = w.hide(); let _ = w.show(); });
                    if current & 1 != 0 && folder_job.is_none() { folder_job = Some(Box::pin(change_folder(&paths))); }
                }
                if !visible.load(Ordering::SeqCst) { continue; }
                let config: Config = match serde_json::from_value(value["config"].clone()) { Ok(c) => c, Err(_) => break };
                let now = crate::now_ms();
                let active = serde_json::from_value::<Option<pneu_proto::PairingUri>>(value["invitation"].clone()).unwrap_or(None);
                if !snapshot.paired {
                    if let Some(invitation)=active {
                        if (pairing_token.as_ref()!=Some(&invitation.token) || was_paired)
                            && let Ok(uri)=invitation.build() && let Ok(pixels)=qr_pixels(&uri) {
                            pairing_token=Some(invitation.token);
                            let _ = weak.upgrade_in_event_loop(move |w| {w.set_qr(slint::Image::from_rgba8(pixels));w.set_network(true);});
                        }
                    } else {
                        // Never show a URI which Core no longer accepts.
                        pairing_token=None;
                        let _ = weak.upgrade_in_event_loop(|w| w.set_network(false));
                        if snapshot.pairing.is_none() && now.saturating_sub(last_pair_attempt)>=3000 {
                            last_pair_attempt=now;
                            let _ = ipc::request(&paths.socket,ipc::Command::EnsurePair).await;
                        }
                    }
                }
                was_paired = snapshot.paired;
                let folder = middle_elide(&super::display_path(&config.save_path),30);
                let error = snapshot.error.as_ref().map(|e| e.banner()).unwrap_or_else(|| action_error.clone());
                let status = display_peer_text(&snapshot.window_status());
                let phone = display_peer_text(&snapshot.phone.name);
                let pairing = display_peer_text(snapshot.pairing.as_deref().unwrap_or_default());
                let motion = animations_enabled(&super::palette::globals());
                let device = snapshot.device.as_ref().map(|d| d.as_str()).unwrap_or_default().to_owned();
                let _ = weak.upgrade_in_event_loop(move |w| {
                    w.set_device(device.into());
                    w.set_paired(snapshot.paired); w.set_phone(phone.into()); w.set_status(status.into()); w.set_connected(snapshot.connected); w.set_receiving(snapshot.receiving && snapshot.connected); w.set_paused(snapshot.paused && snapshot.connected); w.set_motion(motion);
                    w.set_folder(folder.into()); w.set_error(error.into()); w.set_pairing(pairing.into());
                });
            }
            drop(invitation_job);
            if let Some(token)=pairing_token {let _ = ipc::request(&paths.socket,ipc::Command::ClosePairingIf { token }).await;}
            let _ = slint::quit_event_loop();
        }));
    });
    let worker = worker.context("start window IPC thread")?;
    window.show()?;
    slint::run_event_loop_until_quit()?;
    drop(window);
    let _ = worker.join();
    Ok(())
}
// Peer names are labels, not paragraphs. Keep their bytes on the wire and in
// storage unchanged while rendering all line separators as spaces in the window.
fn display_peer_text(text: &str) -> String {
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
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M)?;
    let modules = code.width();
    let scale = 4;
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
async fn folder_result(
    job: &mut Option<futures_util::future::BoxFuture<'_, Result<()>>>,
) -> Result<()> {
    match job {
        Some(job) => job.await,
        None => std::future::pending().await,
    }
}
async fn change_folder(paths: &Paths) -> Result<()> {
    let value = ipc::request(&paths.socket, ipc::Command::UiStatus).await?;
    let config: Config = serde_json::from_value(value["config"].clone())?;
    if let Some(folder) = choose_folder(&config.save_path).await? {
        ipc::request(&paths.socket, ipc::Command::SaveFolder { folder }).await?;
    }
    Ok(())
}
pub async fn choose_folder(current: &Path) -> Result<Option<PathBuf>> {
    choose_folder_on(zbus::Connection::session().await?, current).await
}
async fn choose_folder_on(connection: zbus::Connection, current: &Path) -> Result<Option<PathBuf>> {
    use std::{collections::HashMap, os::unix::ffi::OsStrExt};
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
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn confirmation_keeps_displayed_device_when_status_changes() -> Result<()> {
        // The native palette initialises Qt even with a software window.
        // Isolate Qt's offscreen setting from the user's desktop and parallel tests.
        if std::env::var_os("PNEU_CONFIRMATION_TEST_CHILD").is_none() {
            let status = tokio::time::timeout(Duration::from_secs(15),
                tokio::process::Command::new(std::env::current_exe()?)
                    .args(["--exact", "ui::window::tests::confirmation_keeps_displayed_device_when_status_changes", "--nocapture"])
                    .env("PNEU_CONFIRMATION_TEST_CHILD", "1")
                    .env("QT_QPA_PLATFORM", "offscreen")
                    .kill_on_drop(true)
                    .status(),
            ).await??;
            anyhow::ensure!(status.success(), "offscreen confirmation regression failed");
            return Ok(());
        }
        struct Headless;
        impl slint::platform::Platform for Headless {
            fn create_window_adapter(
                &self,
            ) -> std::result::Result<
                std::rc::Rc<dyn slint::platform::WindowAdapter>,
                slint::PlatformError,
            > {
                Ok(
                    slint::platform::software_renderer::MinimalSoftwareWindow::new(
                        slint::platform::software_renderer::RepaintBufferType::NewBuffer,
                    ),
                )
            }
        }
        slint::platform::set_platform(Box::new(Headless))?;
        let window = PneuWindow::new()?;
        let displayed = pneu_proto::Id::from_bytes([1; 16]);
        let replacement = pneu_proto::Id::from_bytes([2; 16]);
        window.set_phone("Same phone name".into());
        window.set_device(displayed.as_str().into());
        window.invoke_prepare_unpair();
        // Even equal labels must not allow a changed device ID to replace the target.
        window.set_device(replacement.as_str().into());
        window.set_phone("Same phone name".into());
        let submitted = std::rc::Rc::new(std::cell::RefCell::new(None));
        let target = submitted.clone();
        window.on_unpair(move |device| {
            *target.borrow_mut() = Some(device);
        });
        window.invoke_submit_unpair();
        assert_eq!(
            submitted.borrow().as_ref().unwrap().as_str(),
            displayed.as_str()
        );
        Ok(())
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
    struct Chooser(
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<std::sync::atomic::AtomicU32>,
    );
    struct Request(Arc<tokio::sync::Notify>);
    #[zbus::interface(name = "org.freedesktop.portal.Request")]
    impl Request {
        fn close(&self) {
            self.0.notify_one();
        }
    }
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
    #[tokio::test]
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
        tokio::select! {result=folder_result(&mut job)=>{result?;anyhow::bail!("missing response completed");}, _=entered.notified()=>{}}
        // Same select branch used by the live window: a missing modal response
        // must allow state/QR ticks and Close to cancel the outstanding request.
        tokio::select! {result=folder_result(&mut job)=>{result?;anyhow::bail!("missing response completed");}, _=tokio::time::sleep(Duration::from_millis(10))=>{}}
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
        tokio::select! { result=folder_result(&mut job)=>{result?;anyhow::bail!("missing response completed");}, _=entered.notified()=>{} }
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
        let error = tokio::time::timeout(Duration::from_secs(2), folder_result(&mut job))
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
        tokio::select! { result=folder_result(&mut job)=>{result?;anyhow::bail!("missing response completed");}, _=replacement_entered.notified()=>{} }
        assert!(
            replacement
                .release_name("org.freedesktop.portal.Desktop")
                .await?
        );
        replacement
            .request_name("org.freedesktop.portal.Desktop")
            .await?;
        let error = tokio::time::timeout(Duration::from_secs(2), folder_result(&mut job))
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
