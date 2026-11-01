//! Qt owns the Wayland data device; receive native file drops on its QWidget.
//! All callbacks and destruction run on Slint's UI thread.
use super::window::PneuWindow;
use i_slint_backend_qt::QtWidgetAccessor;
use slint::ComponentHandle;
use std::{cell::RefCell, os::unix::ffi::OsStrExt, path::PathBuf, rc::Rc};

type Point = (f64, f64);
#[derive(Clone)]
pub(crate) struct Callbacks {
    pub begin: Rc<dyn Fn() -> bool>,
    pub hover: Rc<dyn Fn(Point) -> bool>,
    pub drop: Rc<dyn Fn(Point, Vec<PathBuf>) -> bool>,
    pub cancel: Rc<dyn Fn()>,
}

// QUrl::FullyEncoded preserves percent-encoded Unix filename bytes. Reject the
// whole offer if any URL is remote or malformed; never silently send a subset.
fn selected_paths(data: &[u8]) -> Option<Vec<PathBuf>> {
    if data.is_empty() || data.len() > 128 * 32768 || !data.ends_with(&[0]) {
        return None;
    }
    let mut paths = Vec::new();
    for encoded in data[..data.len() - 1].split(|b| *b == 0) {
        if paths.len() == 128 || encoded.is_empty() || encoded.len() > 32768 {
            return None;
        }
        let encoded = std::str::from_utf8(encoded).ok()?;
        // Decode the original path: URL parsers normalize dot segments, which
        // can select a different file when a preceding directory is a symlink.
        let (host, raw_path) = encoded.strip_prefix("file://")?.split_once('/')?;
        if (!host.is_empty() && !host.eq_ignore_ascii_case("localhost"))
            || raw_path.contains(['?', '#'])
        {
            return None;
        }
        let mut path = vec![b'/'];
        let mut bytes = raw_path.bytes();
        while let Some(byte) = bytes.next() {
            let byte = if byte == b'%' {
                let high = char::from(bytes.next()?).to_digit(16)?;
                let low = char::from(bytes.next()?).to_digit(16)?;
                ((high << 4) | low) as u8
            } else {
                byte
            };
            if byte == 0 {
                return None;
            }
            path.push(byte);
        }
        if path
            .split(|byte| *byte == b'/')
            .any(|segment| matches!(segment, b"." | b".."))
        {
            return None;
        }
        paths.push(PathBuf::from(std::ffi::OsStr::from_bytes(&path)));
    }
    Some(paths)
}

struct State {
    callbacks: Callbacks,
    paths: RefCell<Option<Vec<PathBuf>>>,
}
impl State {
    fn cancel(&self) {
        self.paths.borrow_mut().take();
        (self.callbacks.cancel)();
    }
    fn event(&self, kind: i32, point: Point, data: &[u8]) -> bool {
        match kind {
            1 => {
                self.cancel();
                let Some(paths) = selected_paths(data) else {
                    return false;
                };
                if !(self.callbacks.begin)() {
                    return false;
                }
                *self.paths.borrow_mut() = Some(paths);
                (self.callbacks.hover)(point);
                // Qt must accept enter even outside a row to receive moves.
                true
            }
            2 => {
                let active = self.paths.borrow().is_some();
                active && (self.callbacks.hover)(point)
            }
            3 => {
                let paths = self.paths.borrow_mut().take();
                let accepted = paths.is_some_and(|paths| {
                    selected_paths(data).as_ref() == Some(&paths)
                        && (self.callbacks.drop)(point, paths)
                });
                self.cancel();
                accepted
            }
            _ => {
                self.cancel();
                false
            }
        }
    }
}

// The filter owns the boxed state and releases it once, including when Qt
// destroys the widget before our registration guard. No event pointers escape.
extern "C" fn event(
    context: *mut State,
    kind: i32,
    x: f64,
    y: f64,
    data: *const u8,
    len: usize,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if kind == 5 {
            let state = unsafe { Box::from_raw(context) };
            state.cancel();
            return false;
        }
        let state = unsafe { &*context };
        let data = if len == 0 {
            &[]
        } else {
            // Only the synchronous filter call supplies this bounded buffer.
            unsafe { std::slice::from_raw_parts(data, len) }
        };
        state.event(kind, (x, y), data)
    }));
    if result.is_err() && kind != 5 {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (&*context).cancel();
        }));
    }
    i32::from(result.unwrap_or(false))
}

