//! The daemon owns and reaps each independently hosted window process.
use anyhow::Result;
#[cfg(target_os = "linux")]
pub(super) type Child = tokio::process::Child;

#[cfg(windows)]
pub(super) struct Child {
    process: tokio::process::Child,
    _job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl Child {
    fn own(process: tokio::process::Child) -> Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::*;
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        anyhow::ensure!(
            !handle.is_null(),
            "create window job: {}",
            std::io::Error::last_os_error()
        );
        let job = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // Only the window host belongs to this lifecycle. Applications opened
        // from it must survive closing Pneu, so let its children break away.
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        anyhow::ensure!(
            unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            } != 0,
            "configure window job: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            unsafe {
                AssignProcessToJobObject(
                    job.as_raw_handle(),
                    process
                        .raw_handle()
                        .ok_or_else(|| anyhow::anyhow!("window process exited"))?,
                )
            } != 0,
            "assign window job: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self { process, _job: job })
    }
}

#[cfg(windows)]
impl std::ops::Deref for Child {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.process
    }
}
#[cfg(windows)]
impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

pub(super) fn launch(request: u64, token: Option<&str>) -> Result<Child> {
    #[cfg(target_os = "linux")]
    let mut command = super::activation::command(std::env::current_exe()?, token);
    #[cfg(windows)]
    let mut command = {
        let _ = token;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        // COM launches detach from their console; those standard handles cannot
        // be inherited. The window communicates with the daemon through IPC.
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    };
    let child = command
        .arg("ui-window")
        .env("PNEU_WINDOW_REQUEST", request.to_string())
        .env(
            "PNEU_DAEMON_INSTANCE",
            super::window_setup::daemon_instance(),
        )
        .kill_on_drop(true)
        .spawn()?;
    #[cfg(windows)]
    let child = Child::own(child)?;
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        // Pass the tray's foreground permission before the child's event loop
        // creates and focuses the HWND.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
        }
    }
    Ok(child)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    };
    use windows_sys::Win32::{
        Foundation::*,
        System::{Console::FreeConsole, Threading::*},
    };

    #[tokio::test]
    async fn closing_job_terminates_the_window_host() -> Result<()> {
        const MARKER: &str = "PNEU_TEST_WINDOW_JOB_CHILD";
        if let Some(ready) = std::env::var_os(MARKER) {
            std::fs::write(ready, b"ready")?;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            return Ok(());
        }
        let scratch = crate::test_support::scratch()?;
        let ready = scratch.path().join("job-ready");
        let process = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "ui::window_process::tests::closing_job_terminates_the_window_host",
            ])
            .env(MARKER, &ready)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true)
            .spawn()?;
        let Child {
            mut process,
            _job: job,
        } = Child::own(process)?;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        drop(job);
        // Job closure may report exit code zero. The assertion is that a
        // running 30-second helper terminates promptly when its job closes.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), process.wait()).await??;
        Ok(())
    }

    #[tokio::test]
    async fn launch_after_console_detach() -> Result<()> {
        const MARKER: &str = "PNEU_TEST_WINDOW_CONSOLE_DETACH";
        if let Some(report) = std::env::var_os(MARKER) {
            unsafe { FreeConsole() };
            let result = async {
                let mut child = launch(2, None)?;
                // The test executable treats ui-window as a filter, running no tests.
                anyhow::ensure!(child.wait().await?.success());
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let _ = std::fs::write(report, format!("{result:?}"));
            std::process::exit(i32::from(result.is_err()));
        }
        let scratch = crate::test_support::scratch()?;
        let report = scratch.path().join("detached-launch.txt");
        let exe = std::env::current_exe()?;
        let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut command: Vec<u16> = format!(
            "\"{}\" --exact ui::window_process::tests::launch_after_console_detach --nocapture",
            exe.display()
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let mut environment = Vec::new();
        let mut variables: Vec<_> = std::env::vars_os()
            .chain(Some((MARKER.into(), report.into_os_string())))
            .collect();
        variables.sort_by_key(|(name, _)| name.to_string_lossy().to_uppercase());
        for (name, value) in variables {
            environment.extend(name.encode_wide());
            environment.push(b'=' as u16);
            environment.extend(value.encode_wide());
            environment.push(0);
        }
        environment.push(0);
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // Rust's Command sets STARTF_USESTDHANDLES. COM does not; reproduce its
        // console-created handles rather than the test runner's redirected pipes.
        anyhow::ensure!(
            unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
                    environment.as_ptr().cast(),
                    std::ptr::null(),
                    &startup,
                    &mut info,
                )
            } != 0,
            "{}",
            std::io::Error::last_os_error()
        );
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
        let _thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
        let wait = unsafe { WaitForSingleObject(process.as_raw_handle(), 30_000) };
        if wait != WAIT_OBJECT_0 {
            unsafe {
                TerminateProcess(process.as_raw_handle(), 1);
                WaitForSingleObject(process.as_raw_handle(), 5_000);
            }
            anyhow::bail!("detached launch test timed out");
        }
        let mut exit = 1;
        anyhow::ensure!(unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut exit) } != 0);
        anyhow::ensure!(
            exit == 0,
            "{}",
            std::fs::read_to_string(scratch.path().join("detached-launch.txt")).unwrap_or_default()
        );
        Ok(())
    }
}
