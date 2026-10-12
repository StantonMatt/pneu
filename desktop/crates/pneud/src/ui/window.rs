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
    import { Button, Spinner, Palette } from "std-widgets.slint";
    export component PneuWindow inherits Window {
        title: "Pneu";
        icon: @image-url("../../../../assets/pneu.svg");
        private property <length> window-width: root.paired ? 400px : max(400px, root.qr.width * 1px + 36px);
        min-width: window-width; max-width: window-width;
        min-height: layout.preferred-height; max-height: layout.preferred-height;
        background: Palette.background;
        in property <bool> paired;
        in property <string> phone;
        in property <string> device;
        private property <string> confirmation-device;
        private property <string> confirmation-phone;
        in property <string> status;
        in property <color> dot;
        in property <bool> dot-muted;
        in property <string> folder;
        in property <string> error;
        in property <image> qr;
        in property <image> phone-icon;
        in property <image> network-icon;
        in property <image> error-icon;
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
        forward-focus: keys;
        keys := FocusScope {
            forward-focus: close-button;
            key-pressed(event) => {
                if event.text == Key.Escape {
                    if root.confirming { confirm.close(); root.confirming = false; } else { root.close(); }
                    return accept;
                }
                if event.text == Key.Return && !root.confirming { root.close(); return accept; }
                return reject;
            }
        layout := VerticalLayout {
            padding: 18px; spacing: 12px;
            if root.error != "": Rectangle {
                border-radius: 4px; background: #da445326; border-color: #da445380; border-width: 1px;
                HorizontalLayout {
                    padding: 8px; spacing: 8px;
                    Image { source: root.error-icon; width: 16px; height: 16px; }
                    Text { text: root.error; color: Palette.foreground; wrap: word-wrap; }
                }
            }
            if !root.paired: VerticalLayout {
                spacing: 12px;
                if root.network && root.pairing == "": HorizontalLayout {
                    Rectangle { horizontal-stretch: 1; }
                    Rectangle { width: root.qr.width * 1px; height: root.qr.height * 1px; border-radius: 6px; background: white; clip: true;
                        Image { source: root.qr; width: parent.width; height: parent.height; image-rendering: pixelated; }
                    }
                    Rectangle { horizontal-stretch: 1; }
                }
                if root.pairing != "": VerticalLayout {
                    height: max(280px, root.qr.height * 1px); alignment: center; spacing: 12px;
                    HorizontalLayout { alignment: center; Spinner { width: 32px; height: 32px; } }
                    Text { text: "Pairing with " + root.pairing + "…"; color: Palette.foreground; horizontal-alignment: center; }
                }
                if !root.network && root.pairing == "": VerticalLayout {
                    height: 280px; alignment: center; spacing: 12px;
                    HorizontalLayout { alignment: center; Image { source: root.network-icon; width: 48px; height: 48px; } }
                    Text { text: "Connect this PC to a network to pair"; color: Palette.foreground; horizontal-alignment: center; }
                }
                if root.network && root.pairing == "": Text { text: "Scan with Pneu on your phone"; color: Palette.foreground; horizontal-alignment: center; }
            }
            if root.paired: HorizontalLayout {
                spacing: 14px;
                Image { source: root.phone-icon; width: 48px; height: 48px; }
                VerticalLayout {
                    spacing: 6px;
                    Text { text: root.phone; font-size: root.default-font-size * 1.2; font-weight: 600; color: Palette.foreground; overflow: elide; }
                    HorizontalLayout { spacing: 6px;
                        Rectangle { width: 8px; height: 8px; border-radius: 4px; background: root.dot-muted ? Palette.foreground : root.dot; opacity: root.dot-muted ? 0.7 : 1; }
                        Text { text: root.status; color: Palette.foreground; opacity: 0.7; overflow: elide; }
                    }
                }
            }
            Rectangle { height: 1px; background: Palette.border; }
            HorizontalLayout {
                spacing: 6px;
                Text { text: "Save to:"; color: Palette.foreground; vertical-alignment: center; }
                Text { text: root.folder; color: Palette.foreground; horizontal-stretch: 1; overflow: elide; vertical-alignment: center; }
                Button { text: "Change…"; clicked => { root.choose-folder(); } }
            }
            if root.paired: Rectangle { height: 1px; background: Palette.border; }
            HorizontalLayout {
                if root.paired: Button { text: "Unpair…"; clicked => { root.prepare-unpair(); confirm.show(); } }
                Rectangle { horizontal-stretch: 1; }
                close-button := Button { text: "Close"; primary: true; clicked => { root.close(); } }
            }
        }
        if root.confirming: Rectangle { background: #00000055; TouchArea {} }
        confirm := PopupWindow {
            x: (root.width - self.width) / 2; y: (root.height - self.height) / 2;
            width: 330px; height: 114px; close-policy: no-auto-close;
            forward-focus: popup-keys;
            popup-keys := FocusScope {
                forward-focus: cancel;
                key-pressed(event) => {
                    if event.text == Key.Escape || event.text == Key.Return { confirm.close(); root.confirming = false; close-button.focus(); return accept; }
                    return reject;
                }
            Rectangle {
                background: Palette.background; border-radius: 4px; border-width: 1px; border-color: Palette.border;
                VerticalLayout {
                    padding: 18px; spacing: 18px;
                    Text { text: "Unpair " + root.confirmation-phone + "?"; color: Palette.foreground; overflow: elide; }
                    HorizontalLayout {
                        Rectangle { horizontal-stretch: 1; }
                        cancel := Button { text: "Cancel"; primary: true; clicked => { confirm.close(); root.confirming = false; close-button.focus(); } }
                        Button { text: "Unpair"; clicked => { confirm.close(); root.confirming = false; root.submit-unpair(); } }
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
    slint::BackendSelector::new().select()?;
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
            let mut request = 0;
            let mut was_paired = false;
            let mut last_pair_attempt = 0;
            let mut failed = 0;
            let mut action_error = String::new();
            let mut icon_theme = None;
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
                let dot = if snapshot.error.is_some() { slint::Color::from_rgb_u8(218,68,83) }
                    else if snapshot.paused { slint::Color::from_rgb_u8(246,116,0) }
                    else if snapshot.connected { slint::Color::from_rgb_u8(39,174,96) }
                    else { slint::Color::from_rgb_u8(127,127,127) };
                let status = snapshot.window_status();
                let ini = super::palette::globals();
                let icons = if icon_theme.as_ref() != Some(&ini) {
                    let icons = (theme_icon(&ini,"smartphone",48),theme_icon(&ini,"network-disconnect",48),theme_icon(&ini,"dialog-error",16));
                    icon_theme = Some(ini);
                    Some(icons)
                } else { None };
                let device = snapshot.device.as_ref().map(|d| d.as_str()).unwrap_or_default().to_owned();
                let _ = weak.upgrade_in_event_loop(move |w| {
                    w.set_device(device.into());
                    w.set_paired(snapshot.paired); w.set_phone(snapshot.phone.name.into()); w.set_status(status.into()); w.set_dot(dot); w.set_dot_muted(!snapshot.connected && snapshot.error.is_none());
                    w.set_folder(folder.into()); w.set_error(error.into()); w.set_pairing(snapshot.pairing.unwrap_or_default().into());
                    if let Some((phone_icon, network_icon, error_icon)) = icons {
                    if let Some((bytes,size)) = phone_icon { w.set_phone_icon(slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(&bytes,size,size))); }
                    if let Some((bytes,size)) = network_icon { w.set_network_icon(slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(&bytes,size,size))); }
                    if let Some((bytes,size)) = error_icon { w.set_error_icon(slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(&bytes,size,size))); }
                    }
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
fn theme_icon(ini: &str, name: &str, size: u32) -> Option<(Vec<u8>, u32)> {
    let active = super::palette::value(ini, "Icons", "Theme").unwrap_or("breeze");
    let palette = super::palette::colors(ini);
    for theme in [active, "breeze", "breeze-dark"] {
        if theme.contains('/') || theme.contains("..") {
            continue;
        }
        for category in ["devices", "status", "actions"] {
            for source_size in [64, 48, 32, 22, 24, 16] {
                let path = PathBuf::from(format!(
                    "/usr/share/icons/{theme}/{category}/{source_size}/{name}.svg"
                ));
                if let Ok(svg) = std::fs::read_to_string(path)
                    && let Ok(bytes) = render_theme_svg(&svg, &palette, size)
                {
                    return Some((bytes, size));
                }
            }
        }
    }
    None
}
fn render_theme_svg(svg: &str, palette: &super::palette::Colors, size: u32) -> Result<Vec<u8>> {
    let svg = svg
        .replace("#232629", &palette.foreground)
        .replace("#eff0f1", &palette.foreground)
        .replace("#da4453", &palette.negative);
    super::tray::render_svg(svg.as_bytes(), size)
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
        // Native Breeze widgets initialise Qt even with a software window.
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
    fn window_icons_follow_actual_palette_for_custom_dark_scheme() -> Result<()> {
        let svg = include_str!("../../../../assets/pneu-tray-disconnected.svg");
        for (ini, rgb) in [
            ("[Colors:Window]\nForegroundNormal=35,38,41\n", [35, 38, 41]),
            (
                "[General]\nColorScheme=CustomMidnight\n[Colors:Window]\nForegroundNormal=240,241,242\n",
                [240, 241, 242],
            ),
        ] {
            let bytes = render_theme_svg(svg, &super::super::palette::colors(ini), 48)?;
            assert!(bytes.chunks_exact(4).any(|p| p[3] > 240 && p[..3] == rgb));
        }
        Ok(())
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
            _parent: &str,
            _title: &str,
            options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
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
                assert!(size + 36 <= 400, "QR must fit within the window padding");
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
