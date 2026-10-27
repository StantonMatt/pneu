//! A user close is explicit IPC: the hidden window host remains reusable.
use super::notifications::Notice;
use crate::{Core, actions::DesktopActions};
use anyhow::Result;
use std::path::Path;
use windows::ApplicationModel::StartupTaskState;

const SEEN: &str = "window-close-notice-seen";

fn notice(state: Option<StartupTaskState>) -> Notice {
    let mut notice = Notice::new("Pneu is still running");
    notice.body = if matches!(
        state,
        Some(StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy)
    ) {
        "It opens when you sign in. Find it under ^ on the taskbar."
    } else {
        "Find it under ^ on the taskbar."
    }
    .into();
    notice.urgency = 0;
    notice.transient = true;
    notice.expires = 4000;
    notice
}
fn eligible(current: bool, notifications: bool, data: &Path) -> Result<bool> {
    Ok(current && notifications && !data.join(SEEN).try_exists()?)
}
pub(super) async fn show(core: &Core, actions: &DesktopActions, request: u64) -> Result<()> {
    if !eligible(
        core.window_close_current(request),
        core.config().notification_preview,
        &core.paths.data,
    )? {
        return Ok(());
    }
    let state = tokio::task::spawn_blocking(|| {
        if crate::windows_package::packaged().unwrap_or(false) {
            super::windows_login::state(None).ok()
        } else {
            None
        }
    })
    .await?;
    // A request, setting change or shutdown can supersede the WinRT query.
    if !eligible(
        core.window_close_current(request),
        core.config().notification_preview,
        &core.paths.data,
    )? {
        return Ok(());
    }
    actions
        .notice(notice(state), Vec::new(), None, 0, None)
        .await?;
    let marker = core.paths.data.join(SEEN);
    tokio::task::spawn_blocking(move || crate::config::atomic_private(&marker, b"1\n")).await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_states_select_exact_copy_and_silent_window_action() -> Result<()> {
        for state in [
            None,
            Some(StartupTaskState::Disabled),
            Some(StartupTaskState::DisabledByUser),
            Some(StartupTaskState::DisabledByPolicy),
            Some(StartupTaskState::Enabled),
            Some(StartupTaskState::EnabledByPolicy),
        ] {
            let n = notice(state);
            assert_eq!(n.summary, "Pneu is still running");
            let enabled = matches!(
                state,
                Some(StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy)
            );
            assert_eq!(
                n.body,
                if enabled {
                    "It opens when you sign in. Find it under ^ on the taskbar."
                } else {
                    "Find it under ^ on the taskbar."
                }
            );
            assert_eq!((n.urgency, n.transient, n.expires), (0, true, 4000));
            assert!(n.actions.is_empty());
            let mut targets = super::super::windows_toast_model::Targets::default();
            let (record, _) = targets.issue(n, (), 0)?;
            let xml = record.xml(None);
            assert!(xml.contains(r#"<audio silent="true"/>"#));
            assert!(!xml.contains("<actions>"));
            assert!(xml.contains(&format!("launch=\"{}\"", record.argument("window"))));
            assert_eq!(targets.take(&record.argument("window"))?.1, "window");
        }
        Ok(())
    }
    #[tokio::test]
    async fn failed_submission_off_stale_and_shutdown_leave_marker_absent() -> Result<()> {
        let scratch = crate::test_support::scratch()?;
        let paths = crate::Paths {
            data: scratch.path().join("data"),
            cache: scratch.path().join("cache"),
            config: scratch.path().join("config/config.toml"),
            socket: scratch.path().join("unused"),
        };
        paths.prepare()?;
        // Existing config files need no new field or migration.
        crate::config::atomic_private(&paths.config, b"notification_preview = false\n")?;
        let mut config = crate::Config::load(&paths.config)?;
        config.save_path = scratch.path().join("saved");
        let actions = std::sync::Arc::new(DesktopActions::new(paths.cache.clone()));
        let core = Core::new(paths, config, actions.clone()).await?;
        core.request_window(false);
        let request = core.window_request();
        assert!(core.window_close_current(request));
        // No toast host: off/stale/shutdown must return without attempting it.
        show(&core, &actions, request).await?;
        core.set_setting("notification_preview".into(), serde_json::json!(true))
            .await?;
        let failure = show(&core, &actions, request).await.unwrap_err();
        assert!(failure.to_string().contains("interactive daemon"));
        assert!(!core.paths.data.join(SEEN).exists());
        let mut events = core.subscribe();
        core.request_window(false);
        assert!(events.try_recv().is_err(), "requests are not user closes");
        assert!(!core.window_close_current(request));
        core.window_closed(request);
        assert!(
            matches!(events.try_recv()?, crate::Event::WindowClosed { request: r } if r == request)
        );
        show(&core, &actions, request).await?;
        let current = core.window_request();
        core.request_shutdown();
        assert!(!core.window_close_current(current));
        show(&core, &actions, current).await?;
        assert!(!core.paths.data.join(SEEN).exists());
        core.finish_shutdown().await?;
        Ok(())
    }
    #[test]
    fn only_current_user_close_with_notifications_can_mark_seen() -> Result<()> {
        let scratch = crate::test_support::scratch()?;
        let data = scratch.path();
        assert!(eligible(true, true, data)?);
        // Crashes/shutdown/replacement never send WindowClosed; superseded
        // close messages and notifications-off must not consume the notice.
        assert!(!eligible(false, true, data)?);
        assert!(!eligible(true, false, data)?);
        assert!(!data.join(SEEN).exists());
        crate::config::atomic_private(&data.join(SEEN), b"1\n")?;
        assert!(!eligible(true, true, data)?);
        assert_eq!(std::fs::read(data.join(SEEN))?, b"1\n");
        let reopened = data.to_path_buf();
        assert!(!eligible(true, true, &reopened)?);
        Ok(())
    }
}
