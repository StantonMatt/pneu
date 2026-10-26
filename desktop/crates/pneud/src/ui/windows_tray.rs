//! Explorer tray objects stay on their creating thread and its Win32 loop.
use super::{
    Command, Snapshot,
    tray_model::{self, MenuEntry},
};
use crate::Config;
use anyhow::{Context, Result, ensure};
use muda::{
    CheckMenuItem, ContextMenu, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use std::{
    ptr::{null, null_mut},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};
use tokio::sync::mpsc;
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW},
        Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        Threading::GetCurrentThreadId,
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, GetDpiForWindow,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::*,
    },
};

pub struct Tray {
    latest: Arc<Mutex<Option<(Snapshot, Config)>>>,
    hwnd: usize,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}
impl Tray {
    pub fn update(&self, snapshot: Snapshot, config: Config) {
        *self.latest.lock().expect("tray state") = Some((snapshot, config));
    }
    fn stop(&self) {
        unsafe {
            // A window message is dispatched even inside TrackPopupMenu's
            // nested loop. Its handler ends that loop before WM_QUIT is read.
            PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
    }
    pub async fn shutdown(mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
            // Covers early runtime errors as well as normal shutdown.
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}
pub async fn start(
    snapshot: Snapshot,
    config: Config,
    commands: mpsc::Sender<Command>,
) -> Result<Tray> {
    let latest = Arc::new(Mutex::new(Some((snapshot, config))));
    let state = latest.clone();
    let (ready, receive) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("pneu-tray".into())
        .spawn(move || {
            let mut ready = Some(ready);
            if let Err(error) = run(state, commands, &mut ready) {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(error));
                } else {
                    eprintln!("pneud: tray stopped: {error:#}");
                }
            }
        })?;
    match receive.await {
        Ok(Ok((hwnd, thread_id))) => Ok(Tray {
            latest,
            hwnd,
            thread_id,
            thread: Some(thread),
        }),
        result => {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            result.context("tray startup thread stopped")??;
            unreachable!()
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_CLOSE {
        unsafe {
            EndMenu();
            PostQuitMessage(0);
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}
struct LoopWindow(HWND);
impl Drop for LoopWindow {
    fn drop(&mut self) {
        unsafe {
            KillTimer(self.0, 1);
            DestroyWindow(self.0);
        }
    }
}
type Ready = tokio::sync::oneshot::Sender<Result<(usize, u32)>>;
fn run(
    latest: Arc<Mutex<Option<(Snapshot, Config)>>>,
    commands: mpsc::Sender<Command>,
    ready: &mut Option<Ready>,
) -> Result<()> {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let class: Vec<u16> = "PneuTrayLoop\0".encode_utf16().collect();
        let module = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        // One daemon per user/data directory; registration may already exist
        // when the runtime is restarted in a test in this process.
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
            null(),
        );
        ensure!(
            !hwnd.is_null(),
            "create tray loop window: {}",
            std::io::Error::last_os_error()
        );
        let window = LoopWindow(hwnd);
        ensure!(
            SetTimer(hwnd, 1, 250, None) != 0,
            "create tray refresh timer"
        );
        let mut menu_theme = dark_theme();
        apply_menu_theme(menu_theme);
        let tray = TrayIconBuilder::new()
            .with_tooltip("Pneu")
            .with_menu_on_left_click(false)
            .with_icon(icon(
                "pneu-tray-disconnected",
                dark_theme(),
                icon_size(tray_dpi()),
            )?)
            .build()?;
        // Keep the popup here, rather than attaching it to tray-icon: muda's
        // TPM_RETURNCMD dispatch preserves IDs above WM_COMMAND's 16-bit limit.
        let mut menu = Menu::new();
        let mut checks: Vec<(CheckMenuItem, bool)> = Vec::new();
        let mut bindings: Vec<(MenuId, Command)> = Vec::new();
        let mut menu_key = String::new();
        let mut icon_key = ("", false, 0);
        let mut tooltip = String::new();
        let mut snapshot = Snapshot::default();
        let mut config = Config::default();
        let mut message: MSG = std::mem::zeroed();
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok((hwnd as usize, GetCurrentThreadId())));
        }
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result == 0 {
                break;
            }
            ensure!(
                result != -1,
                "tray GetMessage: {}",
                std::io::Error::last_os_error()
            );
            TranslateMessage(&message);
            DispatchMessageW(&message);
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(
                    &event,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    }
                ) {
                    SetForegroundWindow(hwnd);
                    let _ = commands.try_send(Command::Window {
                        activation_token: None,
                    });
                } else if matches!(
                    event,
                    TrayIconEvent::Click {
                        button: MouseButton::Right,
                        button_state: MouseButtonState::Up,
                        ..
                    }
                ) {
                    menu.show_context_menu_for_hwnd(hwnd as isize, None);
                    // Required by the notification-icon context-menu contract.
                    PostMessageW(hwnd, WM_NULL, 0, 0);
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some((_, command)) = bindings.iter().find(|(id, _)| *id == event.id) {
                    let _ = commands.try_send(command.clone());
                    // muda auto-toggles check items before publishing the event.
                    // Restore the model even when reselecting the current mode
                    // produces no ConfigChanged event or different menu key.
                    for (item, checked) in &checks {
                        item.set_checked(*checked);
                    }
                }
            }
            if let Some((next, settings)) = latest.lock().expect("tray state").take() {
                snapshot = next;
                config = settings;
            }
            let entries = tray_model::menu(&snapshot, &config);
            let key = format!("{entries:?}");
            if key != menu_key {
                let next_menu = Menu::new();
                let mut next_bindings = Vec::new();
                let mut next_checks = Vec::new();
                for entry in &entries {
                    next_menu.append(
                        build_item(entry, &mut next_bindings, &mut next_checks)?.as_ref(),
                    )?;
                }
                // Open Pneu is the first action, after status and separator.
                SetMenuDefaultItem(next_menu.hpopupmenu() as _, 2, 1);
                menu = next_menu;
                checks = next_checks;
                bindings = next_bindings;
                menu_key = key;
            }
            let transfers = snapshot
                .transfers
                .iter()
                .map(super::window::contract::Transfer::from)
                .collect::<Vec<_>>();
            let peers = super::window::peers(&snapshot, &transfers);
            let dark = dark_theme();
            if dark != menu_theme {
                apply_menu_theme(dark);
                menu_theme = dark;
            }
            let next = (
                tray_model::icon(&snapshot, &peers),
                dark,
                icon_size(tray_dpi()),
            );
            if next != icon_key && tray.set_icon(Some(icon(next.0, next.1, next.2)?)).is_ok() {
                icon_key = next;
            }
            let mut lines: Vec<_> = peers.iter().map(super::window::Peer::short).collect();
            if snapshot.error.is_some()
                || snapshot.pairing.is_some()
                || lines.is_empty()
                || peers.iter().any(|p| p.sending() || p.receiving())
            {
                lines.insert(0, tray_model::status(&snapshot, &peers, &transfers));
            }
            let next_tooltip = format!("Pneu\n{}", lines.join("\n"));
            if next_tooltip != tooltip && tray.set_tooltip(Some(&next_tooltip)).is_ok() {
                tooltip = next_tooltip;
            }
        }
        drop(tray);
        drop(window);
    }
    Ok(())
}
fn build_item(
    entry: &MenuEntry,
    bindings: &mut Vec<(MenuId, Command)>,
    checks: &mut Vec<(CheckMenuItem, bool)>,
) -> Result<Box<dyn muda::IsMenuItem>> {
    Ok(match entry {
        MenuEntry::Separator => Box::new(PredefinedMenuItem::separator()),
        MenuEntry::Submenu { label, items } => {
            let submenu = Submenu::new(label.replace('&', "&&"), true);
            for item in items {
                submenu.append(build_item(item, bindings, checks)?.as_ref())?;
            }
            Box::new(submenu)
        }
        MenuEntry::Item {
            label,
            enabled,
            checked,
            command,
        } => {
            let label = label.replace('&', "&&");
            let item: Box<dyn muda::IsMenuItem> = match checked {
                Some(checked) => {
                    let item = CheckMenuItem::new(label, *enabled, *checked, None);
                    checks.push((item.clone(), *checked));
                    Box::new(item)
                }
                None => Box::new(MenuItem::new(label, *enabled, None)),
            };
            if let Some(command) = command {
                bindings.push((item.id().clone(), command.clone()));
            }
            item
        }
    })
}
// These optional uxtheme exports are absent on older Windows versions. Retain
// the module so its process-wide preferred mode survives between popups.
fn apply_menu_theme(dark: bool) {
    unsafe {
        static MODULE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let module = *MODULE.get_or_init(|| {
            let name: Vec<u16> = "uxtheme.dll\0".encode_utf16().collect();
            LoadLibraryW(name.as_ptr()) as usize
        }) as windows_sys::Win32::Foundation::HMODULE;
        if module.is_null() {
            return;
        }
        if let Some(address) = GetProcAddress(module, 135usize as *const u8) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(address);
            set_mode(if dark { 2 } else { 3 }); // ForceDark / ForceLight
        }
        if let Some(address) = GetProcAddress(module, 136usize as *const u8) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(address);
            flush();
        }
    }
}
fn dark_theme() -> bool {
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
        .encode_utf16()
        .collect();
    let name: Vec<u16> = "SystemUsesLightTheme\0".encode_utf16().collect();
    let mut value = 0u32;
    let mut size = 4;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        );
    }
    value == 0
}
fn tray_dpi() -> u32 {
    let class: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
    unsafe {
        let dpi = GetDpiForWindow(FindWindowW(class.as_ptr(), null()));
        if dpi == 0 { GetDpiForSystem() } else { dpi }
    }
}
fn icon_size(dpi: u32) -> u32 {
    match dpi {
        0..=96 => 16,
        97..=120 => 20,
        121..=144 => 24,
        _ => 32,
    }
}
fn icon(name: &str, dark: bool, size: u32) -> Result<Icon> {
    let svg = tray_model::ICONS
        .iter()
        .find(|(id, _)| *id == name)
        .context("tray icon")?
        .1;
    let svg = tray_model::recolor_svg(
        svg,
        if dark { "#eff0f1" } else { "#232629" },
        if dark { "#ff6b7a" } else { "#da4453" },
        if dark { "#ffad66" } else { "#f67400" },
    );
    Ok(Icon::from_rgba(
        tray_model::render_svg(svg.as_bytes(), size)?,
        size,
        size,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires disposable interactive Windows console and PNEU_TRAY_TEST_CONTROL"]
    async fn native_popup_dispatches_quit_above_u16_ids() -> Result<()> {
        super::super::clipboard::interactive()?;
        let control = std::path::PathBuf::from(
            std::env::var_os("PNEU_TRAY_TEST_CONTROL").context("tray test control directory")?,
        );
        // muda's global native ID allocator starts at 1000. Exhaust the entire
        // WM_COMMAND range before building the production popup and bindings.
        for _ in 0..66_000 {
            let _ = MenuItem::new("discard", true, None);
        }
        let (commands, mut receive) = mpsc::channel(32);
        let tray = start(Snapshot::default(), Config::default(), commands).await?;
        std::fs::write(control.join("high-id-ready"), b"ready")?;
        let command =
            tokio::time::timeout(std::time::Duration::from_secs(60), receive.recv()).await?;
        assert!(matches!(command, Some(Command::Quit)));
        tray.shutdown().await;
        std::fs::write(control.join("high-id-passed"), b"Quit resolved above 65535")?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires disposable interactive Windows console and PNEU_TRAY_TEST_CONTROL"]
    async fn native_tray_shutdown_dismisses_open_menu() -> Result<()> {
        super::super::clipboard::interactive()?;
        let control = std::path::PathBuf::from(
            std::env::var_os("PNEU_TRAY_TEST_CONTROL").context("tray test control directory")?,
        );
        let (commands, _receive) = mpsc::channel(32);
        let tray = start(Snapshot::default(), Config::default(), commands).await?;
        std::fs::write(control.join("tray-ready"), b"ready")?;
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !control.join("tray-stop").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await?;
        // The driver opens the real popup before writing tray-stop. Returning
        // proves EndMenu released its nested loop and the owner joined it.
        tokio::time::timeout(std::time::Duration::from_secs(5), tray.shutdown()).await?;
        std::fs::write(control.join("tray-stopped"), b"joined")?;
        Ok(())
    }
}
