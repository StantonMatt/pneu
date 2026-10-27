//! StartupTask is package-owned; unpackaged development builds have no toggle.
use super::window::{Page, PneuWindow, Ui};
use anyhow::Result;
use slint::ComponentHandle;
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    ApplicationModel::{StartupTask, StartupTaskState},
    core::w,
};

fn state(enabled: Option<bool>) -> Result<StartupTaskState> {
    let _apartment = crate::windows_package::Apartment::new()?;
    let task = StartupTask::GetAsync(&"PneuStartup".into())?.join()?;
    match enabled {
        Some(true) => Ok(task.RequestEnableAsync()?.join()?),
        Some(false) => {
            task.Disable()?;
            Ok(task.State()?)
        }
        None => Ok(task.State()?),
    }
}
fn refresh(window: slint::Weak<PneuWindow>, busy: Arc<AtomicBool>, enabled: Option<bool>) {
    if busy.swap(true, Ordering::AcqRel) {
        return;
    }
    if enabled.is_some()
        && let Some(window) = window.upgrade()
    {
        window.global::<Ui>().set_login_busy(true);
    }
    let completed = busy.clone();
    let weak = window.clone();
    let result = std::thread::Builder::new()
        .name("pneu-login-state".into())
        .spawn(move || {
            let result = state(enabled);
            let _ = slint::invoke_from_event_loop(move || {
                completed.store(false, Ordering::Release);
                if let Some(window) = weak.upgrade() {
                    let ui = window.global::<Ui>();
                    ui.set_login_busy(false);
                    match result {
                        Ok(state) => {
                            ui.set_login_enabled(matches!(
                                state,
                                StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy
                            ));
                            ui.set_login_pending(state == StartupTaskState::DisabledByUser);
                            ui.set_login_managed(matches!(
                                state,
                                StartupTaskState::DisabledByPolicy
                                    | StartupTaskState::EnabledByPolicy
                            ));
                            ui.set_login_detail(
                                match state {
                                    StartupTaskState::DisabledByUser => {
                                        "Turned off in Windows Settings"
                                    }
                                    StartupTaskState::DisabledByPolicy => {
                                        "Turned off by your organization"
                                    }
                                    StartupTaskState::EnabledByPolicy => {
                                        "Managed by your organization"
                                    }
                                    _ => "",
                                }
                                .into(),
                            );
                            ui.set_login_error("".into());
                        }
                        Err(_) => ui.set_login_error(
                            "Couldn't read the startup setting. Try Windows Settings.".into(),
                        ),
                    }
                }
            });
        });
    if result.is_err() {
        busy.store(false, Ordering::Release);
        if let Some(window) = window.upgrade() {
            window.global::<Ui>().set_login_busy(false);
            window
                .global::<Ui>()
                .set_login_error("Couldn't read the startup setting. Try again.".into());
        }
    }
}
pub(super) fn install(window: &PneuWindow) -> Option<slint::Timer> {
    if !crate::windows_package::packaged().unwrap_or(false) {
        return None;
    }
    window.global::<Ui>().set_login_available(true);
    let busy = Arc::new(AtomicBool::new(false));
    let weak = window.as_weak();
    let next = Rc::new(Cell::new(Instant::now()));
    let was_settings = Rc::new(Cell::new(false));
    let timer = slint::Timer::default();
    let polling = busy.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            if let Some(window) = weak.upgrade() {
                let settings = window.global::<Ui>().get_page() == Page::Settings;
                if settings
                    && (!was_settings.get() || Instant::now() >= next.get())
                    && !polling.load(Ordering::Acquire)
                {
                    next.set(Instant::now() + Duration::from_secs(5));
                    refresh(weak.clone(), polling.clone(), None);
                }
                was_settings.set(settings);
            }
        },
    );
    let weak = window.as_weak();
    window
        .global::<Ui>()
        .on_login(move |enabled| refresh(weak.clone(), busy.clone(), Some(enabled)));
    window.global::<Ui>().on_system_settings(|pane| {
        if pane == "login" {
            unsafe {
                windows_sys::Win32::UI::Shell::ShellExecuteW(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    w!("ms-settings:startupapps").as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                );
            }
        }
    });
    Some(timer)
}
