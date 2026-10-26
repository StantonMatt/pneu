//! The daemon owns and reaps each independently hosted window process.
use anyhow::Result;
use tokio::process::Child;

pub(super) fn launch(request: u64, token: Option<&str>) -> Result<Child> {
    #[cfg(target_os = "linux")]
    let mut command = super::activation::command(std::env::current_exe()?, token);
    #[cfg(windows)]
    let mut command = {
        let _ = token;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        command
    };
    let child = command
        .arg("ui-window")
        .env("PNEU_WINDOW_REQUEST", request.to_string())
        .kill_on_drop(true)
        .spawn()?;
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
