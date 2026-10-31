//! Supervise one persistent AppKit child; failures never stop Core or transport.
use super::macos::bridge::Session;
use crate::ui::notifications::Notice;
use crate::{Core, Event, actions::DesktopActions};
use anyhow::{Context, Result};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::{process::Child, sync::watch};

pub(crate) fn require_ui(core: &Core) -> Result<()> {
    anyhow::ensure!(
        core.ui_ready
            .lock()
            .expect("UI readiness")
            .load(Ordering::Acquire),
        "Pneu's menu bar is unavailable; waiting for the UI child"
    );
    Ok(())
}
struct Supervised {
    child: Child,
    channel: Arc<Session>,
    started: Instant,
    established: bool,
    swept: bool,
    _cache: Option<crate::ui::viewer_cache::Lease>,
}
fn spawn(
    events: tokio::sync::mpsc::Sender<(u32, String, String)>,
    core: &Core,
) -> Result<Supervised> {
    let cache = crate::ui::viewer_cache::Lease::new(&core.paths.cache)
        .and_then(|lease| Ok((lease.path()?, lease)))
        .map_err(|error| eprintln!("pneud: received-file viewer cache unavailable; menu bar remains available: {error:#}"))
        .ok();
    let (child, parent, token) = crate::ui::child_supervision::spawn(
        &std::env::current_exe()?,
        &["ui-window"],
        core.window_request(),
        cache
            .as_ref()
            .map(|(path, lease)| (path.as_path(), &lease.lock)),
    )
    .context("start menu bar child")?;
    let channel = Session::start(parent, token, events)?;
    channel.activate();
    *core.ui_ready.lock().expect("UI readiness") = channel.ready.clone();
    Ok(Supervised {
        child,
        channel,
        started: Instant::now(),
        established: false,
        swept: false,
        _cache: cache.map(|(_, lease)| lease),
    })
}
pub async fn run(
    core: Arc<Core>,
    actions: Arc<DesktopActions>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let watcher = super::clipboard::Watch::start(core.clone())?;
    if core.paired_devices().is_empty() {
        core.request_window(false);
    }
    let mut events = core.subscribe();
    let (action_tx, mut action_rx) = tokio::sync::mpsc::channel(32);
    let mut child: Option<Supervised> = None;
    let mut restart = crate::ui::child_supervision::Restart::new(Instant::now());
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    let mut interactions = crate::ui::portal::Interactions::new();
    let mut last_blocked: Option<Instant> = None;
    let mut notices = tokio::task::JoinSet::new();
    let mut requests = Some(crate::ui::window_setup::Requests::default());
    let mut request_job: Option<
        futures_util::future::BoxFuture<'static, crate::ui::window_setup::Requests>,
    > = None;
    loop {
        if *stop.borrow() {
            break;
        }
        let mut sync_requests = true;
        tokio::select! {
            state = async { request_job.as_mut().expect("link request job").await }, if request_job.is_some() => {
                request_job = None;
                requests = Some(state);
                sync_requests = false;
            },

            _=stop.changed()=>break,
            Some(_)=notices.join_next(),if !notices.is_empty()=>{},
            _=timer.tick()=>{
                let failed=if let Some(ui)=child.as_mut() {
                    if ui.channel.ready.load(Ordering::Acquire) {
                        ui.established=true;
                        // A killed child cannot run notification destructors.
                        // The replacement removes stale IDs from earlier daemon
                        // namespaces too, preserving newly issued live records.
                        if !ui.swept && let Ok(wait) = actions.clear_stale_native_notifications() {
                            ui.swept = true;
                            notices.spawn(async move { let _ = wait.await; });
                            // Joined notices left from before the menu bar was ready.
                            queue_joined(&mut notices, core.clone(), actions.clone());
                        }
                    }
                    match ui.child.try_wait() {
                        Ok(Some(_))=>true,
                        Err(error)=>{eprintln!("pneud: UI child wait: {error}");true},
                        Ok(None)=> !ui.channel.ready.load(Ordering::Acquire) && (ui.established || ui.started.elapsed()>Duration::from_secs(15)),
                    }
                } else {false};
                if failed && let Some(mut ui)=child.take() {
                    ui.channel.close();
                    crate::ui::child_supervision::stop(&mut ui.child, Duration::from_secs(3)).await;
                    actions.clear_notifications().await;
                    request_job = None;
                    requests = Some(crate::ui::window_setup::Requests::default());
                    restart.failed(Instant::now(),ui.started.elapsed());
                    eprintln!("pneud: menu bar child stopped; restarting with backoff");
                }
                if child.is_none() && Instant::now()>=restart.next {
                    match spawn(action_tx.clone(),&core) {
                        Ok(ui)=>{child=Some(ui);},
                        Err(error)=>{eprintln!("pneud: menu bar unavailable: {error:#}"); restart.failed(Instant::now(),Duration::ZERO);}
                    }
                }
                // The window worker alone consumes window_request. A second
                // native ShowWindow delivery can arrive after the user closes
                // a newly shown window and reopen it for the same request.
            }
            action=action_rx.recv()=>if let Some((id,namespace,action))=action {
                if id==0 && namespace.is_empty() && action=="quit" {core.request_shutdown();}
                else if interactions.available() {
                    let core=core.clone(); let actions=actions.clone();
                    interactions.push(async move {if let Err(error)=actions.notification_action(&core,id,&namespace,&action).await {eprintln!("pneud: notification action: {error:#}");}});
                }
            },
            event=events.recv()=>match event {
                Ok(Event::Error {message,..})=>eprintln!("pneud: {message}"),
                Ok(Event::Blocked {address})=>{
                    if last_blocked.is_none_or(|time|time.elapsed()>=Duration::from_secs(3600)) {
                        last_blocked=Some(Instant::now());queue_notice(&mut notices,actions.clone(),Notice::blocked(&address));
                    }
                }
                Ok(Event::ComputerPaired {name,..})=>if !core.window_focused(){queue_notice(&mut notices,actions.clone(),Notice::paired(&name));},
                Ok(Event::RemoteUnpaired {name})=>queue_notice(&mut notices,actions.clone(),Notice::unpaired(&name)),
                Ok(event @ Event::Paired {..})=>{queue_joined(&mut notices,core.clone(),actions.clone());clipboard_policy_event(&core,&event);},
                Ok(event)=>clipboard_policy_event(&core,&event),
                Err(tokio::sync::broadcast::error::RecvError::Closed)=>break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>for device in core.paired_devices() {core.cancel_automatic_clipboard(&device);},
            }
        }

        if sync_requests
            && require_ui(&core).is_ok()
            && request_job.is_none()
            && let Some(mut state) = requests.take()
        {
            let core = core.clone();
            let actions = actions.clone();
            request_job = Some(Box::pin(async move {
                state.sync(&core, &actions).await;
                state
            }));
        }
    }
    notices.abort_all();
    while notices.join_next().await.is_some() {}
    interactions.shutdown().await;
    if let Some(mut ui) = child {
        ui.channel.close();
        crate::ui::child_supervision::stop(&mut ui.child, Duration::from_secs(3)).await;
    }
    actions.clear_notifications().await;
    watcher.shutdown().await;
    Ok(())
}
/// "{device} joined" for each device another of your devices added.
fn queue_joined(
    jobs: &mut tokio::task::JoinSet<()>,
    core: Arc<Core>,
    actions: Arc<DesktopActions>,
) {
    // The existing child-ready pass queues these again after startup.
    if require_ui(&core).is_err() {
        return;
    }
    while jobs.try_join_next().is_some() {}
    if jobs.len() >= crate::ui::portal::Interactions::LIMIT {
        eprintln!("pneud: notification submission capacity exhausted");
        return;
    }
    jobs.spawn(async move {
        for joined in crate::ui::window_setup::joined(&core).await {
            let result = actions
                .notice(joined.notice.clone(), Vec::new(), None, 0, None)
                .await;
            joined.posted(&core, result).await;
        }
    });
}
fn queue_notice(jobs: &mut tokio::task::JoinSet<()>, actions: Arc<DesktopActions>, notice: Notice) {
    while jobs.try_join_next().is_some() {}
    if jobs.len() >= crate::ui::portal::Interactions::LIMIT {
        eprintln!("pneud: notification submission capacity exhausted");
        return;
    }
    jobs.spawn(async move {
        if let Err(error) = actions.notice(notice, Vec::new(), None, 0, None).await {
            eprintln!("pneud: notification unavailable: {error:#}");
        }
    });
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
