#![cfg(target_os = "linux")]
use anyhow::Result;
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn command(dir: &Path, systemctl: &str) -> Result<tokio::process::Command> {
    std::fs::create_dir_all(dir.join("bin"))?;
    let stub = dir.join("bin/systemctl");
    std::fs::write(&stub, format!("#!/bin/sh\n{systemctl}\n"))?;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))?;
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_pneud"));
    cmd.arg("window")
        .env("HOME", dir)
        .env("XDG_RUNTIME_DIR", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        // No route to the real systemctl, even if the stub is missing.
        .env("PATH", dir.join("bin"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(cmd)
}

async fn reply(listener: tokio::net::UnixListener, ok: bool) -> Result<()> {
    let (stream, _) = listener.accept().await?;
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await?;
    let request: serde_json::Value = serde_json::from_str(&line)?;
    assert_eq!(request["command"], "show_window");
    stream
        .get_mut()
        .write_all(format!("{{\"ok\":{ok},\"value\":{{}}}}\n").as_bytes())
        .await?;
    Ok(())
}

#[tokio::test]
async fn first_window_starts_user_unit_and_waits_for_ipc() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut cmd = command(
        dir.path(),
        "printf '%s\\n' \"$*\" >> \"$HOME/commands\"; if [ \"$2\" = start ]; then : > \"$HOME/started\"; fi",
    )?;
    let child = cmd.spawn()?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !dir.path().join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("commands"))?,
            "--user daemon-reload\n--user start pneud.service\n"
        );
        let listener = tokio::net::UnixListener::bind(dir.path().join("pneud.sock"))?;
        reply(listener, true).await?;
        let output = child.wait_with_output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::Ok(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn running_daemon_and_command_errors_never_start_service() -> Result<()> {
    for ok in [true, false] {
        let dir = tempfile::tempdir()?;
        let listener = tokio::net::UnixListener::bind(dir.path().join("pneud.sock"))?;
        let child = command(dir.path(), "echo unexpected > \"$HOME/started\"; exit 99")?.spawn()?;
        reply(listener, ok).await?;
        let output = child.wait_with_output().await?;
        assert_eq!(output.status.success(), ok);
        assert!(!dir.path().join("started").exists());
    }
    Ok(())
}

#[tokio::test]
async fn failed_service_start_has_an_actionable_error() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let output = command(dir.path(), "exit 1")?.output().await?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("systemctl --user status pneud.service")
    );
    Ok(())
}
