//! Keep the fixed-width Slint window inside the current monitor's work area.
use slint::{
    ComponentHandle,
    winit_030::{
        EventResult, WinitWindowAccessor,
        winit::{
            event::WindowEvent,
            raw_window_handle::{HasWindowHandle, RawWindowHandle},
        },
    },
};
use windows_sys::Win32::{
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    UI::WindowsAndMessaging::{GetWindowRect, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos},
};

pub(super) fn install(
    window: &super::window::PneuWindow,
    mut ready: impl FnMut(&super::window::PneuWindow) + 'static,
) {
    let weak = window.as_weak();
    window.window().on_winit_window_event(move |window, event| {
        if let Some(window) = weak.upgrade() {
            ready(&window);
        }
        if matches!(event, WindowEvent::Resized(size) if size.width > 0 && size.height > 0) {
            window.with_winit_window(|native| {
                let Ok(handle) = native.window_handle() else {
                    return;
                };
                let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                    return;
                };
                let hwnd = handle.hwnd.get() as _;
                unsafe {
                    let mut monitor: MONITORINFO = std::mem::zeroed();
                    monitor.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                    let mut rect = std::mem::zeroed();
                    if GetMonitorInfoW(
                        MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                        &mut monitor,
                    ) == 0
                        || GetWindowRect(hwnd, &mut rect) == 0
                    {
                        return;
                    }
                    let work = monitor.rcWork;
                    let width = (rect.right - rect.left).min(work.right - work.left);
                    let height = (rect.bottom - rect.top).min(work.bottom - work.top);
                    let x = rect.left.clamp(work.left, work.right - width);
                    let y = rect.top.clamp(work.top, work.bottom - height);
                    if (x, y, width, height)
                        != (
                            rect.left,
                            rect.top,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                        )
                    {
                        // Programmatic placement can fit below the layout's
                        // preferred minimum. Its existing Flickable then scrolls.
                        SetWindowPos(
                            hwnd,
                            std::ptr::null_mut(),
                            x,
                            y,
                            width,
                            height,
                            SWP_NOACTIVATE | SWP_NOZORDER,
                        );
                    }
                }
            });
        }
        EventResult::Propagate
    });
}

/// Present the retained child without recreating its window or navigating away.
pub(super) fn activate(window: &super::window::PneuWindow) {
    let _ = window.show();
    window.window().with_winit_window(|native| {
        let Ok(handle) = native.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let hwnd = handle.hwnd.get() as _;
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
            };
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            if SetForegroundWindow(hwnd) == 0 {
                // The share host can restore Explorer after forwarding activation,
                // revoking its temporary foreground grant. This UI-thread call
                // follows an explicit presentation request; briefly join that
                // foreground input queue to complete the requested activation.
                use windows_sys::Win32::{
                    System::Threading::{AttachThreadInput, GetCurrentThreadId},
                    UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
                };
                let current = GetCurrentThreadId();
                let foreground =
                    GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
                if foreground != 0
                    && foreground != current
                    && AttachThreadInput(current, foreground, 1) != 0
                {
                    SetForegroundWindow(hwnd);
                    AttachThreadInput(current, foreground, 0);
                }
            }
            // Foreground activation and shell share dismissal can leave the
            // HWND below Explorer in the non-topmost stack. Raise it explicitly.
            use windows_sys::Win32::UI::WindowsAndMessaging::{SWP_NOMOVE, SWP_NOSIZE};
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    });
}
