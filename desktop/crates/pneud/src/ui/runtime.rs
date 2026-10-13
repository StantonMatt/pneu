use crate::{
    Core, Event,
    actions::DesktopActions,
    ui::{Command, notifications::Notice},
};
use anyhow::Result;
use futures_util::StreamExt;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    process::Child,
    sync::{mpsc, watch},
};

pub async fn run(
    core: Arc<Core>,
    actions: Arc<DesktopActions>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    Box::pin(async move {

    let (commands, mut rx) = mpsc::channel(32);
    actions.set_commands(commands.clone());
    let mut events = core.subscribe();
    let mut tray = match super::tray::start(
        core.ui_snapshot(),
        core.config(),
        &core.paths.data,
        commands,
    )
    .await
    {
        Ok(tray) => Some(tray),
        Err(error) => {
            eprintln!("pneud: tray unavailable: {error}");
            None
        }
    };
    let watcher = match super::clipboard::Watch::start(core.clone(), actions.echo.clone()) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            eprintln!("pneud: clipboard watch unavailable: {error}");
            None
        }
    };
    let mut window: Option<Child> = None;
    let mut folder_open: Option<futures_util::future::BoxFuture<'static, Result<()>>> = None;
    let mut last_request = 0;
    let mut last_blocked = None;
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    let mut themes = ThemeRefresh::new(Instant::now());
    let (theme_tx, mut theme_rx) = mpsc::channel(1);
    let theme_signals = tokio::spawn(Box::pin(async move {
        let Ok(bus) = zbus::Connection::session().await else {
            return;
        };
        let Ok(rule) = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.kde.KGlobalSettings")
            .and_then(|b| b.member("notifyChange"))
            .map(|b| b.build())
        else {
            return;
        };
        let Ok(mut signals) = zbus::MessageStream::for_match_rule(rule, &bus, Some(4)).await else {
            return;
        };
        while signals.next().await.is_some() {
            let _ = theme_tx.try_send(());
        }
    }));
    if core.paired_devices().is_empty() {
        core.request_window(false);
    }
    loop {
        if *stop.borrow() {
            break;
        }
        let mut refresh_icons = false;
        let mut update_tray = true;
        tokio::select! {
            _=stop.changed()=>break,
            result=async {folder_open.as_mut().expect("folder open").await},if folder_open.is_some()=>{
                folder_open=None;
                if let Err(error)=result { eprintln!("pneud: open folder unavailable: {error}"); let mut notice=Notice::new("Couldn't open folder"); notice.body=error.to_string(); let _=actions.notice(notice,Vec::new(),None,0,None).await; }
            },
            _=timer.tick()=>{refresh_icons = themes.timer(Instant::now());},
            Some(())=theme_rx.recv()=>{themes.changed(Instant::now());refresh_icons=true;},
            event=events.recv()=>match event {
                Ok(Event::Error {message,..})=>eprintln!("pneud: {message}"),
                Ok(Event::Blocked {address})=>{
                    if last_blocked.is_none_or(|time:Instant|time.elapsed()>=Duration::from_secs(3600)) {
                        last_blocked=Some(Instant::now());let _=actions.notice(Notice::blocked(&address),Vec::new(),None,0,None).await;
                    }
                }
                Ok(Event::RemoteUnpaired {name})=>{let _=actions.notice(Notice::unpaired(&name),Vec::new(),None,0,None).await;},
                Ok(event @ (Event::ConnectionChanged {..}|Event::PhonePaused {..}|Event::ConfigChanged))=>clipboard_policy_event(&core,&event),
                Err(tokio::sync::broadcast::error::RecvError::Closed)=>break,
                Ok(event)=>update_tray=event_updates_tray(&event),
                Err(_)=>update_tray=false,
            },
            command=rx.recv()=>match command {
                Some(Command::Window { activation_token })=>core.request_window_with_activation(false, activation_token),
                Some(Command::ChooseFolder)=>core.request_window(true),
                Some(Command::Quit)=>core.request_shutdown(),
                Some(Command::Toggle(toggle))=>{
                    if let Err(error)=core.toggle_setting(toggle).await {eprintln!("pneud: settings unavailable: {error}");}
                }
                Some(Command::OpenFolder)=>{
                    if folder_open.is_none() { let folder=core.config().save_path; folder_open=Some(Box::pin(async move {open_folder(&folder).await})); }
                }
                None=>break,
            },
        }
        if !update_tray { continue; }
        if let Some(child) = window.as_mut()
            && child.try_wait()?.is_some()
        {
            window = None;
        }
        let (request, activation_token) = core.window_request_with_activation();
        if request != last_request {
            last_request = request;
            let startup_token = window_startup_token(request, activation_token.as_deref());
            replace_window_for_activation(&mut window, startup_token).await?;
            if window.is_none()
                && (std::env::var_os("WAYLAND_DISPLAY").is_some()
                    || std::env::var_os("DISPLAY").is_some())
            {
                match super::activation::command(std::env::current_exe()?, startup_token)
                    .arg("ui-window")
                    .env("PNEU_WINDOW_REQUEST", request.to_string())
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(child) => window = Some(child),
                    Err(error) => eprintln!("pneud: window unavailable: {error}"),
                }
            }
        }
        if let Some(tray) = &mut tray {
            let snapshot = core.ui_snapshot();
            let config = core.config();
            let pixmaps = if refresh_icons {
                super::tray::pixmaps().ok()
            } else {
                None
            };
            tray.update(move |tray| {
                tray.snapshot = snapshot;
                tray.config = config;
                if let Some(pixmaps) = pixmaps {
                    tray.pixmaps = pixmaps;
                }
            })
            .await;
        }
    }
    drop(folder_open);
    theme_signals.abort();
    let _ = theme_signals.await;
    if let Some(mut window) = window {
        let _ = window.kill().await;
        let _ = window.wait().await;
    }
    if let Some(watcher) = watcher {
        watcher.shutdown().await;
    }
    if let Some(tray) = tray {
        tray.shutdown().await;
    }
    core.close_pairing().await;
    Ok(())

        }).await
}

