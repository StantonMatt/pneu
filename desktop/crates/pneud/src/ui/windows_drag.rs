//! OLE file drops run on the Slint UI apartment. winit's own target is disabled.
use super::window::PneuWindow;
use slint::{
    ComponentHandle,
    winit_030::{
        WinitWindowAccessor,
        winit::raw_window_handle::{HasWindowHandle, RawWindowHandle},
    },
};
use std::{cell::RefCell, os::windows::ffi::OsStringExt, path::PathBuf, rc::Rc};
use windows::{
    Win32::{
        Foundation::{HWND, POINT, POINTL},
        System::{
            Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL},
            Ole::{
                DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
                OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
            },
            SystemServices::MODIFIERKEYS_FLAGS,
        },
        UI::Shell::{DragQueryFileW, HDROP},
    },
    core::{Ref, implement},
};

type Point = (f64, f64);
#[derive(Clone)]
pub(crate) struct Callbacks {
    pub begin: Rc<dyn Fn() -> bool>,
    pub hover: Rc<dyn Fn(Point) -> bool>,
    pub drop: Rc<dyn Fn(Point, Vec<PathBuf>) -> bool>,
    pub cancel: Rc<dyn Fn()>,
}
#[implement(IDropTarget)]
struct Target {
    hwnd: HWND,
    callbacks: Callbacks,
    paths: RefCell<Option<Vec<PathBuf>>>,
}
impl Target {
    fn point(&self, point: &POINTL) -> Option<Point> {
        let mut client = POINT {
            x: point.x,
            y: point.y,
        };
        if unsafe {
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(
                self.hwnd.0,
                (&mut client as *mut POINT).cast(),
            )
        } == 0
        {
            return None;
        }
        let scale =
            unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(self.hwnd.0) } as f64 / 96.0;
        (scale > 0.0).then_some((client.x as f64 / scale, client.y as f64 / scale))
    }
    fn cancel(&self) {
        self.paths.borrow_mut().take();
        (self.callbacks.cancel)();
    }
    fn effect(&self, point: &POINTL, effect: *mut DROPEFFECT) {
        if effect.is_null() {
            return;
        }
        unsafe {
            let copy = *effect & DROPEFFECT_COPY == DROPEFFECT_COPY;
            *effect = if copy
                && self.paths.borrow().is_some()
                && self.point(point).is_some_and(|p| (self.callbacks.hover)(p))
            {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            };
        }
    }
}
impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.cancel();
        if let Some(data) = data.as_ref()
            && let Ok(paths) = selected_paths(data)
            && (self.callbacks.begin)()
        {
            *self.paths.borrow_mut() = Some(paths);
        }
        self.effect(point, effect);
        Ok(())
    }
    fn DragOver(
        &self,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.effect(point, effect);
        Ok(())
    }
    fn DragLeave(&self) -> windows::core::Result<()> {
        self.cancel();
        Ok(())
    }
    fn Drop(
        &self,
        _: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = self.paths.borrow_mut().take();
        let accepted = if effect.is_null() {
            false
        } else {
            (unsafe { *effect & DROPEFFECT_COPY == DROPEFFECT_COPY })
                && paths
                    .zip(self.point(point))
                    .is_some_and(|(paths, p)| (self.callbacks.drop)(p, paths))
        };
        if !effect.is_null() {
            unsafe {
                *effect = if accepted {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                };
            }
        }
        self.cancel();
        Ok(())
    }
}
fn selected_paths(data: &IDataObject) -> anyhow::Result<Vec<PathBuf>> {
    let format = FORMATETC {
        cfFormat: 15,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    };
    let mut medium = unsafe { data.GetData(&format)? };
    let result = (|| {
        anyhow::ensure!(
            medium.tymed == TYMED_HGLOBAL.0 as u32,
            "expected file paths"
        );
        let drop = HDROP(unsafe { medium.u.hGlobal.0 });
        let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
        anyhow::ensure!((1..=128).contains(&count), "choose 1 to 128 regular files");
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let len = unsafe { DragQueryFileW(drop, index, None) };
            anyhow::ensure!(len > 0 && len <= 32767, "invalid file path");
            let mut path = vec![0; len as usize + 1];
            anyhow::ensure!(
                unsafe { DragQueryFileW(drop, index, Some(&mut path)) } == len,
                "file path changed"
            );
            paths.push(PathBuf::from(std::ffi::OsString::from_wide(
                &path[..len as usize],
            )));
        }
        crate::windows_package::validate_send_paths(&paths)?;
        Ok(paths)
    })();
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    result
}
struct Registration {
    hwnd: HWND,
    _target: IDropTarget,
}
impl Drop for Registration {
    fn drop(&mut self) {
        unsafe {
            let _ = RevokeDragDrop(self.hwnd);
            OleUninitialize();
        }
    }
}
thread_local! { static REGISTRATION: RefCell<Option<Registration>> = const { RefCell::new(None) }; }
pub(crate) fn install(window: &PneuWindow, callbacks: Callbacks) {
    REGISTRATION.with(|state| {
        if state.borrow().is_some() {
            return;
        }
        window.window().with_winit_window(|native| {
            let Ok(handle) = native.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return;
            };
            let hwnd = HWND(handle.hwnd.get() as _);
            let result = unsafe { OleInitialize(None) }.and_then(|_| {
                let target: IDropTarget = Target {
                    hwnd,
                    callbacks,
                    paths: RefCell::new(None),
                }
                .into();
                match unsafe { RegisterDragDrop(hwnd, &target) } {
                    Ok(()) => {
                        *state.borrow_mut() = Some(Registration {
                            hwnd,
                            _target: target,
                        });
                        Ok(())
                    }
                    Err(error) => {
                        unsafe {
                            OleUninitialize();
                        }
                        Err(error)
                    }
                }
            });
            if let Err(error) = result {
                eprintln!("pneud: file drop target: {error}");
            }
        });
    });
}
pub(crate) fn cleanup() {
    REGISTRATION.with(|s| {
        s.borrow_mut().take();
    });
}
