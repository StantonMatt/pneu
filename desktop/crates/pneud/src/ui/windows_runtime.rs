//! Headless Windows integrations; tray and window hosting come later.
use crate::{Core, Event, actions::DesktopActions};
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

pub(crate) fn require_ui(_core: &Core) -> Result<()> {
    anyhow::bail!("desktop UI is not yet supported on Windows")
}
pub async fn run(
    core: Arc<Core>,
    _actions: Arc<DesktopActions>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let mut events = core.subscribe();
    let mut watcher = match super::clipboard::Watch::start(core.clone()) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            eprintln!("pneud: clipboard watch unavailable: {error}");
            None
        }
    };
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    while !*stop.borrow() {
        tokio::select! {
            _ = stop.changed() => break,
            _ = timer.tick() => {
                if watcher.as_ref().is_some_and(|w| w.is_finished()) {
                    if let Some(watcher) = watcher.take() { watcher.shutdown().await; }
                    eprintln!("pneud: clipboard watcher stopped; reconnecting");
                }
                if watcher.is_none() { watcher = super::clipboard::Watch::start(core.clone()).ok(); }
            }
            event = events.recv() => match event {
                Ok(event) => clipboard_policy_event(&core, &event),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for device in core.paired_devices() { core.cancel_automatic_clipboard(&device); }
                }
            }
        }
    }
    if let Some(watcher) = watcher {
        watcher.shutdown().await;
    }
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
