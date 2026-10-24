//! Camera lifetime belongs to the UI child, never Core or the daemon. Native
//! callbacks only replace a bounded mailbox; Slint images stay on its main thread.
use crate::ui::{
    camera::{Generation, Permission, State as CameraState, pairing_payload},
    window::{Page, PneuWindow, ScanState, Ui},
};
use slint::ComponentHandle;
use std::{cell::RefCell, ffi::c_void, sync::Mutex, time::Duration};

type Event = unsafe extern "C" fn(u64, i32, *const u8, usize, u32, u32);
type Filter = unsafe extern "C" fn(*const u8, usize) -> bool;
unsafe extern "C" {
    fn pneu_camera_available() -> bool;
    fn pneu_camera_authorization() -> i32;
    fn pneu_camera_request_authorization(generation: u64, callback: Event);
    fn pneu_camera_start(generation: u64, callback: Event, filter: Filter) -> *mut c_void;
    fn pneu_camera_stop(handle: *mut c_void, wait: bool);
    fn pneu_camera_wait_for_stops();
    fn pneu_camera_is_running() -> bool;
    #[cfg(test)]
    fn pneu_camera_decode_bgra(
        bytes: *const u8,
        len: usize,
        width: u32,
        height: u32,
        callback: Event,
    ) -> bool;
}

struct Frame {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}
#[derive(Default)]
struct Mailbox {
    generation: Option<u64>,
    frame: Option<Frame>,
    payload: Option<String>,
    error: bool,
    permission: Option<i32>,
}
static MAILBOX: Mutex<Mailbox> = Mutex::new(Mailbox {
    generation: None,
    frame: None,
    payload: None,
    error: false,
    permission: None,
});
struct State {
    generation: Generation,
    current: u64,
    handle: *mut c_void,
    _timer: slint::Timer,
    tick: u32,
}
thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }

// The bridge borrows these buffers only for the callback. Copy bounded data
// before returning; never save a pixel buffer, sample buffer or ObjC pointer.
unsafe extern "C" fn event(
    generation: u64,
    kind: i32,
    bytes: *const u8,
    len: usize,
    width: u32,
    height: u32,
) {
    let Ok(mut mailbox) = MAILBOX.lock() else {
        return;
    };
    if mailbox.generation != Some(generation) {
        return;
    }
    match kind {
        1 if !bytes.is_null()
            && width > 0
            && height > 0
            && width <= 1280
            && height <= 720
            && len == width as usize * height as usize * 4 =>
        {
            mailbox.frame = Some(Frame {
                bytes: unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec(),
                width,
                height,
            });
        }
        2 if !bytes.is_null() && len <= 8193 => {
            if let Ok(text) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes, len) })
                && let Some(payload) = pairing_payload(text)
            {
                mailbox.payload = Some(payload.to_owned());
            }
        }
        3 => mailbox.error = true,
        4 => mailbox.permission = Some(width as i32),
        _ => {}
    }
}
unsafe extern "C" fn filter(bytes: *const u8, len: usize) -> bool {
    if bytes.is_null() || len > 8193 {
        return false;
    }
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes, len) })
        .ok()
        .and_then(pairing_payload)
        .is_some()
}

pub(crate) fn install(window: &PneuWindow) {
    super::native::main_thread();
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            if let Some(window) = weak.upgrade() {
                poll(&window);
            }
        },
    );
    STATE.with(|state| {
        *state.borrow_mut() = Some(State {
            generation: Generation::default(),
            current: 0,
            handle: std::ptr::null_mut(),
            _timer: timer,
            tick: 0,
        })
    });
}

fn stop(state: &mut State, wait: bool) {
    state.generation.close();
    *MAILBOX.lock().expect("camera mailbox") = Mailbox::default();
    if !state.handle.is_null() {
        unsafe {
            pneu_camera_stop(
                std::mem::replace(&mut state.handle, std::ptr::null_mut()),
                wait,
            );
        }
    }
}
pub(crate) fn close() {
    STATE.with(|state| {
        if let Some(state) = state.borrow_mut().as_mut() {
            stop(state, false);
        }
    });
}
pub(crate) fn cleanup() {
    STATE.with(|state| {
        if let Some(mut state) = state.borrow_mut().take() {
            stop(&mut state, true);
            unsafe {
                pneu_camera_wait_for_stops();
            }
        }
    });
}
pub(crate) fn assert_stopped() -> anyhow::Result<()> {
    unsafe {
        pneu_camera_wait_for_stops();
    }
    anyhow::ensure!(
        !unsafe { pneu_camera_is_running() },
        "capture session is still running"
    );
    anyhow::ensure!(
        STATE.with(|state| state
            .borrow()
            .as_ref()
            .is_none_or(|state| state.handle.is_null())),
        "camera handle is still active"
    );
    Ok(())
}
pub(crate) fn navigated(window: &PneuWindow) {
    if window.global::<Ui>().get_page() != Page::Scan {
        close();
        window
            .global::<Ui>()
            .set_camera_preview(slint::Image::default());
    }
    // Discovery doesn't request access or open the device. Recheck each Add
    // Device visit, and periodically while the Computer rows are visible.
    if window.global::<Ui>().get_page() == Page::Add {
        refresh_available(window);
    }
}
fn refresh_available(window: &PneuWindow) {
    // Headless shared UI tests have no installed native state.
    if STATE.with(|state| state.borrow().is_some()) {
        window
            .global::<Ui>()
            .set_camera_available(unsafe { pneu_camera_available() });
    }
}

