//! Package activation and Restart Manager lifetime. No installer privileges.
use anyhow::{Context, Result, ensure};
use std::{
    ptr::{null, null_mut},
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
    thread::JoinHandle,
    time::Duration,
};
use windows::{
    ApplicationModel::{Activation::ActivationKind, AppInstance},
    Win32::System::{
        Recovery::{
            RESTART_NO_CRASH, RESTART_NO_HANG, RESTART_NO_REBOOT, RegisterApplicationRestart,
        },
        WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
    },
    core::w,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

pub(crate) struct Apartment(bool);
impl Apartment {
    pub(crate) fn new() -> Result<Self> {
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self(true)),
            // The UI thread may already own an STA. Use that apartment without
            // unbalancing its initialization count.
            Err(error) if error.code().0 == 0x80010106u32 as i32 => Ok(Self(false)),
            Err(error) => Err(error.into()),
        }
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { RoUninitialize() }
        }
    }
}
pub fn packaged() -> Result<bool> {
    let mut len = 0;
    match unsafe {
        windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName(&mut len, None)
    }
    .0
    {
        122 => Ok(true),
        15700 => Ok(false),
        code => anyhow::bail!("query package identity: {code}"),
    }
}
pub fn startup_activation() -> Result<bool> {
    if !packaged()? {
        return Ok(false);
    }
    let _apartment = Apartment::new()?;
    Ok(AppInstance::GetActivatedEventArgs()?.Kind()? == ActivationKind::StartupTask)
}

