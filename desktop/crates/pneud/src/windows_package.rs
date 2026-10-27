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
