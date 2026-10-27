//! Per-user Windows daemon integrations, with optional interactive presentation.
use super::{Command, window::SendRequest};
use crate::{Core, Event, actions::DesktopActions};
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use tokio::{
    process::Child,
    sync::{mpsc, watch},
};

pub(crate) fn require_ui(_core: &Core) -> Result<()> {
    super::clipboard::interactive()
        .map_err(|_| anyhow::anyhow!("desktop UI requires an interactive Windows session"))
}
pub async fn run(
    core: Arc<Core>,
    _actions: Arc<DesktopActions>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let interactive = require_ui(&core).is_ok();
    let (commands, mut rx) = mpsc::channel(32);
    let tray = if interactive {
        match super::tray::start(core.ui_snapshot(), core.config(), commands).await {
            Ok(tray) => Some(tray),
            Err(error) => {
                eprintln!("pneud: tray unavailable: {error:#}");
                None
            }
        }
    } else {
        eprintln!("pneud: tray and window unavailable: no interactive Windows session");
        None
    };
    let mut events = core.subscribe();
    let mut watcher = if interactive {
        super::clipboard::Watch::start(core.clone()).ok()
    } else {
        None
    };
    let mut window: Option<Child> = None;
    let mut last_request = 0;
    let mut jobs = tokio::task::JoinSet::new();
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    if interactive && core.paired_devices().is_empty() {
        core.request_window(false);
    }
    while !*stop.borrow() {
        let mut update_tray = true;
        tokio::select! {
            _ = stop.changed() => break,
            _ = timer.tick() => {
                if watcher.as_ref().is_some_and(|w| w.is_finished()) {
                    if let Some(watcher) = watcher.take() { watcher.shutdown().await; }
                    eprintln!("pneud: clipboard watcher stopped; reconnecting");
                }
                if interactive && watcher.is_none() { watcher = super::clipboard::Watch::start(core.clone()).ok(); }
            }
            Some(_) = jobs.join_next(), if !jobs.is_empty() => {},
            Some(command) = rx.recv() => match command {
                Command::Window { .. } => core.request_window(false),
                Command::ChooseFolder => core.request_window(true),
                Command::AddDevice { .. } => core.request_add_device_with_activation(None),
                Command::Quit => core.request_shutdown(),
                Command::OpenFolder => {
                    let folder = core.config().save_path;
                    jobs.spawn(async move {
                        if let Err(error) = crate::actions::open_folder(&folder).await {
                            eprintln!("pneud: couldn't open received files: {error:#}");
                        }
                    });
                }
                Command::SendFiles { recipients, targeted } => {
                    if let Err(error) = core.present_send(SendRequest { id: crate::random_hex(16), paths: None, targets: recipients, clipboard: false, targeted, error: None }) { eprintln!("pneud: send unavailable: {error:#}"); }
                }
                Command::SendClipboard(recipients) => {
                    if let Err(error) = core.present_send(SendRequest { id: crate::random_hex(16), paths: None, targets: recipients, clipboard: true, targeted: false, error: None }) { eprintln!("pneud: send unavailable: {error:#}"); }
                }
                command => {
                    let core = core.clone();
                    jobs.spawn(async move {
                        let result = match command {
                            Command::Toggle(toggle) => core.toggle_setting(toggle).await,
                            Command::ClipboardMode(mode) => core.set_setting("clipboard_mode".into(), serde_json::json!(mode)).await,
                            Command::RetryTransfer { operation, artifact, device } => core.retry_transfer_artifact(operation, Some(artifact), device).await,
                            _ => unreachable!(),
                        };
                        if let Err(error) = result { eprintln!("pneud: tray action: {error:#}"); let _ = core.present_send_error(error.to_string()); }
                    });
                }
            },
            event = events.recv() => match event {
                Ok(Event::TransferProgress { .. }) => update_tray = false,
                Ok(event) => clipboard_policy_event(&core, &event),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for device in core.paired_devices() { core.cancel_automatic_clipboard(&device); }
                }
            }
        }
        if let Some(child) = window.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => window = None,
                Ok(None) => {}
                Err(error) => eprintln!("pneud: window status: {error}"),
            }
        }
        let request = core.window_request_with_activation().0;
        if interactive && request != last_request {
            last_request = request;
            if let Some(child) = window.as_ref() {
                if let Some(pid) = child.id() {
                    // The tray click gives the daemon foreground rights; pass
                    // them to the retained child before it presents this request.
                    unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
                    }
                }
            } else {
                match super::window_process::launch(request, None) {
                    Ok(child) => window = Some(child),
                    Err(error) => eprintln!("pneud: window unavailable: {error:#}"),
                }
            }
        }
        if update_tray && let Some(tray) = &tray {
            tray.update(core.ui_snapshot(), core.config());
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    if let Some(mut child) = window {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    if let Some(watcher) = watcher {
        watcher.shutdown().await;
    }
    if let Some(tray) = tray {
        tray.shutdown().await;
    }
    core.close_pairing().await;
    Ok(())
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