cpp::cpp! {{ #include "linux_drag.cpp" }}

struct Registration(*mut ());
impl Drop for Registration {
    fn drop(&mut self) {
        let registration = self.0;
        unsafe {
            cpp::cpp!([registration as "QPointer<PneuDropFilter>*"] {
                delete registration->data();
                delete registration;
            });
        }
    }
}
thread_local! {
    static REGISTRATION: RefCell<Option<Registration>> = const { RefCell::new(None) };
}
pub(crate) fn install(window: &PneuWindow, callbacks: Callbacks) -> bool {
    let Some(widget) = window.window().qt_widget_ptr() else {
        return false;
    };
    REGISTRATION.with(|slot| {
        if slot.borrow().is_some() {
            return true;
        }
        let widget = widget.as_ptr();
        let context = Box::into_raw(Box::new(State {
            callbacks,
            paths: RefCell::new(None),
        }));
        let callback = event as extern "C" fn(*mut State, i32, f64, f64, *const u8, usize) -> i32;
        let registration = unsafe {
            cpp::cpp!([widget as "QWidget*", callback as "PneuDropCallback", context as "void*"] -> *mut () as "QPointer<PneuDropFilter>*" {
                return new QPointer<PneuDropFilter>(new PneuDropFilter(widget, callback, context));
            })
        };
        *slot.borrow_mut() = Some(Registration(registration));
        true
    })
}
pub(crate) fn cleanup() {
    // Drop outside the RefCell borrow: cancellation can update Slint properties.
    let registration = REGISTRATION.with(|slot| slot.borrow_mut().take());
    drop(registration);
}

#[cfg(test)]
pub(super) fn device_pixel_ratio(window: &PneuWindow) -> Option<f64> {
    let widget = window.window().qt_widget_ptr()?.as_ptr();
    Some(unsafe {
        cpp::cpp!([widget as "QWidget*"] -> f64 as "double" {
            return widget->devicePixelRatioF();
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enter_outside_rows_leave_repeat_and_rejected_drop_clear_state() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let log = events.clone();
        let begin = Rc::new(move || {
            log.borrow_mut().push("begin");
            true
        });
        let log = events.clone();
        let hover = Rc::new(move |_| {
            log.borrow_mut().push("hover");
            false
        });
        let log = events.clone();
        let drop = Rc::new(move |_, paths: Vec<PathBuf>| {
            assert_eq!(paths, [PathBuf::from("/tmp/a")]);
            log.borrow_mut().push("drop");
            false
        });
        let log = events.clone();
        let state = State {
            callbacks: Callbacks {
                begin,
                hover,
                drop,
                cancel: Rc::new(move || log.borrow_mut().push("cancel")),
            },
            paths: RefCell::new(None),
        };
        let urls = b"file:///tmp/a\0";
        assert!(state.event(1, (1.0, 2.0), urls));
        assert!(!state.event(2, (3.0, 4.0), &[]));
        state.event(4, (0.0, 0.0), &[]);
        assert!(!state.event(3, (1.0, 2.0), urls));
        assert!(state.event(1, (1.0, 2.0), urls));
        assert!(!state.event(3, (1.0, 2.0), urls));
        assert!(state.paths.borrow().is_none());
        assert_eq!(
            *events.borrow(),
            [
                "cancel", "begin", "hover", "hover", "cancel", "cancel", "cancel", "begin",
                "hover", "drop", "cancel"
            ]
        );
        // An offer that changes between enter and drop cannot send files.
        assert!(state.event(1, (1.0, 2.0), urls));
        assert!(!state.event(3, (1.0, 2.0), b"file:///tmp/b\0"));
        assert_eq!(events.borrow().iter().filter(|&&e| e == "drop").count(), 1);
    }
    #[test]
    fn local_urls_preserve_unicode_and_unix_bytes() {
        let paths = selected_paths(
            b"file:///tmp/a%20b\0file:///tmp/caf%C3%A9-%E6%B5%8B%E8%AF%95\0file:///tmp/%FF\0file://localhost/tmp/%252e%252e%2B%3F%23%25%5C\0",
        )
        .unwrap();
        assert_eq!(paths[0], PathBuf::from("/tmp/a b"));
        assert_eq!(paths[1], PathBuf::from("/tmp/café-测试"));
        assert_eq!(paths[2].as_os_str().as_bytes(), b"/tmp/\xff");
        assert_eq!(paths[3], PathBuf::from("/tmp/%2e%2e+?#%\\"));
    }
    #[test]
    fn rejects_dot_segments_before_symlink_path_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("target/child")).unwrap();
        std::os::unix::fs::symlink(root.join("target/child"), root.join("link")).unwrap();
        std::fs::write(root.join("target/file"), b"intended file").unwrap();
        std::fs::write(root.join("file"), b"different file").unwrap();
        assert_eq!(
            std::fs::read(root.join("link/../file")).unwrap(),
            b"intended file"
        );
        let base = url::Url::from_directory_path(root).unwrap();
        let traversal = format!("{base}link/../file");
        let normalized = url::Url::parse(&traversal).unwrap().to_file_path().unwrap();
        assert_eq!(std::fs::read(normalized).unwrap(), b"different file");
        for segment in ["..", ".", "%2e", "%2E", "%2e%2e", ".%2E", "%2e."] {
            let offer = format!("{base}link/{segment}/file\0");
            assert!(selected_paths(offer.as_bytes()).is_none(), "{offer:?}");
        }
    }
    #[test]
    fn rejects_remote_mixed_empty_nul_and_oversized_offers() {
        for data in [
            &b""[..],
            b"file:///tmp/a",
            b"file:///tmp/a\0\0",
            b"https://example.com/a\0",
            b"file://server/tmp/a\0",
            b"file:///tmp/a\0https://example.com/b\0",
            b"file:///tmp/%00\0",
            b"file:///tmp/a?query\0",
            b"file:///tmp/a#fragment\0",
            b"file:relative-name\0",
            b"file:///tmp/%\0",
            b"file:///tmp/%2\0",
            b"file:///tmp/%GG\0",
            b"file:///tmp/link%2f%2e%2e%2ffile\0",
        ] {
            assert!(selected_paths(data).is_none(), "{data:?}");
        }
        assert!(selected_paths(&b"file:///tmp/a\0".repeat(128)).is_some());
        assert!(selected_paths(&b"file:///tmp/a\0".repeat(129)).is_none());
        assert!(
            selected_paths(&[b"file:///".as_slice(), &vec![b'a'; 32768], &[0]].concat()).is_none()
        );
    }
}
