//! M1 daemon runtime: clipboard only, no AppKit application/UI event loop.
use crate::{Core, Event, actions::DesktopActions};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::watch;
/// M2 enables these requests when the persistent UI child implements them.
pub(crate) fn require_ui() -> Result<()> {
    anyhow::bail!("window, tray and pickers unavailable on this platform yet (macOS M1)")
}

pub async fn run(
    core: Arc<Core>,
    _actions: Arc<DesktopActions>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let watcher = super::clipboard::Watch::start(core.clone())?;
    let mut events = core.subscribe();
    let mut request = core.window_request_with_activation().0;
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
    eprintln!(
        "pneud: running headless; tray, window and pickers unavailable on this platform yet (macOS M1)"
    );
    loop {
        if *stop.borrow() {
            break;
        }
        tokio::select! {
            _ = stop.changed() => break,
            _ = timer.tick() => {
                let next = core.window_request_with_activation().0;
                if request != next { request = next; eprintln!("pneud: window/pickers unavailable on this platform yet (macOS M1)"); }
            }
            event = events.recv() => match event {
                Ok(Event::Error { message, .. }) => eprintln!("pneud: {message}"),
                Ok(event) => clipboard_policy_event(&core, &event),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { for device in core.paired_devices() { core.cancel_automatic_clipboard(&device); } },
            }
        }
    }
    watcher.shutdown().await;
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
