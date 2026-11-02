//! Native drag gestures retain their exact drop point and selected paths.
//! winit's file events omit coordinates and gesture boundaries, so this child's
//! own delegate gets an ivar-free subclass for NSDraggingDestination only. Every
//! non-drag winit method remains inherited, with the same instance layout.
use crate::ui::window::PneuWindow;
use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel},
    sel,
};
use objc2_foundation::{NSArray, NSString};
use slint::{
    ComponentHandle,
    winit_030::{
        WinitWindowAccessor,
        winit::raw_window_handle::{HasWindowHandle, RawWindowHandle},
    },
};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::OnceLock};

type Point = (f64, f64);
#[derive(Clone)]
pub(crate) struct Callbacks {
    pub begin: Rc<dyn Fn(&[PathBuf]) -> bool>,
    pub hover: Rc<dyn Fn(Point) -> bool>,
    pub drop: Rc<dyn Fn(Point, Vec<PathBuf>) -> bool>,
    pub cancel: Rc<dyn Fn()>,
}
struct Gesture {
    sequence: isize,
    paths: Vec<PathBuf>,
}
struct Capture {
    _delegate: Retained<AnyObject>,
    callbacks: Callbacks,
    gesture: Option<Gesture>,
}
thread_local! {static CAPTURE:RefCell<Option<Capture>>=const {RefCell::new(None)};}
static ORIGINAL: OnceLock<&'static AnyClass> = OnceLock::new();
static SUBCLASS: OnceLock<&'static AnyClass> = OnceLock::new();

unsafe fn point(sender: &AnyObject) -> Option<Point> {
    unsafe {
        let window: Option<Retained<AnyObject>> = msg_send![sender, draggingDestinationWindow];
        let window = window?;
        let view: Option<Retained<AnyObject>> = msg_send![&*window, contentView];
        let view = view?;
        let location: objc2_foundation::NSPoint = msg_send![sender, draggingLocation];
        let location: objc2_foundation::NSPoint =
            msg_send![&*view,convertPoint:location,fromView:Option::<&AnyObject>::None];
        let bounds: objc2_foundation::NSRect = msg_send![&*view, bounds];
        let flipped: Bool = msg_send![&*view, isFlipped];
        Some((
            location.x,
            if flipped.as_bool() {
                location.y
            } else {
                bounds.size.height - location.y
            },
        ))
    }
}
unsafe fn selected_paths(sender: &AnyObject) -> Option<Vec<PathBuf>> {
    unsafe {
        let pasteboard: Retained<AnyObject> = msg_send![sender, draggingPasteboard];
        let value: Option<Retained<AnyObject>> = msg_send![&*pasteboard,propertyListForType:&*NSString::from_str("NSFilenamesPboardType")];
        let value = value?;
        let array: Bool = msg_send![&*value,isKindOfClass:objc2::class!(NSArray)];
        if !array.as_bool() {
            return None;
        }
        let values: Retained<NSArray<AnyObject>> = Retained::cast_unchecked(value);
        if values.is_empty() || values.len() > 128 {
            return None;
        }
        values
            .iter()
            .map(|value| {
                let string: Bool = msg_send![&*value,isKindOfClass:objc2::class!(NSString)];
                if !string.as_bool() {
                    return None;
                }
                let value: Retained<NSString> = Retained::cast_unchecked(value);
                let path = PathBuf::from(value.to_string());
                path.is_absolute().then_some(path)
            })
            .collect()
    }
}
unsafe fn sequence(sender: &AnyObject) -> isize {
    unsafe { msg_send![sender, draggingSequenceNumber] }
}
fn cancel() {
    let callback = CAPTURE.with(|s| {
        let mut state = s.borrow_mut();
        let s = state.as_mut()?;
        s.gesture = None;
        Some(s.callbacks.cancel.clone())
    });
    if let Some(callback) = callback {
        callback();
    }
}
unsafe extern "C-unwind" fn entered(
    _receiver: *mut AnyObject,
    _selector: Sel,
    sender: *mut AnyObject,
) -> Bool {
    unsafe {
        cancel();
        let sender = &*sender;
        let Some(paths) = selected_paths(sender) else {
            return Bool::NO;
        };
        let callbacks = CAPTURE.with(|s| s.borrow().as_ref().map(|s| s.callbacks.clone()));
        let Some(callbacks) = callbacks else {
            return Bool::NO;
        };
        // This callback captures the currently rendered authorities immediately,
        // before a queued Slint status update can replace the displayed binding.
        if !(callbacks.begin)(&paths) {
            return Bool::NO;
        }
        CAPTURE.with(|s| {
            if let Some(s) = s.borrow_mut().as_mut() {
                s.gesture = Some(Gesture {
                    sequence: sequence(sender),
                    paths,
                });
            }
        });
        Bool::YES // NSDragOperationCopy (the same ABI as winit's existing method).
    }
}
unsafe extern "C-unwind" fn updated(
    _receiver: *mut AnyObject,
    _selector: Sel,
    sender: *mut AnyObject,
) -> usize {
    unsafe {
        let sender = &*sender;
        let sequence = sequence(sender);
        let callback = CAPTURE.with(|s| {
            s.borrow()
                .as_ref()
                .filter(|s| s.gesture.as_ref().is_some_and(|g| g.sequence == sequence))
                .map(|s| s.callbacks.hover.clone())
        });
        match (callback, point(sender)) {
            (Some(callback), Some(point)) => usize::from(callback(point)),
            _ => 0,
        }
    }
}
unsafe extern "C-unwind" fn performed(
    _receiver: *mut AnyObject,
    _selector: Sel,
    sender: *mut AnyObject,
) -> Bool {
    unsafe {
        let sender = &*sender;
        let sequence = sequence(sender);
        let gesture = CAPTURE.with(|s| {
            let mut state = s.borrow_mut();
            let s = state.as_mut()?;
            Some((s.gesture.take()?, s.callbacks.clone()))
        });
        let accepted = if let (Some((gesture, callbacks)), Some(point)) = (gesture, point(sender)) {
            gesture.sequence == sequence && (callbacks.drop)(point, gesture.paths)
        } else {
            false
        };
        cancel();
        Bool::new(accepted)
    }
}
unsafe extern "C-unwind" fn exited(
    _receiver: *mut AnyObject,
    _selector: Sel,
    _sender: *mut AnyObject,
) {
    cancel();
}
unsafe extern "C-unwind" fn ended(
    _receiver: *mut AnyObject,
    _selector: Sel,
    _sender: *mut AnyObject,
) {
    cancel();
}
unsafe extern "C-unwind" fn periodic(_receiver: *mut AnyObject, _selector: Sel) -> Bool {
    Bool::YES
}

