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

pub(super) fn install(window: &super::window::PneuWindow) {
    window.window().on_winit_window_event(|window, event| {
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
            SetForegroundWindow(hwnd);
        }
    });
}