pub(crate) fn open(window: &PneuWindow) {
    if window.global::<Ui>().get_page() != Page::Scan
        || !super::native::camera_window_visible(window)
    {
        return;
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(state) = state.as_mut() else { return };
        stop(state, false);
        state.current = state.generation.open();
        MAILBOX.lock().expect("camera mailbox").generation = Some(state.current);
        window
            .global::<Ui>()
            .set_camera_preview(slint::Image::default());
        authorize(window, state);
    });
}

fn authorize(window: &PneuWindow, state: &mut State) {
    let permission = Permission::from_native(unsafe { pneu_camera_authorization() });
    let presentation = CameraState::new(unsafe { pneu_camera_available() }, permission);
    let ui = window.global::<Ui>();
    match presentation {
        CameraState::Unavailable => ui.set_scan_state(ScanState::Unavailable),
        CameraState::Denied => ui.set_scan_state(ScanState::Denied),
        CameraState::Requesting => {
            ui.set_scan_state(ScanState::Requesting);
            unsafe {
                pneu_camera_request_authorization(state.current, event);
            }
        }
        CameraState::Scanning => {
            ui.set_scan_state(ScanState::Scanning);
            state.handle = unsafe { pneu_camera_start(state.current, event, filter) };
        }
    }
}

fn poll(window: &PneuWindow) {
    let ui = window.global::<Ui>();
    let payload = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let state = state.as_mut()?;
        state.tick = state.tick.wrapping_add(1);
        if ui.get_page() == Page::Add && ui.get_add_tab() == 1 && state.tick.is_multiple_of(10) {
            ui.set_camera_available(unsafe { pneu_camera_available() });
        }
        if ui.get_page() != Page::Scan || !super::native::camera_window_visible(window) {
            if ui.get_page() == Page::Scan && state.generation.accepts(state.current) {
                ui.set_scan_state(ScanState::Failed);
            }
            stop(state, false);
            ui.set_camera_preview(slint::Image::default());
            return None;
        }
        if !state.generation.accepts(state.current) {
            return None;
        }
        let (frame, payload, error, permission) = {
            let mut mailbox = MAILBOX.lock().expect("camera mailbox");
            (
                mailbox.frame.take(),
                mailbox.payload.take(),
                std::mem::take(&mut mailbox.error),
                mailbox.permission.take(),
            )
        };
        if payload.is_some() {
            stop(state, false);
            ui.set_camera_preview(slint::Image::default());
            return payload;
        }
        if error {
            stop(state, false);
            ui.set_camera_preview(slint::Image::default());
            ui.set_scan_state(
                if matches!(
                    Permission::from_native(unsafe { pneu_camera_authorization() }),
                    Permission::Denied | Permission::Restricted
                ) {
                    state.current = state.generation.open();
                    MAILBOX.lock().expect("camera mailbox").generation = Some(state.current);
                    ScanState::Denied
                } else {
                    ScanState::Failed
                },
            );
            return None;
        }
        if permission.is_some()
            || (state.tick.is_multiple_of(5)
                && ui.get_scan_state() == ScanState::Denied
                && unsafe { pneu_camera_authorization() } == 3)
        {
            authorize(window, state);
        }
        if let Some(frame) = frame {
            let mut pixels =
                slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(frame.width, frame.height);
            // Mirror just the webcam preview; Vision sees the original pixels.
            for (y, row) in pixels
                .make_mut_slice()
                .chunks_mut(frame.width as usize)
                .enumerate()
            {
                for (x, pixel) in row.iter_mut().enumerate() {
                    let offset = (y * frame.width as usize + frame.width as usize - x - 1) * 4;
                    *pixel = slint::Rgb8Pixel::new(
                        frame.bytes[offset + 2],
                        frame.bytes[offset + 1],
                        frame.bytes[offset],
                    );
                }
            }
            ui.set_camera_preview(slint::Image::from_rgb8(pixels));
        }
        None
    });
    if let Some(payload) = payload {
        // Exactly the Paste Link callback, after returning to its ordinary page.
        // It captures the new navigation scope and uses Join -> confirmation.
        crate::ui::window::scanned_pairing(window, payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static DECODED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    unsafe extern "C" fn decoded(_: u64, kind: i32, bytes: *const u8, len: usize, _: u32, _: u32) {
        assert_eq!(kind, 2);
        let text = std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes, len) }).unwrap();
        DECODED.lock().unwrap().push(text.to_owned());
    }
    #[test]
    fn vision_decodes_generated_still_through_capture_decoder() {
        let payload = "pneu://pair?v=1&name=Vision%20test&fp=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef&addr=192.0.2.1%3A47822&token=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef&exp=2000000000";
        let qr = qrcode::QrCode::new(payload).unwrap();
        let size = (qr.width() + 8) * 6;
        let mut bgra = vec![255u8; size * size * 4];
        for y in 0..qr.width() {
            for x in 0..qr.width() {
                if qr[(x, y)] == qrcode::Color::Dark {
                    for dy in 0..6 {
                        for dx in 0..6 {
                            let offset = (((y + 4) * 6 + dy) * size + (x + 4) * 6 + dx) * 4;
                            bgra[offset..offset + 3].fill(0);
                        }
                    }
                }
            }
        }
        DECODED.lock().unwrap().clear();
        assert!(unsafe {
            pneu_camera_decode_bgra(bgra.as_ptr(), bgra.len(), size as u32, size as u32, decoded)
        });
        assert_eq!(*DECODED.lock().unwrap(), [payload]);
        assert!(!unsafe {
            pneu_camera_decode_bgra(bgra.as_ptr(), 1, size as u32, size as u32, decoded)
        });
    }
}