// A supplied credential must exist before Slint creates the adapter. There is
// no API in Slint 1.15.1 to set it on an existing Qt or winit Wayland window.
fn window_startup_token(request: u64, token: Option<&str>) -> Option<&str> {
    token.filter(|token| request & 1 == 0 && !token.is_empty())
}
async fn replace_window_for_activation(
    window: &mut Option<Child>,
    token: Option<&str>,
) -> Result<()> {
    if token.is_some()
        && let Some(child) = window.as_mut()
    {
        child.kill().await?;
        child.wait().await?;
        *window = None;
    }
    Ok(())
}

struct ThemeRefresh {
    next: Instant,
}
impl ThemeRefresh {
    fn new(now: Instant) -> Self {
        Self {
            next: now + Duration::from_secs(5),
        }
    }
    fn timer(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.changed(now);
        true
    }
    fn changed(&mut self, now: Instant) {
        self.next = now + Duration::from_secs(5);
    }
}
fn event_updates_tray(event: &Event) -> bool {
    matches!(
        event,
        Event::TransferStarted { .. }
            | Event::TransferFinished { .. }
            | Event::Paired { .. }
            | Event::Unpaired { .. }
            | Event::PairingProgress { .. }
    )
}

async fn open_folder(folder: &std::path::Path) -> Result<()> {
    tokio::fs::create_dir_all(folder).await?;
    let uri = url::Url::from_directory_path(folder)
        .map_err(|_| anyhow::anyhow!("folder is not absolute"))?;
    let portal = async {
        let bus = zbus::Connection::session().await?;
        crate::actions::open_uri_portal(uri.as_str(), &bus, None).await
    };
    open_folder_fallback(portal, || async {
        let status = super::activation::command("xdg-open", None)
            .arg(folder)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .await?;
        anyhow::ensure!(status.success(), "folder opener exited unsuccessfully");
        Ok(())
    })
    .await
}
async fn open_folder_fallback<P, L, F>(portal: P, launch: L) -> Result<()>
where
    P: std::future::Future<Output = Result<()>>,
    L: FnOnce() -> F,
    F: std::future::Future<Output = Result<()>>,
{
    super::portal::with_fallback(portal, launch).await
}