/// Native share/drop inputs use the same folder and regular-file admission as CLI sends.
pub(crate) fn validate_send_paths(paths: &[std::path::PathBuf]) -> Result<()> {
    ensure!(
        !paths.is_empty() && paths.len() <= 128,
        "choose 1 to 128 regular files"
    );
    for path in paths {
        ensure!(path.is_absolute(), "file paths must be absolute");
        if crate::storage::Folder::read(path).is_ok() {
            continue;
        }
        let file = crate::storage::open_regular_file(path)?;
        ensure!(
            file.metadata()?.len() <= crate::outbox::PEER_BYTES.min(pneu_proto::MAX_FILE_SIZE),
            "file exceeds send size limit"
        );
    }
    Ok(())
}
/// Share activation is a short-lived client. The normal daemon owns the chooser
/// and all transfers, including when the share sheet was the first launch.
pub async fn share_activation(paths: &crate::Paths) -> Result<bool> {
    use std::os::windows::{ffi::OsStringExt, process::CommandExt};
    use windows::{
        ApplicationModel::{
            Activation::ShareTargetActivatedEventArgs, DataTransfer::StandardDataFormats,
        },
        Storage::StorageItemTypes,
        core::{HSTRING, Interface},
    };
    if !packaged()? {
        return Ok(false);
    }
    let _apartment = Apartment::new()?;
    let args = AppInstance::GetActivatedEventArgs()?;
    if args.Kind()? != ActivationKind::ShareTarget {
        return Ok(false);
    }
    let operation = args
        .cast::<ShareTargetActivatedEventArgs>()?
        .ShareOperation()?;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let data = operation.Data()?;
        // Image sources can advertise Bitmap for share-target filtering. Always
        // use their original files, never a re-encoded bitmap. A bitmap-only
        // source gets an actionable share-sheet error without starting a send.
        ensure!(
            data.Contains(&StandardDataFormats::StorageItems()?)?,
            "Pneu needs original files; save the image, then share its file"
        );
        let items = data.GetStorageItemsAsync()?.await?;
        ensure!(
            (1..=128).contains(&items.Size()?),
            "choose 1 to 128 regular files"
        );
        let mut files = Vec::new();
        for item in items {
            ensure!(
                item.IsOfType(StorageItemTypes::File)?
                    || item.IsOfType(StorageItemTypes::Folder)?,
                "choose files or folders"
            );
            files.push(std::path::PathBuf::from(std::ffi::OsString::from_wide(
                item.Path()?.as_ref(),
            )));
        }
        validate_send_paths(&files)?;
        if crate::ipc::request(&paths.socket, crate::ipc::Command::Status)
            .await
            .is_err()
        {
            std::process::Command::new(std::env::current_exe()?)
                .arg("run")
                .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            let mut ready = false;
            for _ in 0..80 {
                if crate::ipc::request(&paths.socket, crate::ipc::Command::Status)
                    .await
                    .is_ok()
                {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            ensure!(ready, "Pneu could not start");
        }
        crate::ipc::request(
            &paths.socket,
            crate::ipc::Command::PresentFiles {
                paths: files,
                targets: crate::outbox::Targets::All("all".into()),
                request_id: None,
            },
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Pneu did not respond to the share request")
    .and_then(|result| result);
    match result {
        Ok(()) => operation.ReportCompleted()?,
        Err(error) => {
            operation.ReportError(&HSTRING::from(format!("Couldn't send files: {error}")))?;
            return Err(error);
        }
    }
    Ok(true)
}

static CORE: OnceLock<Mutex<Weak<crate::Core>>> = OnceLock::new();
/// Also used by the tray and clipboard windows. Only the session owner waits
/// for cleanup, so the UI threads can still join during daemon shutdown.
pub(crate) fn session_message(message: u32, w: WPARAM) -> Option<LRESULT> {
    match message {
        WM_QUERYENDSESSION => Some(1),
        WM_CLOSE | WM_ENDSESSION if message == WM_CLOSE || w != 0 => {
            if let Some(core) = CORE.get().and_then(|c| c.lock().ok()?.upgrade()) {
                core.request_shutdown();
            }
            Some(0)
        }
        WM_ENDSESSION => Some(0),
        _ => None,
    }
}
struct State {
    complete: Mutex<bool>,
    finished: Condvar,
}
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(l as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if let Some(result) = session_message(message, w) {
            if message == WM_ENDSESSION && w != 0 {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
                if let Some(state) = state.as_ref() {
                    // Give the async owner time to drain IPC, stop helpers and
                    // flush stores before acknowledging the final end-session.
                    let _ = state.finished.wait_timeout_while(
                        state.complete.lock().expect("session completion"),
                        Duration::from_secs(4),
                        |complete| !*complete,
                    );
                }
            }
            return result;
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}
pub struct Session {
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}
impl Session {
    pub async fn start(core: &Arc<crate::Core>) -> Result<Self> {
        *CORE
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .expect("session core") = Arc::downgrade(core);
        // Explicit run bypasses launch UI on update restart. Never auto-restart
        // after a crash/hang or reboot; the StartupTask owns sign-in startup.
        unsafe {
            RegisterApplicationRestart(
                w!("run"),
                RESTART_NO_CRASH | RESTART_NO_HANG | RESTART_NO_REBOOT,
            )?;
        }
        let state = Arc::new(State {
            complete: Mutex::new(false),
            finished: Condvar::new(),
        });
        let owner = state.clone();
        let (ready, receive) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("pneu-session".into())
            .spawn(move || {
                let result = (|| -> Result<(HWND, u32)> {
                    unsafe {
                        let class: Vec<u16> = "PneuSession\0".encode_utf16().collect();
                        let module = GetModuleHandleW(null());
                        let wc = WNDCLASSW {
                            lpfnWndProc: Some(window_proc),
                            hInstance: module,
                            lpszClassName: class.as_ptr(),
                            ..std::mem::zeroed()
                        };
                        RegisterClassW(&wc);
                        let hwnd = CreateWindowExW(
                            0,
                            class.as_ptr(),
                            class.as_ptr(),
                            0,
                            0,
                            0,
                            0,
                            0,
                            null_mut(),
                            null_mut(),
                            module,
                            Arc::as_ptr(&owner).cast(),
                        );
                        ensure!(
                            !hwnd.is_null(),
                            "create session window: {}",
                            std::io::Error::last_os_error()
                        );
                        Ok((
                            hwnd,
                            windows_sys::Win32::System::Threading::GetCurrentThreadId(),
                        ))
                    }
                })();
                match result {
                    Ok((hwnd, id)) => {
                        let _ = ready.send(Ok(id));
                        unsafe {
                            let mut msg = std::mem::zeroed();
                            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                                TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                            DestroyWindow(hwnd);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })?;
        match receive.await.context("session window thread stopped")? {
            Ok(thread_id) => Ok(Self {
                state,
                thread: Some(thread),
                thread_id,
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        *self.state.complete.lock().expect("session completion") = true;
        self.state.finished.notify_all();
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_and_drop_paths_accept_folders_and_bounded_absolute_files() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let file = dir.path().join("file with spaces.txt");
        std::fs::write(&file, b"native shell input")?;
        validate_send_paths(std::slice::from_ref(&file))?;
        validate_send_paths(&vec![file.clone(); 128])?;
        assert!(validate_send_paths(&[]).is_err());
        assert!(validate_send_paths(&vec![file.clone(); 129]).is_err());
        for invalid in [
            std::path::PathBuf::from("relative.txt"),
            std::path::PathBuf::from(r"C:drive-relative.txt"),
            std::path::PathBuf::from(r"\root-relative.txt"),
            dir.path().join("missing.txt"),
        ] {
            assert!(validate_send_paths(&[file.clone(), invalid]).is_err());
        }
        // All native entry points dispatch directories to the common capture path.
        std::fs::remove_file(&file)?;
        std::fs::create_dir(&file)?;
        validate_send_paths(&[file, dir.path().to_path_buf()])?;
        Ok(())
    }

    #[tokio::test]
    async fn native_session_cancel_then_close_or_end_drains_owner() -> Result<()> {
        unsafe extern "system" fn find(hwnd: HWND, state: LPARAM) -> i32 {
            unsafe {
                *(state as *mut usize) = hwnd as usize;
            }
            0
        }
        for end in [false, true] {
            let dir = crate::test_support::scratch()?;
            let paths = crate::Paths {
                data: dir.path().join("data"),
                cache: dir.path().join("cache"),
                config: dir.path().join("config/config.toml"),
                socket: dir.path().join("unused"),
            };
            paths.prepare()?;
            let core = crate::Core::new(
                paths,
                crate::Config {
                    save_path: dir.path().join("saved"),
                    ..crate::Config::default()
                },
                Arc::new(crate::actions::DesktopActions::new(
                    dir.path().join("cache"),
                )),
            )
            .await?;
            let session = Session::start(&core).await?;
            let mut hwnd = 0usize;
            unsafe {
                EnumThreadWindows(
                    session.thread_id,
                    Some(find),
                    (&mut hwnd as *mut usize) as isize,
                );
            }
            ensure!(hwnd != 0, "session window exists on its owning thread");
            let hwnd = hwnd as HWND;
            unsafe {
                let mut reply = 0;
                ensure!(
                    SendMessageTimeoutW(
                        hwnd,
                        WM_QUERYENDSESSION,
                        0,
                        ENDSESSION_CLOSEAPP as isize,
                        SMTO_ABORTIFHUNG,
                        1000,
                        &mut reply
                    ) != 0
                );
                assert_eq!(reply, 1);
                ensure!(
                    SendMessageTimeoutW(
                        hwnd,
                        WM_ENDSESSION,
                        0,
                        0,
                        SMTO_ABORTIFHUNG,
                        1000,
                        &mut reply
                    ) != 0
                );
            }
            // A cancelled update/logoff must leave the daemon serving.
            assert!(
                tokio::time::timeout(Duration::from_millis(30), core.wait_shutdown())
                    .await
                    .is_err()
            );
            unsafe {
                PostMessageW(
                    hwnd,
                    if end { WM_ENDSESSION } else { WM_CLOSE },
                    usize::from(end),
                    0,
                );
            }
            tokio::time::timeout(Duration::from_secs(2), core.wait_shutdown()).await?;
            core.shutdown_actions().await?;
            // Completes the pending WM_ENDSESSION and joins the window thread.
            tokio::task::spawn_blocking(move || drop(session)).await?;
            drop(core);
            // Store ownership has been released for the next process/version.
            let _store = crate::store::Store::open(&dir.path().join("data/state.sqlite3"))?;
        }
        Ok(())
    }
}
