//! App-menu activation also works before the globally enabled unit's next login.
use crate::{Paths, ipc};
use anyhow::{Context, Result, bail};
use std::{io::ErrorKind, time::Duration};

pub async fn show_window(paths: &Paths) -> Result<()> {
    match ipc::request(
        &paths.socket,
        ipc::Command::ShowWindow {
            choose_folder: false,
        },
    )
    .await
    {
        Ok(_) => return Ok(()),
        Err(error) => {
            // Never start a service after an authorization/protocol failure.
            if !error.chain().any(|cause| {
                cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        ErrorKind::NotFound | ErrorKind::ConnectionRefused
                    )
                })
            }) {
                return Err(error);
            }
        }
    }
    // A running user manager may have cached unit lookup paths before the .deb
    // was installed. Reload its unit files before this session's first start.
    for arguments in [
        &["--user", "daemon-reload"][..],
        &["--user", "start", "pneud.service"][..],
    ] {
        let status = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new("systemctl")
                .args(arguments)
                .kill_on_drop(true)
                .status(),
        )
        .await
        .context("Starting Pneu timed out; try `systemctl --user start pneud.service`")?
        .context("Couldn't start Pneu; run `systemctl --user start pneud.service` in your graphical session")?;
        if !status.success() {
            bail!(
                "Couldn't start Pneu; check `systemctl --user status pneud.service` in your graphical session"
            );
        }
    }
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if ipc::request(
                &paths.socket,
                ipc::Command::ShowWindow {
                    choose_folder: false,
                },
            )
            .await
            .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if ready.is_ok() {
        return Ok(());
    }
    bail!("Pneu didn't become ready; check `journalctl --user -u pneud.service`")
}