pub(crate) fn clipboard_policy_event(core: &Core, event: &Event) {
    match event {
        Event::ConnectionChanged {
            device,
            connected: false,
        }
        | Event::PhonePaused {
            device,
            paused: true,
        } => {
            if !core.connected_devices().contains(device) || core.phone_paused(device) == Some(true)
            {
                core.cancel_automatic_clipboard(device);
            }
        }
        Event::ConfigChanged if !core.config().clipboard_sync_to_phone => {
            for device in core.paired_devices() {
                core.cancel_automatic_clipboard(&device);
            }
        }
        _ => {}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn activation_reaps_pair_window_and_ignores_chooser_token() -> Result<()> {
        let mut window = Some(
            tokio::process::Command::new("sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()?,
        );
        let pid = window.as_ref().unwrap().id().unwrap();
        for token in [None, Some("")] {
            replace_window_for_activation(&mut window, window_startup_token(2, token)).await?;
            assert_eq!(window.as_ref().unwrap().id(), Some(pid));
        }
        let chooser_token = window_startup_token(3, Some("chooser"));
        assert!(chooser_token.is_none());
        replace_window_for_activation(&mut window, chooser_token).await?;
        assert_eq!(window.as_ref().unwrap().id(), Some(pid));
        replace_window_for_activation(&mut window, window_startup_token(4, Some("pair"))).await?;
        assert!(window.is_none());
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "old child must be reaped before relaunch"
        );
        // Cold launches need no previous child; their token is still passed on.
        let token = window_startup_token(6, Some("startup"));
        replace_window_for_activation(&mut window, token).await?;
        let output = super::super::activation::command("sh", token)
            .args([
                "-c",
                "printf '%s/%s' \"$XDG_ACTIVATION_TOKEN\" \"$DESKTOP_STARTUP_ID\"",
            ])
            .output()
            .await?;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"startup/startup");
        Ok(())
    }
    #[test]
    fn progress_bursts_do_not_render_or_advance_theme_schedule() -> Result<()> {
        let start = Instant::now();
        let mut themes = ThemeRefresh::new(start);
        let progress = Event::TransferProgress {
            device: pneu_proto::Id::new("a".repeat(32))?,
            transfer_id: pneu_proto::Id::new("b".repeat(32))?,
            received: 1,
            total: 10,
        };
        for _ in 0..10000 {
            assert!(!event_updates_tray(&progress));
        }
        assert!(!themes.timer(start + Duration::from_secs(4)));
        assert!(themes.timer(start + Duration::from_secs(5)));
        assert!(!themes.timer(start + Duration::from_secs(5)));
        themes.changed(start + Duration::from_secs(6));
        assert!(!themes.timer(start + Duration::from_secs(10)));
        assert!(themes.timer(start + Duration::from_secs(11)));
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn absent_failed_or_unresponsive_portal_uses_fallback() -> Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        for portal in [false, true] {
            open_folder_fallback(
                async {
                    if portal {
                        super::super::portal::accept(std::future::pending())
                            .await
                            .map(|_| ())
                    } else {
                        anyhow::bail!("portal service absent")
                    }
                },
                || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await?;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            open_folder_fallback(async { anyhow::bail!("absent") }, || async {
                anyhow::bail!("opener failed")
            })
            .await
            .is_err()
        );
        open_folder_fallback(async { Ok(()) }, || async {
            panic!("successful portal must not launch fallback")
        })
        .await?;
        Ok(())
    }
}
