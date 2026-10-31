//! Restart policy is independent of AppKit and deterministic in tests.
use std::time::{Duration, Instant};
pub(crate) struct Restart {
    pub next: Instant,
    failures: u32,
}
impl Restart {
    pub fn new(now: Instant) -> Self {
        Self {
            next: now,
            failures: 0,
        }
    }
    pub fn failed(&mut self, now: Instant, lived: Duration) {
        if lived >= Duration::from_secs(60) {
            self.failures = 0;
        }
        let delay = Duration::from_millis(250u64.saturating_mul(1u64 << self.failures.min(7)))
            .min(Duration::from_secs(30));
        self.failures = self.failures.saturating_add(1);
        self.next = now + delay;
    }
}
/// EOF lets the UI run its main-thread cleanup. A stuck main loop still has a
/// strict deadline; kill and reap only our child after that deadline.
#[cfg(any(target_os = "macos", test))]
pub(crate) async fn stop(child: &mut tokio::process::Child, grace: Duration) {
    if !matches!(tokio::time::timeout(grace, child.wait()).await, Ok(Ok(_))) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}
/// Only this child inherits the socket capability. The executable is supplied
/// by the caller (production always uses current_exe inside its own .app).
pub(crate) fn spawn(
    executable: &std::path::Path,
    arguments: &[&str],
    window_request: u64,
    view_cache: Option<(&std::path::Path, &std::fs::File)>,
) -> anyhow::Result<(
    tokio::process::Child,
    std::os::unix::net::UnixStream,
    String,
)> {
    use std::os::fd::AsRawFd;
    let (parent, child_socket) = std::os::unix::net::UnixStream::pair()?;
    let descriptor = child_socket.as_raw_fd();
    let token = crate::random_hex(32);
    let mut command = tokio::process::Command::new(executable);
    command
        .args(arguments)
        .env("PNEU_UI_FD", descriptor.to_string())
        .env("PNEU_UI_TOKEN", &token)
        .env("PNEU_WINDOW_REQUEST", window_request.to_string())
        .env(
            "PNEU_DAEMON_INSTANCE",
            super::window_setup::daemon_instance(),
        )
        .kill_on_drop(true);
    command
        .env_remove("PNEU_UI_VIEW_CACHE")
        .env_remove("PNEU_UI_VIEW_LEASE_FD");
    let lease_fd = view_cache.map(|(_, lock)| lock.as_raw_fd());
    if let Some((cache, _)) = view_cache {
        command.env("PNEU_UI_VIEW_CACHE", cache);
        command.env(
            "PNEU_UI_VIEW_LEASE_FD",
            lease_fd.expect("lease descriptor").to_string(),
        );
    }
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(descriptor, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(lease_fd) = lease_fd
                && libc::fcntl(lease_fd, libc::F_SETFD, 0) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop(child_socket);
    Ok((child, parent, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shutdown_allows_eof_cleanup_and_bounds_a_stuck_child() -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        let temp = tempfile::tempdir()?;
        let marker = temp.path().join("cleaned");
        let mut child = tokio::process::Command::new("sh")
            .args([
                "-c",
                "cat >/dev/null; sleep 0.1; echo cleaned > \"$1\"",
                "sh",
            ])
            .arg(&marker)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.take().expect("stdin").shutdown().await?;
        stop(&mut child, Duration::from_secs(2)).await;
        assert!(child.wait().await?.success());
        assert_eq!(std::fs::read_to_string(marker)?.trim(), "cleaned");
        let mut stuck = tokio::process::Command::new("sleep").arg("30").spawn()?;
        stop(&mut stuck, Duration::from_millis(30)).await;
        assert!(!stuck.wait().await?.success());
        Ok(())
    }
    #[test]
    fn restart_backoff_is_bounded_and_resets_after_a_stable_child() {
        let now = Instant::now();
        let mut policy = Restart::new(now);
        assert_eq!(policy.next, now);
        policy.failed(now, Duration::ZERO);
        assert_eq!(policy.next - now, Duration::from_millis(250));
        policy.failed(now, Duration::ZERO);
        assert_eq!(policy.next - now, Duration::from_millis(500));
        for _ in 0..100 {
            policy.failed(now, Duration::ZERO);
        }
        assert_eq!(policy.next - now, Duration::from_secs(30));
        policy.failed(now, Duration::from_secs(60));
        assert_eq!(policy.next - now, Duration::from_millis(250));
    }
}