pub(crate) fn install(window: &PneuWindow, callbacks: Callbacks) {
    if CAPTURE.with(|s| s.borrow().is_some()) {
        return;
    }
    window.window().with_winit_window(|window| unsafe {
        super::native::main_thread();
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };
        let view = &*handle.ns_view.as_ptr().cast::<AnyObject>();
        let native: Option<Retained<AnyObject>> = msg_send![view, window];
        let Some(native) = native else { return };
        let delegate: Option<Retained<AnyObject>> = msg_send![&*native, delegate];
        let Some(delegate) = delegate else { return };
        let class = delegate.class();
        if ![sel!(draggingEntered:), sel!(performDragOperation:)]
            .iter()
            .all(|selector| class.instance_method(*selector).is_some())
        {
            return;
        }
        if !std::ptr::eq(*ORIGINAL.get_or_init(|| class), class) {
            return;
        }
        let subclass = SUBCLASS.get_or_init(|| {
            let mut builder =
                ClassBuilder::new(c"PneuWindowDropDelegate", class).expect("unique drag subclass");
            builder.add_method(
                sel!(draggingEntered:),
                entered as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool,
            );
            builder.add_method(
                sel!(draggingUpdated:),
                updated
                    as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize,
            );
            builder.add_method(
                sel!(performDragOperation:),
                performed
                    as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool,
            );
            builder.add_method(
                sel!(draggingExited:),
                exited as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
            );
            builder.add_method(
                sel!(draggingEnded:),
                ended as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
            );
            builder.add_method(
                sel!(wantsPeriodicDraggingUpdates),
                periodic as unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool,
            );
            builder.register()
        });
        assert_eq!(class.instance_size(), subclass.instance_size());
        // Same main-thread object, compatible existing signatures, no new ivars.
        assert!(std::ptr::eq(
            AnyObject::set_class(&delegate, subclass),
            class
        ));
        CAPTURE.with(|s| {
            *s.borrow_mut() = Some(Capture {
                _delegate: delegate.clone(),
                callbacks,
                gesture: None,
            })
        });
        // Refresh AppKit's cached optional delegate methods.
        let _: () = msg_send![&*native,setDelegate:Option::<&AnyObject>::None];
        let _: () = msg_send![&*native,setDelegate:&*delegate];
    });
}
pub(crate) fn cleanup() {
    cancel();
    CAPTURE.with(|s| s.borrow_mut().take());
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::{AnyThread, DeclaredClass, define_class};
    use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};
    use std::cell::Cell;

    #[derive(Default)]
    struct Values {
        point: Cell<NSPoint>,
        sequence: Cell<isize>,
        paths: RefCell<Vec<String>>,
    }
    // Exercise the native NSDraggingInfo selectors without creating AppKit
    // windows, accessing Finder or requiring screen/Accessibility permission.
    define_class!(
        #[unsafe(super(NSObject))]
        #[ivars = Values]
        struct PneuTestDragInfo;
        impl PneuTestDragInfo {
            #[unsafe(method(draggingDestinationWindow))]
            fn window(&self) -> *const AnyObject { (self as *const Self).cast() }
            #[unsafe(method(contentView))]
            fn view(&self) -> *const AnyObject { (self as *const Self).cast() }
            #[unsafe(method(draggingPasteboard))]
            fn pasteboard(&self) -> *const AnyObject { (self as *const Self).cast() }
            #[unsafe(method(draggingLocation))]
            fn location(&self) -> NSPoint { self.ivars().point.get() }
            #[unsafe(method(convertPoint:fromView:))]
            fn convert(&self, point: NSPoint, _view: *const AnyObject) -> NSPoint { point }
            #[unsafe(method(bounds))]
            fn bounds(&self) -> NSRect { NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(400.0, 300.0)) }
            #[unsafe(method(isFlipped))]
            fn flipped(&self) -> Bool { Bool::YES }
            #[unsafe(method(draggingSequenceNumber))]
            fn sequence(&self) -> isize { self.ivars().sequence.get() }
            #[unsafe(method(propertyListForType:))]
            fn paths(&self, _kind: &NSString) -> *mut NSArray<NSString> {
                let paths = self.ivars().paths.borrow().iter().map(|p| NSString::from_str(p)).collect::<Vec<_>>();
                Retained::autorelease_ptr(NSArray::from_slice(&paths.iter().map(|p| &**p).collect::<Vec<_>>()))
            }
        }
    );

    #[test]
    fn native_drag_preserves_gesture_paths_and_exact_drop_point_and_rejects_stale_sequences() {
        objc2::rc::autoreleasepool(|_| unsafe {
            let info = PneuTestDragInfo::alloc().set_ivars(Values::default());
            let info: Retained<PneuTestDragInfo> = msg_send![super(info), init];
            info.ivars().point.set(NSPoint::new(20.0, 40.0));
            *info.ivars().paths.borrow_mut() =
                vec!["/private/first.txt".into(), "/private/second.txt".into()];
            let captured = Rc::new(RefCell::new(None));
            let output = captured.clone();
            let begins = Rc::new(Cell::new(0));
            let count = begins.clone();
            CAPTURE.with(|s| {
                *s.borrow_mut() = Some(Capture {
                    _delegate: Retained::cast_unchecked(info.clone()),
                    callbacks: Callbacks {
                        begin: Rc::new(move |_| {
                            count.set(count.get() + 1);
                            true
                        }),
                        hover: Rc::new(|point| point.0 < 100.0),
                        drop: Rc::new(move |point, paths| {
                            *output.borrow_mut() = Some((point, paths));
                            true
                        }),
                        cancel: Rc::new(|| {}),
                    },
                    gesture: None,
                })
            });
            struct Clean;
            impl Drop for Clean {
                fn drop(&mut self) {
                    cleanup();
                }
            }
            let _clean = Clean;
            let sender = Retained::as_ptr(&info).cast_mut().cast::<AnyObject>();
            assert!(entered(std::ptr::null_mut(), sel!(draggingEntered:), sender).as_bool());
            // The pasteboard can change after entry; only the frozen selection
            // belongs to this gesture, while the final position stays exact.
            *info.ivars().paths.borrow_mut() = vec!["/private/replacement.txt".into()];
            info.ivars().point.set(NSPoint::new(75.0, 80.0));
            assert_eq!(
                updated(std::ptr::null_mut(), sel!(draggingUpdated:), sender),
                1
            );
            assert!(performed(std::ptr::null_mut(), sel!(performDragOperation:), sender).as_bool());
            assert_eq!(
                captured.borrow_mut().take(),
                Some((
                    (75.0, 80.0),
                    vec![
                        PathBuf::from("/private/first.txt"),
                        PathBuf::from("/private/second.txt")
                    ]
                ))
            );
            assert_eq!(begins.get(), 1);
            assert!(entered(std::ptr::null_mut(), sel!(draggingEntered:), sender).as_bool());
            info.ivars().sequence.set(1);
            assert_eq!(
                updated(std::ptr::null_mut(), sel!(draggingUpdated:), sender),
                0
            );
            assert!(
                !performed(std::ptr::null_mut(), sel!(performDragOperation:), sender).as_bool()
            );
            assert!(captured.borrow().is_none());
            *info.ivars().paths.borrow_mut() = vec!["relative.txt".into()];
            assert!(!entered(std::ptr::null_mut(), sel!(draggingEntered:), sender).as_bool());
        });
    }
}
