//! AppKit lives exclusively in the persistent child's main-thread event loop.
use super::bridge::{Child, Notification, Request};
use crate::ui::notice_lifecycle::{CancelOnDrop, Lifecycle, RequestGuard};
use crate::ui::viewer_cache::materialize;
use crate::{
    Config,
    ui::{Snapshot, window::PneuWindow},
};
use anyhow::{Context, Result, ensure};
use block2::{DynBlock, RcBlock};
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, class, define_class, msg_send, sel};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSSet, NSString, NSURL};
use objc2_user_notifications::*;
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    collections::HashMap,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    ptr::NonNull,
    rc::Rc,
    sync::{Arc, Mutex},
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum MenuAction {
    Command(serde_json::Value),
    Send {
        recipients: Vec<crate::outbox::Authority>,
        targeted: bool,
        clipboard: bool,
    },
    Folder,
    Settings,
}
struct State {
    native: Native,
    window: slint::Weak<PneuWindow>,
    callback: Rc<dyn Fn(MenuAction)>,
    snapshot: Snapshot,
    config: Config,
    tracking: bool,
    child: Option<Child>,
    panels: HashMap<u64, Retained<AnyObject>>,
    notifications: HashMap<u64, Arc<NativeNotice>>,
    next_panel: u64,
    materials: crate::ui::viewer_cache::Materials,
    requests: HashMap<u64, Arc<std::sync::atomic::AtomicBool>>,
    jobs: Vec<std::thread::JoinHandle<()>>,
    cache: crate::ui::viewer_cache::ViewerCache,
    login_busy: Arc<std::sync::atomic::AtomicBool>,
    login_next: std::time::Instant,
    login_thread: Option<std::thread::JoinHandle<()>>,
}
thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }

// Window frames are prepared on the IPC worker, including clock and motion
// preferences. Publish main-thread reads as plain values rather than letting
// that worker (or a Headless test) call Foundation/AppKit.
static TWELVE_HOUR_CLOCK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static REDUCED_MOTION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn refresh_preferences() {
    main_thread();
    TWELVE_HOUR_CLOCK.store(
        read_twelve_hour_clock(),
        std::sync::atomic::Ordering::Relaxed,
    );
    REDUCED_MOTION.store(read_reduced_motion(), std::sync::atomic::Ordering::Relaxed);
}

/// Check before sending untyped main-thread-only AppKit messages. Keep this
/// enabled in release too: an accidental off-thread call must fail before ObjC.
pub(super) fn main_thread() -> MainThreadMarker {
    MainThreadMarker::new().expect("Pneu AppKit calls must run on the native main thread")
}

/// Shared window code also runs with Slint's Headless platform in tests. Only
/// install() may create native state, and it requires the main thread. Never
/// infer native availability from target_os or create NSApplication to probe it.
fn installed() -> bool {
    let installed = STATE.with(|s| s.borrow().is_some());
    if installed {
        main_thread();
    }
    installed
}

fn require_installed() -> Result<()> {
    ensure!(installed(), "native UI backend is not installed");
    Ok(())
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct MenuTarget;
    unsafe impl NSObjectProtocol for MenuTarget {}
    impl MenuTarget {
        #[unsafe(method(invoke:))]
        fn invoke(&self, sender: &AnyObject) {
            let payload: Option<Retained<NSString>> = unsafe {msg_send![sender, representedObject]};
            let action=payload.and_then(|p|serde_json::from_str::<MenuAction>(&p.to_string()).ok());
            let callback=STATE.with(|s|s.borrow().as_ref().map(|s|s.callback.clone()));
            if let (Some(callback),Some(action))=(callback,action){callback(action);}

        }
        #[unsafe(method(quit:))]
        fn quit(&self, _sender: &AnyObject) {
            super::camera::close();
            STATE.with(|s| { if let Some(child) = s.borrow().as_ref().and_then(|s| s.child.as_ref()) { child.action(0, String::new(), "quit".into()); } });
        }
        #[unsafe(method(closeWindow:))]
        fn close_window(&self, _sender: &AnyObject) {
            close_window();
        }
        #[unsafe(method(menuWillOpen:))]
        fn menu_open(&self, _menu: &AnyObject) { STATE.with(|s| { if let Some(s) = s.borrow_mut().as_mut() { s.tracking = true; } }); }
        #[unsafe(method(menuDidClose:))]
        fn menu_close(&self, _menu: &AnyObject) { STATE.with(|s| { if let Some(s) = s.borrow_mut().as_mut() { s.tracking = false; } }); }
    }
);
// Notification callbacks can arrive on any thread. Preserve both identifiers;
// attribution/authorization remains in the daemon's bounded target registry.
define_class!(
    #[unsafe(super(NSObject))]
    struct NotificationDelegate;
    unsafe impl NSObjectProtocol for NotificationDelegate {}
    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn response(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>,
        ) {
            autoreleasepool(|_| {
                let identifier = response.notification().request().identifier().to_string();
                let action = response.actionIdentifier().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some((namespace, id)) = identifier
                        .strip_prefix("pneu-")
                        .and_then(|v| v.rsplit_once('-'))
                        .and_then(|(namespace, id)| {
                            Some((namespace.to_owned(), id.parse::<u32>().ok()?))
                        })
                    {
                        let action = if action
                            == unsafe { UNNotificationDefaultActionIdentifier }.to_string()
                        {
                            "default".into()
                        } else if action
                            == unsafe { UNNotificationDismissActionIdentifier }.to_string()
                        {
                            "dismiss".into()
                        } else {
                            action
                        };
                        STATE.with(|s| {
                            if let Some(child) = s.borrow().as_ref().and_then(|s| s.child.as_ref())
                            {
                                child.action(id, namespace, action);
                            }
                        });
                    }
                });
                completion.call(());
            });
        }
    }
);
struct NativeNotice {
    notification: Notification,
    life: Arc<Lifecycle>,
}
fn remove_native(identifier: &str) {
    main_thread();
    autoreleasepool(|_| {
        let identifiers = NSArray::from_slice(&[&*NSString::from_str(identifier)]);
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.removePendingNotificationRequestsWithIdentifiers(&identifiers);
        center.removeDeliveredNotificationsWithIdentifiers(&identifiers);
    });
}
fn retire_stale_notices(identifiers: Vec<String>) {
    let active = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| {
                s.notifications
                    .values()
                    .filter(|n| n.life.active())
                    .map(|n| n.notification.identifier())
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default()
    });
    for id in crate::ui::notice_lifecycle::stale_identifiers(identifiers, |id| active.contains(id))
    {
        remove_native(&id);
    }
}
fn clear_stale_notices(guard: RequestGuard) {
    main_thread();
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(2));
    let finished = move |ids: Vec<String>| {
        let remaining = remaining.clone();
        let guard = guard.clone();
        let _ = slint::invoke_from_event_loop(move || {
            // Recheck on the main thread after each asynchronous getter: new
            // notifications may have been issued since the sweep started.
            let result = guard.run(|| {
                retire_stale_notices(ids);
                Ok(())
            });
            if result.is_err() || remaining.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                guard.finish(result);
            }
        });
    };
    let delivered = finished.clone();
    center.getDeliveredNotificationsWithCompletionHandler(&RcBlock::new(
        move |notifications: NonNull<NSArray<UNNotification>>| {
            autoreleasepool(|_| {
                delivered(
                    unsafe { notifications.as_ref() }
                        .iter()
                        .map(|n| n.request().identifier().to_string())
                        .collect(),
                )
            });
        },
    ));
    center.getPendingNotificationRequestsWithCompletionHandler(&RcBlock::new(
        move |requests: NonNull<NSArray<UNNotificationRequest>>| {
            autoreleasepool(|_| {
                finished(
                    unsafe { requests.as_ref() }
                        .iter()
                        .map(|n| n.identifier().to_string())
                        .collect(),
                )
            });
        },
    ));
}
fn notification_category(notification: &Notification) -> Retained<UNNotificationCategory> {
    let category = notification.identifier();
    let actions: Vec<_> = notification
        .actions
        .iter()
        .filter(|(id, _)| id != "default")
        .map(|(id, title)| {
            UNNotificationAction::actionWithIdentifier_title_options(
                &NSString::from_str(id),
                &NSString::from_str(title),
                UNNotificationActionOptions::Foreground,
            )
        })
        .collect();
    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str(&category),
        &NSArray::from_slice(&actions.iter().map(|a| &**a).collect::<Vec<_>>()),
        &NSArray::new(),
        UNNotificationCategoryOptions::CustomDismissAction,
    )
}
fn refresh_categories() {
    main_thread();
    let categories = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| {
                crate::ui::notice_lifecycle::live_values(
                    s.notifications
                        .values()
                        .map(|n| (&*n.life, notification_category(&n.notification))),
                )
            })
            .unwrap_or_default()
    });
    UNUserNotificationCenter::currentNotificationCenter().setNotificationCategories(
        &NSSet::from_slice(&categories.iter().map(|c| &**c).collect::<Vec<_>>()),
    );
}
fn retire_notice(id: u64) {
    let notice = STATE.with(|s| {
        s.borrow_mut()
            .as_mut()
            .and_then(|s| s.notifications.remove(&id))
    });
    if let Some(notice) = notice {
        notice.life.cancel();
        remove_native(&notice.notification.identifier());
        refresh_categories();
    }
}
struct Native {
    status: Retained<AnyObject>,
    target: Retained<MenuTarget>,
    _delegate: Retained<NotificationDelegate>,
    menu: Retained<AnyObject>,
}
unsafe fn new_menu(title: &str) -> Retained<AnyObject> {
    unsafe {
        let menu: Allocated<AnyObject> = msg_send![class!(NSMenu), alloc];
        let menu: Retained<AnyObject> = msg_send![menu, initWithTitle: &*NSString::from_str(title)];
        let _: () = msg_send![&*menu, setAutoenablesItems: Bool::NO];
        menu
    }
}
unsafe fn item(
    title: &str,
    action: Option<objc2::runtime::Sel>,
    target: &MenuTarget,
) -> Retained<AnyObject> {
    unsafe {
        let item: Allocated<AnyObject> = msg_send![class!(NSMenuItem), alloc];
        let item: Retained<AnyObject> = msg_send![item, initWithTitle: &*NSString::from_str(title), action: action, keyEquivalent: &*NSString::from_str("")];
        let _: () = msg_send![&*item, setTarget: target];
        item
    }
}
impl State {
    unsafe fn add(
        &mut self,
        menu: &AnyObject,
        title: &str,
        action: Option<MenuAction>,
        checked: Option<bool>,
        enabled: bool,
    ) -> Retained<AnyObject> {
        unsafe {
            let selector = action.as_ref().map(|_| sel!(invoke:));
            let item = item(title, selector, &self.native.target);
            if let Some(action) = action
                && let Ok(payload) = serde_json::to_string(&action)
            {
                let _: () = msg_send![&*item,setRepresentedObject:&*NSString::from_str(&payload)];
            }
            if let Some(checked) = checked {
                let _: () = msg_send![&*item, setState: if checked {1isize} else {0isize}];
            }
            let _: () = msg_send![&*item, setEnabled: Bool::new(enabled)];
            let _: () = msg_send![menu, addItem: &*item];
            item
        }
    }
    unsafe fn separator(menu: &AnyObject) {
        unsafe {
            let separator: Retained<AnyObject> = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![menu, addItem: &*separator];
        }
    }
    fn add_history(&mut self, menu: &AnyObject) {
        if !self.config.clipboard_history {
            return;
        }
        unsafe {
            let submenu = new_menu("Clipboard History");
            let entries: Vec<_> = self
                .snapshot
                .clipboard_history
                .iter()
                .take(10)
                .cloned()
                .collect();
            if entries.is_empty() {
                self.add(&submenu, "Nothing copied yet", None, None, false);
            } else {
                for entry in entries {
                    let item = self.add(
                        &submenu,
                        &crate::ui::tray_model::history_label(&entry, false),
                        Some(command(
                            serde_json::json!({"command":"copy_clipboard_history","id":entry.id}),
                        )),
                        None,
                        true,
                    );
                    if let Some(path) = entry.thumbnail {
                        let allocated: Allocated<AnyObject> = msg_send![class!(NSImage), alloc];
                        let image: Option<Retained<AnyObject>> = msg_send![allocated,initWithContentsOfFile:&*NSString::from_str(&path.to_string_lossy())];
                        if let Some(image) = image {
                            let _: () = msg_send![&*image,setSize:objc2_foundation::NSSize {width:16.0,height:16.0}];
                            let _: () = msg_send![&*item,setImage:&*image];
                        }
                    }
                }
                Self::separator(&submenu);
                self.add(
                    &submenu,
                    "Show All",
                    Some(command(
                        serde_json::json!({"command":"show_clipboard_history"}),
                    )),
                    None,
                    true,
                );
            }
            let parent = self.add(menu, "Clipboard History", None, None, true);
            let _: () = msg_send![&*parent,setSubmenu:&*submenu];
        }
    }
    fn rebuild(&mut self) {
        main_thread();
        unsafe {
            if self.tracking {
                return;
            }
            let menu = new_menu("Pneu");
            let transfers = self
                .snapshot
                .transfers
                .iter()
                .map(crate::ui::window::contract::Transfer::from)
                .collect::<Vec<_>>();
            let peers = crate::ui::window::peers(&self.snapshot, &transfers);
            let status = crate::ui::tray_model::status(&self.snapshot, &peers, &transfers);
            self.add(&menu, &status, None, None, false);
            Self::separator(&menu);
            if peers.is_empty() {
                self.add_history(&menu);
            }
            if !peers.is_empty() {
                let submenu = new_menu("Send Files To");
                let all = crate::file_recipients::resolve(
                    &self.snapshot.send_targets,
                    crate::file_recipients::Selection::ConnectedOnly,
                )
                .map(|r| r.recipients)
                .unwrap_or_default();
                self.add(
                    &submenu,
                    "All Connected Devices…",
                    Some(MenuAction::Send {
                        recipients: all,
                        targeted: false,
                        clipboard: false,
                    }),
                    None,
                    true,
                );
                Self::separator(&submenu);
                for peer in &peers {
                    let Ok(id) = pneu_proto::Id::new(&peer.id) else {
                        continue;
                    };
                    let captured = crate::file_recipients::resolve(
                        &self.snapshot.send_targets,
                        crate::file_recipients::Selection::Explicit(&[id]),
                    )
                    .map(|r| r.recipients)
                    .unwrap_or_default();
                    let label = crate::ui::window::display_peer_text(&peer.name);
                    let label: String = label.chars().take(39).collect();
                    self.add(
                        &submenu,
                        &format!("{label}…"),
                        Some(MenuAction::Send {
                            recipients: captured,
                            targeted: true,
                            clipboard: false,
                        }),
                        None,
                        true,
                    );
                }
                if let [one] = peers.as_slice() {
                    if let Ok(id) = pneu_proto::Id::new(&one.id) {
                        let captured = crate::file_recipients::resolve(
                            &self.snapshot.send_targets,
                            crate::file_recipients::Selection::Explicit(&[id]),
                        )
                        .map(|r| r.recipients)
                        .unwrap_or_default();
                        self.add(
                            &menu,
                            "Send Files…",
                            Some(MenuAction::Send {
                                recipients: captured,
                                targeted: true,
                                clipboard: false,
                            }),
                            None,
                            true,
                        );
                    }
                } else {
                    let parent = self.add(&menu, "Send Files To", None, None, true);
                    let _: () = msg_send![&*parent, setSubmenu: &*submenu];
                }
                if self.config.clipboard_mode == crate::config::ClipboardMode::Manual {
                    self.add(
                        &menu,
                        "Send Clipboard",
                        Some(MenuAction::Send {
                            recipients: crate::ui::captured_authorities(&self.snapshot),
                            targeted: false,
                            clipboard: true,
                        }),
                        None,
                        true,
                    );
                }
                self.add_history(&menu);
                self.add(&menu, "Open Folder", Some(MenuAction::Folder), None, true);
                Self::separator(&menu);
                let clipboard = new_menu("Clipboard");
                let current = serde_json::to_value(self.config.clipboard_mode).unwrap_or_default();
                for (label, mode) in [
                    ("Share Automatically", "automatic"),
                    ("Share Manually", "manual"),
                    ("Off", "off"),
                ] {
                    self.add(
                        &clipboard,
                        label,
                        Some(setting("clipboard_mode", mode.into())),
                        Some(current.as_str() == Some(mode)),
                        true,
                    );
                }
                Self::separator(&clipboard);
                self.add(
                    &clipboard,
                    "Include Images",
                    Some(setting(
                        "clipboard_include_images",
                        (!self.config.clipboard_include_images).into(),
                    )),
                    Some(self.config.clipboard_include_images),
                    current.as_str() != Some("off"),
                );
                let parent = self.add(&menu, "Clipboard", None, None, true);
                let _: () = msg_send![&*parent, setSubmenu: &*clipboard];
                let arrivals = new_menu("When Files Arrive");
                for toggle in [
                    crate::ui::Toggle::Save,
                    crate::ui::Toggle::Notifications,
                    crate::ui::Toggle::CopyImages,
                    crate::ui::Toggle::Viewer,
                ] {
                    let checked = toggle.checked(&self.config);
                    let label = if matches!(toggle, crate::ui::Toggle::Viewer) {
                        "Open in Preview"
                    } else {
                        toggle.label()
                    };
                    let key = match toggle {
                        crate::ui::Toggle::Save => "save_to_folder",
                        crate::ui::Toggle::Notifications => "notification_preview",
                        crate::ui::Toggle::CopyImages => "copy_image_to_clipboard",
                        crate::ui::Toggle::Viewer => "open_in_viewer",
                        _ => unreachable!(),
                    };
                    let value = (!checked).into();
                    self.add(
                        &arrivals,
                        label,
                        Some(setting(key, value)),
                        Some(checked),
                        true,
                    );
                }
                let parent = self.add(&menu, "When Files Arrive", None, None, true);
                let _: () = msg_send![&*parent, setSubmenu: &*arrivals];
                Self::separator(&menu);
            }
            self.add(
                &menu,
                "Add Device…",
                Some(command(serde_json::json!({"command":"add_device"}))),
                None,
                true,
            );
            self.add(
                &menu,
                "Open Pneu",
                Some(command(
                    serde_json::json!({"command":"show_window","choose_folder":false}),
                )),
                None,
                true,
            );
            let settings = self.add(&menu, "Settings…", Some(MenuAction::Settings), None, true);
            let _: () = msg_send![&*settings, setKeyEquivalent: &*NSString::from_str(",")];
            let quit = item("Quit Pneu", Some(sel!(quit:)), &self.native.target);
            let _: () = msg_send![&*quit, setKeyEquivalent: &*NSString::from_str("q")];
            let _: () = msg_send![&*menu, addItem: &*quit];
            let _: () = msg_send![&*menu, setDelegate: &*self.native.target];
            let _: () = msg_send![&*self.native.status, setMenu: &*menu];
            self.native.menu = menu;
            let button: Retained<AnyObject> = msg_send![&*self.native.status, button];
            let name = crate::ui::tray_model::icon(&self.snapshot, &peers);
            let name = if name == "pneu-tray-receiving" {
                "pneu-tray-connected"
            } else {
                name
            };
            let resources = objc2_foundation::NSBundle::mainBundle().resourcePath();
            let image: Option<Retained<AnyObject>> = resources.and_then(|r| {
            let directory=PathBuf::from(r.to_string()).join("menubar");
            let retina=directory.join(format!("{name}@2x.png"));
            let path=if retina.is_file(){retina}else{directory.join(format!("{name}.png"))};
            let allocated: Allocated<AnyObject> = msg_send![class!(NSImage), alloc];
            msg_send![allocated, initWithContentsOfFile: &*NSString::from_str(&path.to_string_lossy())]
        }).or_else(|| msg_send![class!(NSImage), imageWithSystemSymbolName: &*NSString::from_str(if name == "pneu-tray-error" {"exclamationmark.circle"} else if name == "pneu-tray-paused" {"pause.circle"} else if name == "pneu-tray-disconnected" {"link.badge.plus"} else {"link"}), accessibilityDescription: &*NSString::from_str("Pneu")]);
            if let Some(image) = image {
                let _: () =
                    msg_send![&*image,setSize:objc2_foundation::NSSize {width:18.0,height:18.0}];
                let _: () = msg_send![&*image, setTemplate: Bool::YES];
                let _: () = msg_send![&*button, setImage: &*image];
            }
            let lines = crate::ui::tray_model::tooltip_lines(&self.snapshot, &peers, &transfers);
            let _: () = msg_send![&*button, setToolTip: &*NSString::from_str(&lines.join("\n"))];
        }
    }
}
fn command(value: serde_json::Value) -> MenuAction {
    MenuAction::Command(value)
}
fn setting(key: &str, value: serde_json::Value) -> MenuAction {
    command(serde_json::json!({"command":"set_setting","key":key,"value":value}))
}

pub(crate) fn install(
    window: &PneuWindow,
    cache_root: PathBuf,
    callback: impl Fn(MenuAction) + 'static,
) -> Result<()> {
    unsafe {
        let cache = crate::ui::viewer_cache::ViewerCache::new(
            cache_root,
            std::env::var_os("PNEU_UI_VIEW_CACHE").map(PathBuf::from),
        );
        let mtm = MainThreadMarker::new().context("UI must run on the main thread")?;
        refresh_preferences();
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let _: Bool = msg_send![&*app, setActivationPolicy: 1isize];
        let target: Retained<MenuTarget> = msg_send![MenuTarget::alloc(mtm), init];
        let delegate: Retained<NotificationDelegate> =
            msg_send![NotificationDelegate::alloc(), init];
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let main_menu = new_menu("Pneu");
        let application_menu = new_menu("Pneu");
        let settings = item("Settings…", Some(sel!(invoke:)), &target);
        let payload = serde_json::to_string(&MenuAction::Settings)?;
        let _: () = msg_send![&*settings,setRepresentedObject:&*NSString::from_str(&payload)];
        let _: () = msg_send![&*settings,setKeyEquivalent:&*NSString::from_str(",")];
        let _: () = msg_send![&*application_menu,addItem:&*settings];
        let quit = item("Quit Pneu", Some(sel!(quit:)), &target);
        let _: () = msg_send![&*quit,setKeyEquivalent:&*NSString::from_str("q")];
        let _: () = msg_send![&*application_menu,addItem:&*quit];
        let application = item("Pneu", None, &target);
        let _: () = msg_send![&*application,setSubmenu:&*application_menu];
        let _: () = msg_send![&*main_menu,addItem:&*application];
        let window_menu = new_menu("Window");
        let close = item("Close Window", Some(sel!(closeWindow:)), &target);
        let _: () = msg_send![&*close,setKeyEquivalent:&*NSString::from_str("w")];
        let _: () = msg_send![&*window_menu,addItem:&*close];
        let window_item = item("Window", None, &target);
        let _: () = msg_send![&*window_item,setSubmenu:&*window_menu];
        let _: () = msg_send![&*main_menu,addItem:&*window_item];
        let _: () = msg_send![&*app,setMainMenu:&*main_menu];

        let bar: Retained<AnyObject> = msg_send![class!(NSStatusBar), systemStatusBar];
        let status: Retained<AnyObject> = msg_send![&*bar, statusItemWithLength: -1.0f64];
        STATE.with(|s| {
            let mut state = State {
                native: Native {
                    status,
                    target,
                    _delegate: delegate,
                    menu: new_menu("Pneu"),
                },
                window: window.as_weak(),
                callback: Rc::new(callback),
                snapshot: Snapshot::default(),
                config: Config::default(),
                tracking: false,
                child: None,
                panels: HashMap::new(),
                notifications: HashMap::new(),
                next_panel: 1,
                materials: crate::ui::viewer_cache::Materials::default(),
                requests: HashMap::new(),
                jobs: Vec::new(),
                cache,
                login_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                login_next: std::time::Instant::now(),
                login_thread: None,
            };
            state.rebuild();
            *s.borrow_mut() = Some(state);
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &RcBlock::new(|granted: Bool, error: *mut NSError| {
                    let code =
                        autoreleasepool(|_| error.as_ref().map(|error| error.code()).unwrap_or(0));
                    eprintln!(
                        "pneud: notification authorization granted={} error_code={code}",
                        granted.as_bool()
                    );
                }),
            );
        let child = Child::start()?;
        STATE.with(|s| {
            s.borrow_mut().as_mut().expect("native installed").child = Some(child);
        });
        Ok(())
    }
}
pub(crate) fn accepts_drop() -> bool {
    STATE.with(|s| s.borrow().as_ref().is_some_and(|s| s.panels.is_empty()))
}
pub(crate) fn ready() -> Result<()> {
    STATE.with(|s| {
        s.borrow_mut()
            .as_mut()
            .and_then(|s| s.child.as_mut())
            .context("UI channel unavailable")?
            .ready()
    })
}
pub(crate) fn update(snapshot: Snapshot, config: Config) {
    if !installed() {
        return;
    }
    refresh_preferences();
    poll_login();
    STATE.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.snapshot = snapshot;
            s.config = config;
            s.rebuild();
        }
    });
}
pub(crate) fn activate(window: &PneuWindow) {
    if !installed() {
        return;
    }
    unsafe {
        let _ = window.show();
        window.window().request_redraw();
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![&*app, activateIgnoringOtherApps: Bool::YES];
        use slint::winit_030::WinitWindowAccessor;
        window.window().with_winit_window(|w| {
            use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(handle) = w.window_handle()
                && let RawWindowHandle::AppKit(handle) = handle.as_raw()
            {
                let view = &*handle.ns_view.as_ptr().cast::<AnyObject>();
                let native: Option<Retained<AnyObject>> = msg_send![view, window];
                if let Some(native) = native {
                    let frame: objc2_foundation::NSRect = msg_send![&*native, frame];
                    let screens: Retained<NSArray<AnyObject>> =
                        msg_send![class!(NSScreen), screens];
                    let visible = screens.iter().any(|screen| {
                        let bounds: objc2_foundation::NSRect = msg_send![&*screen, visibleFrame];
                        frame.origin.x + frame.size.width > bounds.origin.x + 32.0
                            && frame.origin.x < bounds.origin.x + bounds.size.width - 32.0
                            && frame.origin.y + frame.size.height > bounds.origin.y + 32.0
                            && frame.origin.y < bounds.origin.y + bounds.size.height - 32.0
                    });
                    if !visible {
                        let _: () = msg_send![&*native, center];
                    }
                }
            }
            w.set_minimized(false);
            w.focus_window();
        });
    }
}
/// Slint's visibility flag alone does not cover Hide Pneu or minimization.
pub(crate) fn camera_window_visible(window: &PneuWindow) -> bool {
    if !installed() || !window.window().is_visible() {
        return false;
    }
    autoreleasepool(|_| unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let hidden: Bool = msg_send![&*app, isHidden];
        if hidden.as_bool() {
            return false;
        }
        use slint::winit_030::{
            WinitWindowAccessor,
            winit::raw_window_handle::{HasWindowHandle, RawWindowHandle},
        };
        window
            .window()
            .with_winit_window(|w| {
                let Ok(handle) = w.window_handle() else {
                    return false;
                };
                let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
                    return false;
                };
                let view = &*handle.ns_view.as_ptr().cast::<AnyObject>();
                let native: Option<Retained<AnyObject>> = msg_send![view, window];
                native.is_some_and(|native| {
                    let visible: Bool = msg_send![&*native, isVisible];
                    let minimized: Bool = msg_send![&*native, isMiniaturized];
                    visible.as_bool() && !minimized.as_bool()
                })
            })
            .unwrap_or(false)
    })
}
pub(crate) fn twelve_hour_clock() -> bool {
    TWELVE_HOUR_CLOCK.load(std::sync::atomic::Ordering::Relaxed)
}

fn read_twelve_hour_clock() -> bool {
    main_thread();
    autoreleasepool(|_| unsafe {
        let defaults: Retained<AnyObject> = msg_send![class!(NSUserDefaults), standardUserDefaults];
        for (key, twelve) in [
            ("AppleICUForce24HourTime", false),
            ("AppleICUForce12HourTime", true),
        ] {
            let value: Option<Retained<AnyObject>> =
                msg_send![&*defaults,objectForKey:&*NSString::from_str(key)];
            if let Some(value) = value {
                let yes: Bool = msg_send![&*value, boolValue];
                if yes.as_bool() {
                    return twelve;
                }
            }
        }
        let locale = objc2_foundation::NSLocale::currentLocale();
        objc2_foundation::NSDateFormatter::dateFormatFromTemplate_options_locale(
            &NSString::from_str("j"),
            0,
            Some(&locale),
        )
        .is_some_and(|s| s.to_string().contains('a'))
    })
}
pub(crate) fn reduced_motion() -> bool {
    REDUCED_MOTION.load(std::sync::atomic::Ordering::Relaxed)
}

fn read_reduced_motion() -> bool {
    main_thread();
    autoreleasepool(|_| unsafe {
        let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
        let reduced: Bool = msg_send![&*workspace, accessibilityDisplayShouldReduceMotion];
        reduced.as_bool()
    })
}

pub(crate) fn request(id: u64, request: Request, descriptor: Option<OwnedFd>, guard: RequestGuard) {
    if !guard.life().active() {
        guard.finish(Err(anyhow::anyhow!("UI request cancelled or expired")));
        return;
    }
    if let Err(error) = require_installed() {
        guard.finish(Err(error));
        return;
    }
    match request {
        Request::Notify(notification) if descriptor.is_none() => post(id, notification, guard),
        Request::ClearStale { .. } if descriptor.is_none() => clear_stale_notices(guard),
        Request::Withdraw {
            id: notice_id,
            namespace,
            revision,
        } if descriptor.is_none() => {
            let ids = STATE.with(|s| {
                s.borrow()
                    .as_ref()
                    .map(|s| {
                        s.notifications
                            .iter()
                            .filter(|(_, n)| {
                                n.notification.id == notice_id
                                    && n.notification.namespace == namespace
                                    && n.notification.revision == revision
                            })
                            .map(|(id, _)| *id)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            });
            let result = guard.run(|| {
                for id in ids {
                    retire_notice(id);
                }
                remove_native(&format!("pneu-{namespace}-{revision}-{notice_id}"));
                Ok(())
            });
            guard.finish(result);
        }
        Request::Open { name, preview } => {
            let Some(descriptor) = descriptor else {
                guard.finish(Err(anyhow::anyhow!("missing received descriptor")));
                return;
            };
            let descriptor = std::fs::File::from(descriptor);
            let reused = STATE.with(|s| {
                s.borrow_mut()
                    .as_mut()
                    .and_then(|s| s.materials.find(&descriptor, &name))
            });
            if let Some(path) = reused {
                guard.finish(open_viewer_path(&path, preview, &guard));
                return;
            }
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cache = STATE.with(|s| {
                let mut s = s.borrow_mut();
                let s = s.as_mut().context("native state")?;
                ensure!(
                    s.requests.len() < crate::ui::portal::Interactions::LIMIT,
                    "viewer capacity exhausted"
                );
                let path = s.cache.path()?;
                s.requests.insert(id, cancelled.clone());
                Ok(path)
            });
            let cache = match cache {
                Ok(cache) => cache,
                Err(error) => {
                    guard.finish(Err(error));
                    return;
                }
            };
            let job = std::thread::spawn(move || {
                let result = materialize(&cache, descriptor, &name, &cancelled);
                let _ = slint::invoke_from_event_loop(move || {
                    STATE.with(|s| {
                        if let Some(s) = s.borrow_mut().as_mut() {
                            s.requests.remove(&id);
                        }
                    });
                    let result = result.and_then(|material| {
                        ensure!(
                            !cancelled.load(std::sync::atomic::Ordering::Acquire),
                            "viewer request cancelled"
                        );
                        open_viewer_path(&material.path, preview, &guard)?;
                        STATE.with(|s| {
                            if let Some(s) = s.borrow_mut().as_mut() {
                                s.materials.push(material);
                            }
                        });
                        Ok(())
                    });
                    guard.finish(result);
                });
            });
            STATE.with(|s| {
                if let Some(s) = s.borrow_mut().as_mut() {
                    s.jobs.retain(|job| !job.is_finished());
                    s.jobs.push(job);
                }
            });
        }
        Request::Reveal { paths } if descriptor.is_none() => guard.finish(reveal(&paths, &guard)),
        Request::OpenUrl(url) if descriptor.is_none() => {
            guard.finish(open_url_guarded(&url, guard.life()))
        }
        Request::Test(action)
            if descriptor.is_none() && std::env::var_os("PNEU_UI_TEST").is_some() =>
        {
            if matches!(
                action.as_str(),
                "assert-notification-delivered" | "assert-notifications-empty"
            ) {
                let empty = action == "assert-notifications-empty";
                UNUserNotificationCenter::currentNotificationCenter()
                    .getDeliveredNotificationsWithCompletionHandler(&RcBlock::new(
                        move |notices: NonNull<NSArray<UNNotification>>| {
                            autoreleasepool(|_| {
                                let count = unsafe { notices.as_ref() }
                                    .iter()
                                    .filter(|n| {
                                        n.request().identifier().to_string().starts_with("pneu-")
                                    })
                                    .count();
                                guard.finish(if (count == 0) == empty {
                                    Ok(())
                                } else {
                                    Err(anyhow::anyhow!(
                                        "unexpected delivered notification count: {count}"
                                    ))
                                });
                            });
                        },
                    ));
                return;
            }
            if action == "assert-retry-category" || action == "assert-categories-empty" {
                let empty = action == "assert-categories-empty";
                UNUserNotificationCenter::currentNotificationCenter()
                    .getNotificationCategoriesWithCompletionHandler(&RcBlock::new(
                        move |categories: NonNull<NSSet<UNNotificationCategory>>| {
                            autoreleasepool(|_| {
                                let categories = unsafe { categories.as_ref() };
                                let has_retry = categories.iter().any(|c| {
                                    c.actions()
                                        .iter()
                                        .any(|a| a.identifier().to_string() == "retry")
                                });
                                guard.finish(
                                    if if empty {
                                        categories.is_empty()
                                    } else {
                                        has_retry
                                    } {
                                        Ok(())
                                    } else {
                                        Err(anyhow::anyhow!(
                                            "unexpected native notification categories"
                                        ))
                                    },
                                );
                            });
                        },
                    ));
                return;
            }
            if action == "stall" {
                // Opt-in acceptance only: imitate an unresponsive main thread.
                std::thread::sleep(std::time::Duration::from_secs(30));
                return;
            }
            let result = guard.run(|| test_control(&action));
            guard.finish(result);
        }
        _ => guard.finish(Err(anyhow::anyhow!("invalid UI request descriptor"))),
    }
}
fn plain(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}
fn post(id: u64, notification: Notification, guard: RequestGuard) {
    let life = guard.life().clone();
    // Enforce capacity before starting any deferred settings callback. Replaced
    // requests have distinct native identifiers, so late callbacks cannot remove
    // or resurrect the replacement.
    let previous = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| {
                s.notifications
                    .iter()
                    .filter(|(_, n)| {
                        n.notification.id == notification.id
                            && n.notification.namespace == notification.namespace
                    })
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    for id in previous {
        retire_notice(id);
    }
    let oldest = STATE.with(|s| {
        s.borrow().as_ref().and_then(|s| {
            (s.notifications.len() >= 128)
                .then(|| s.notifications.keys().min().copied())
                .flatten()
        })
    });
    if let Some(id) = oldest {
        retire_notice(id);
    }
    let category_id = notification.identifier();
    let record = Arc::new(NativeNotice {
        life,
        notification: notification.clone(),
    });
    STATE.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.notifications.insert(id, record.clone());
        }
    });
    if let Some(lifetime) = notification.lifetime() {
        slint::Timer::single_shot(lifetime, move || retire_notice(id));
    }
    refresh_categories();
    let center = UNUserNotificationCenter::currentNotificationCenter();
    center.getNotificationSettingsWithCompletionHandler(&RcBlock::new(
        move |settings: NonNull<UNNotificationSettings>| {
            autoreleasepool(|_| {
                let status = unsafe { settings.as_ref() }.authorizationStatus();
                let allowed = matches!(status, UNAuthorizationStatus::Authorized | UNAuthorizationStatus::Provisional | UNAuthorizationStatus::Ephemeral);
                if !allowed || !record.life.active() {
                    let completion = guard.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        retire_notice(id);
                        completion.finish(Err(anyhow::anyhow!("notification cancelled, expired or disabled")));
                    });
                    return;
                }
                let content = UNMutableNotificationContent::new();
                content.setTitle(&NSString::from_str(&notification.summary));
                content.setBody(&NSString::from_str(&plain(&notification.body)));
                content.setCategoryIdentifier(&NSString::from_str(&category_id));
                let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                    &NSString::from_str(&notification.identifier()), &content, None,
                );
                let completion = guard.clone();
                let completed = record.clone();
                if !guard.submit(|| {
                    UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(
                        &request, Some(&RcBlock::new(move |error: *mut NSError| {
                            autoreleasepool(|_| {
                                let succeeded = error.is_null();
                                let completed = completed.clone();
                                let completion = completion.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    let accepted = succeeded && completed.life.acknowledge();
                                    if !accepted {
                                        retire_notice(id);
                                        // Cancellation may have removed the record
                                        // before native submission finished.
                                        remove_native(&completed.notification.identifier());
                                    }
                                    completion.finish(if accepted { Ok(()) } else { Err(anyhow::anyhow!("notification cancelled, expired or submission failed")) });
                                });
                            });
                        })),
                    );
                }) {
                    let _ = slint::invoke_from_event_loop(move || retire_notice(id));
                }
            });
        },
    ));
}

fn open_viewer_path(path: &Path, preview: bool, guard: &RequestGuard) -> Result<()> {
    anyhow::ensure!(guard.life().active(), "viewer request cancelled or expired");
    require_installed()?;
    if !preview {
        return open_path_guarded(path, guard.life());
    }
    unsafe {
        let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
        let accepted: Bool = guard.run(|| Ok(msg_send![&*workspace,openFile:&*NSString::from_str(&path.to_string_lossy()),withApplication:&*NSString::from_str("/System/Applications/Preview.app")]))?;
        ensure!(accepted.as_bool(), "Preview could not open the image");
    }
    Ok(())
}
pub(crate) fn open_path(path: &Path) -> Result<()> {
    open_path_guarded(path, &Lifecycle::new(None))
}
fn open_path_guarded(path: &Path, life: &Lifecycle) -> Result<()> {
    require_installed()?;
    autoreleasepool(|_| {
        let path = path.to_owned();
        unsafe {
            let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let accepted: Bool = life.run(|| Ok(msg_send![&*workspace, openURL: &*url]))?;
            ensure!(accepted.as_bool(), "macOS could not open the file");
        }
        Ok(())
    })
}
fn reveal(paths: &[PathBuf], guard: &RequestGuard) -> Result<()> {
    require_installed()?;
    unsafe {
        let urls: Vec<_> = paths
            .iter()
            .map(|p| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy())))
            .collect();
        let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
        guard.run(|| {
            let _: () = msg_send![&*workspace, activateFileViewerSelectingURLs: &*NSArray::from_slice(&urls.iter().map(|v|&**v).collect::<Vec<_>>())];
            Ok(())
        })?;
        Ok(())
    }
}
pub(crate) fn open_url(url: &str) -> Result<()> {
    open_url_guarded(url, &Lifecycle::new(None))
}
fn open_url_guarded(url: &str, life: &Lifecycle) -> Result<()> {
    require_installed()?;
    autoreleasepool(|_| unsafe {
        let parsed = url::Url::parse(url)?;
        ensure!(
            matches!(
                parsed.scheme(),
                "http" | "https" | "x-apple.systempreferences"
            ),
            "unsupported URL scheme"
        );
        let url = NSURL::URLWithString(&NSString::from_str(url)).context("invalid URL")?;
        let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
        let accepted: Bool = life.run(|| Ok(msg_send![&*workspace, openURL: &*url]))?;
        ensure!(accepted.as_bool(), "macOS could not open the URL");
        Ok(())
    })
}

pub(crate) async fn choose(
    title: &str,
    directory: bool,
    current: Option<&Path>,
) -> Result<Option<Vec<PathBuf>>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let title = title.to_owned();
    let current = current.map(Path::to_owned);
    let id = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let shared_id = id.clone();
    struct Cancel(Arc<std::sync::atomic::AtomicU64>);
    impl Drop for Cancel {
        fn drop(&mut self) {
            let id = self.0.swap(u64::MAX, std::sync::atomic::Ordering::AcqRel);
            if id != 0 && id != u64::MAX {
                let _ = slint::invoke_from_event_loop(move || cancel(id));
            }
        }
    }
    let _cancel = Cancel(id);
    let life = Arc::new(Lifecycle::new(None));
    let _life_cancel = CancelOnDrop(life.clone());
    slint::invoke_from_event_loop(move || unsafe {
        if sender.is_closed() {
            return;
        }
        if let Err(error) = require_installed() {
            let _ = sender.send(Err(error));
            return;
        }
        let panel: Retained<AnyObject> = msg_send![class!(NSOpenPanel), openPanel];
        let _: () = msg_send![&*panel, setTitle: &*NSString::from_str(&title)];
        let _: () = msg_send![&*panel, setCanChooseDirectories: Bool::new(directory)];
        let _: () = msg_send![&*panel, setCanChooseFiles: Bool::new(!directory)];
        let _: () = msg_send![&*panel, setAllowsMultipleSelection: Bool::new(!directory)];
        if let Some(current) = current {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&current.to_string_lossy()));
            let _: () = msg_send![&*panel, setDirectoryURL: &*url];
        }
        let id = STATE.with(|s| {
            let mut s = s.borrow_mut();
            let s = s.as_mut().expect("native installed");
            let id = s.next_panel;
            s.next_panel += 1;
            s.panels.insert(id, panel.clone());
            id
        });
        if shared_id
            .compare_exchange(
                0,
                id,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            STATE.with(|s| {
                if let Some(s) = s.borrow_mut().as_mut() {
                    s.panels.remove(&id);
                }
            });
            return;
        }
        let sender = Arc::new(Mutex::new(Some(sender)));
        let completed = sender.clone();
        let completion = RcBlock::new(move |result: isize| {
            let panel = STATE.with(|s| s.borrow_mut().as_mut().and_then(|s| s.panels.remove(&id)));
            let result = if result == 1 {
                panel.context("picker cancelled").and_then(|panel| {
                    let urls: Retained<NSArray<NSURL>> = msg_send![&*panel, URLs];
                    let paths: Result<Vec<PathBuf>> = urls
                        .iter()
                        .map(|u| {
                            u.path()
                                .map(|p| PathBuf::from(p.to_string()))
                                .context("picker returned a nonlocal path")
                        })
                        .collect();
                    paths.map(Some)
                })
            } else {
                Ok(None)
            };
            if let Some(sender) = completed.lock().expect("picker response").take() {
                let _ = sender.send(result);
            }
        });
        let result = life.run(|| {
            let _: () = msg_send![&*panel, beginWithCompletionHandler: &*completion];
            Ok(())
        });
        if let Err(error) = result {
            STATE.with(|s| {
                if let Some(s) = s.borrow_mut().as_mut() {
                    s.panels.remove(&id);
                }
            });
            if let Some(sender) = sender.lock().expect("picker response").take() {
                let _ = sender.send(Err(error));
            }
        }
    })?;
    receiver.await.context("picker cancelled")?
}
pub(crate) fn cancel_request(id: u64) {
    STATE.with(|s| {
        if let Some(flag) = s.borrow().as_ref().and_then(|s| s.requests.get(&id)) {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
    });
    retire_notice(id);
}
pub(crate) fn cancel(id: u64) {
    let panel = STATE.with(|s| s.borrow_mut().as_mut().and_then(|s| s.panels.remove(&id)));
    if let Some(panel) = panel {
        main_thread();
        unsafe {
            let _: () = msg_send![&*panel, cancel: Option::<&AnyObject>::None];
        }
    }
}
pub(crate) fn cleanup() {
    super::camera::cleanup();
    super::drag::cleanup();
    let state = STATE.with(|s| s.borrow_mut().take());
    if let Some(mut state) = state {
        main_thread();
        let identifiers = state
            .notifications
            .values()
            .map(|notice| {
                notice.life.cancel();
                NSString::from_str(&notice.notification.identifier())
            })
            .collect::<Vec<_>>();
        let identifiers =
            NSArray::from_slice(&identifiers.iter().map(|id| &**id).collect::<Vec<_>>());
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.removePendingNotificationRequestsWithIdentifiers(&identifiers);
        center.removeDeliveredNotificationsWithIdentifiers(&identifiers);
        center.setNotificationCategories(&NSSet::new());
        for cancelled in state.requests.values() {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
        }
        for job in state.jobs.drain(..) {
            let _ = job.join();
        }
        for panel in state.panels.values() {
            unsafe {
                let _: () = msg_send![&**panel, cancel: Option::<&AnyObject>::None];
            }
        }
        drop(state.child);
        if state.login_thread.as_ref().is_some_and(|t| t.is_finished()) {
            let _ = state
                .login_thread
                .take()
                .expect("finished login worker")
                .join();
        }
        unsafe {
            let bar: Retained<AnyObject> = msg_send![class!(NSStatusBar), systemStatusBar];
            let _: () = msg_send![&*bar, removeStatusItem: &*state.native.status];
        }
        drop(state.materials);
    }
}

/// Recent is navigation to the current regular file at its recorded path.
/// The caller rejects symlinks component by component. NSWorkspace opens that
/// path directly; only notification descriptor-authority opens materialize bytes.
pub(crate) async fn open_received_local(file: crate::actions::ReceivedFile) -> Result<()> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let life = Arc::new(Lifecycle::new(None));
    let _cancel = CancelOnDrop(life.clone());
    slint::invoke_from_event_loop(move || {
        let _ = sender.send(open_path_guarded(file.path(), &life));
    })?;
    receiver.await?
}

fn open_recent(file: crate::actions::ReceivedFile) -> Result<()> {
    open_path(file.path())
}

pub(crate) fn set_login(enabled: bool) -> Result<()> {
    start_login(if enabled { "register" } else { "unregister" })
}
fn poll_login() {
    let due = STATE.with(|s| {
        s.borrow().as_ref().is_some_and(|s| {
            !s.login_busy.load(std::sync::atomic::Ordering::Acquire)
                && std::time::Instant::now() >= s.login_next
                && s.window.upgrade().is_some_and(|w| {
                    w.global::<crate::ui::window::Ui>().get_page()
                        == crate::ui::window::Page::Settings
                })
        })
    });
    if due {
        let _ = start_login("status");
    }
}
fn start_login(operation: &'static str) -> Result<()> {
    let busy = STATE.with(|s| -> Result<_> {
        let mut state = s.borrow_mut();
        let s = state.as_mut().context("native state")?;
        ensure!(
            !s.login_busy.swap(true, std::sync::atomic::Ordering::AcqRel),
            "Login items are still responding; try again shortly"
        );
        s.login_next = std::time::Instant::now() + std::time::Duration::from_secs(60);
        if let Some(window) = s.window.upgrade() {
            window
                .global::<crate::ui::window::Ui>()
                .set_login_busy(true);
        }
        if s.login_thread.as_ref().is_some_and(|t| t.is_finished()) {
            let _ = s.login_thread.take().expect("finished login worker").join();
        }
        Ok(s.login_busy.clone())
    })?;
    let result_busy = busy.clone();
    let worker = std::thread::Builder::new()
        .name("pneu-login-state".into())
        .spawn(move || {
            // ServiceManagement's synchronous XPC can wait while the Mac is locked.
            // One worker isolates it from AppKit; no duplicate queries accumulate.
            // It has no independent process and dies with the supervised UI child.
            let result = autoreleasepool(|_| crate::macos_login::perform(operation));
            let _ = slint::invoke_from_event_loop(move || {
                result_busy.store(false, std::sync::atomic::Ordering::Release);
                STATE.with(|s| {
                    let state = s.borrow();
                    let Some(s) = state.as_ref() else { return };
                    if !Arc::ptr_eq(&s.login_busy, &result_busy) {
                        return;
                    }
                    let Some(window) = s.window.upgrade() else {
                        return;
                    };
                    let ui = window.global::<crate::ui::window::Ui>();
                    ui.set_login_busy(false);
                    match result {
                        Ok(status) => {
                            use crate::macos_login::LoginItemStatus;
                            ui.set_login_enabled(matches!(
                                status,
                                LoginItemStatus::Enabled | LoginItemStatus::RequiresApproval
                            ));
                            ui.set_login_pending(status == LoginItemStatus::RequiresApproval);
                            ui.set_login_error("".into());
                        }
                        Err(error) => {
                            eprintln!("pneud: login item {operation}: {error:#}");
                            ui.set_login_error(
                                if operation == "status" {
                                    "Couldn't read login settings"
                                } else {
                                    "Couldn't change Open at login"
                                }
                                .into(),
                            );
                        }
                    }
                });
            });
        });
    match worker {
        Ok(worker) => STATE.with(|s| {
            if let Some(s) = s.borrow_mut().as_mut() {
                s.login_thread = Some(worker);
            }
        }),
        Err(error) => {
            busy.store(false, std::sync::atomic::Ordering::Release);
            STATE.with(|s| {
                if let Some(w) = s.borrow().as_ref().and_then(|s| s.window.upgrade()) {
                    w.global::<crate::ui::window::Ui>().set_login_busy(false);
                }
            });
            return Err(error.into());
        }
    }
    Ok(())
}
pub(crate) fn system_settings(pane: &str) -> Result<()> {
    open_url(match pane {
        "camera" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera",
        "notifications" => "x-apple.systempreferences:com.apple.Notifications-Settings.extension",
        "clipboard" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Pasteboard",
        "login" => "x-apple.systempreferences:com.apple.LoginItems-Settings.extension",
        _ => anyhow::bail!("unknown System Settings pane"),
    })
}

fn close_window() {
    let window = STATE.with(|s| s.borrow().as_ref().and_then(|s| s.window.upgrade()));
    if let Some(window) = window {
        window.global::<crate::ui::window::Ui>().invoke_close();
    }
}
/// Opt-in private acceptance controls. The daemon independently checks the same
/// environment gate; normal installations expose no test actions in the UI.
fn test_control(action: &str) -> Result<()> {
    main_thread();
    use slint::winit_030::{WinitWindowAccessor, winit::window::Theme};
    if let Some(path) = action.strip_prefix("recent-open:") {
        return open_recent(crate::actions::ReceivedFile::from_file(
            PathBuf::from(path),
            crate::storage::open_regular_file(Path::new(path))?,
        )?);
    }
    match action {
        "add-computer"
        | "scan-qr"
        | "scan-back"
        | "assert-camera-available"
        | "assert-camera-preview"
        | "assert-camera-denied" => {
            use crate::ui::window::{Page, ScanState, Ui};
            let window = STATE
                .with(|s| s.borrow().as_ref().and_then(|s| s.window.upgrade()))
                .context("window unavailable")?;
            let ui = window.global::<Ui>();
            match action {
                "add-computer" => window.invoke_open_add(1),
                "scan-qr" => {
                    ensure!(ui.get_camera_available(), "Scan QR Code row is unavailable");
                    window.invoke_go(Page::Scan);
                    ui.invoke_scan_qr();
                }
                "scan-back" => window.invoke_back(),
                "assert-camera-available" => {
                    ensure!(ui.get_camera_available(), "camera was not discovered")
                }
                "assert-camera-preview" => ensure!(
                    ui.get_camera_preview().size().width > 0,
                    "no camera frame displayed"
                ),
                "assert-camera-denied" => ensure!(
                    ui.get_scan_state() == ScanState::Denied,
                    "camera permission is not denied"
                ),
                _ => unreachable!(),
            }
            Ok(())
        }
        "assert-camera-stopped" => super::camera::assert_stopped(),
        "open-from-menu" => {
            let menu = STATE
                .with(|s| s.borrow().as_ref().map(|s| s.native.menu.clone()))
                .context("menu unavailable")?;
            // Dispatch the actual menu item's selector and immutable payload;
            // do not shortcut the daemon/worker path by calling activate here.
            unsafe {
                let index: isize =
                    msg_send![&*menu, indexOfItemWithTitle: &*NSString::from_str("Open Pneu")];
                ensure!(index >= 0, "Open Pneu menu item unavailable");
                let _: () = msg_send![&*menu, performActionForItemAtIndex: index];
            }
            Ok(())
        }
        "assert-window-focused" => {
            let window = STATE
                .with(|s| s.borrow().as_ref().and_then(|s| s.window.upgrade()))
                .context("window unavailable")?;
            unsafe {
                let application: Retained<AnyObject> =
                    msg_send![class!(NSApplication), sharedApplication];
                let active: Bool = msg_send![&*application, isActive];
                ensure!(active.as_bool(), "native application is not active");
                let key: Option<Retained<AnyObject>> = msg_send![&*application, keyWindow];
                let key = key.context("native application has no key window")?;
                // Check that the key window is this Slint window, rather than
                // another panel belonging to the same app.
                use slint::winit_030::winit::raw_window_handle::{
                    HasWindowHandle, RawWindowHandle,
                };
                window
                    .window()
                    .with_winit_window(|w| -> Result<()> {
                        let handle = w.window_handle()?;
                        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
                            anyhow::bail!("window has no AppKit handle");
                        };
                        let view = &*handle.ns_view.as_ptr().cast::<AnyObject>();
                        let native: Option<Retained<AnyObject>> = msg_send![view, window];
                        let native = native.context("native window unavailable")?;
                        ensure!(
                            std::ptr::eq(&*key, &*native),
                            "Pneu window is not the key window"
                        );
                        Ok(())
                    })
                    .context("window not shown")??;
            }
            Ok(())
        }
        "light" | "dark" | "system" => {
            unsafe {
                let appearance: Option<Retained<AnyObject>> = if action == "system" {
                    None
                } else {
                    let name = if action == "light" {
                        "NSAppearanceNameAqua"
                    } else {
                        "NSAppearanceNameDarkAqua"
                    };
                    msg_send![class!(NSAppearance),appearanceNamed:&*NSString::from_str(name)]
                };
                let application: Retained<AnyObject> =
                    msg_send![class!(NSApplication), sharedApplication];
                let _: () = msg_send![&*application,setAppearance:appearance.as_deref()];
            }
            let window = STATE
                .with(|s| s.borrow().as_ref().and_then(|s| s.window.upgrade()))
                .context("window unavailable")?;
            window
                .window()
                .with_winit_window(|w| {
                    w.set_theme(match action {
                        "light" => Some(Theme::Light),
                        "dark" => Some(Theme::Dark),
                        _ => None,
                    })
                })
                .context("window is not shown")?;
            Ok(())
        }
        "close-window" => {
            close_window();
            Ok(())
        }
        "assert-window-visible" | "assert-window-hidden" => {
            let window = STATE
                .with(|s| s.borrow().as_ref().and_then(|s| s.window.upgrade()))
                .context("window unavailable")?;
            ensure!(
                window.window().is_visible() == (action == "assert-window-visible"),
                "window visibility does not match {action}"
            );
            Ok(())
        }
        "minimize-window" | "assert-window-restored" => {
            let window = STATE
                .with(|s| s.borrow().as_ref().and_then(|s| s.window.upgrade()))
                .context("window unavailable")?;
            window
                .window()
                .with_winit_window(|w| -> Result<()> {
                    if action == "minimize-window" {
                        w.set_minimized(true);
                    } else {
                        ensure!(w.is_minimized() == Some(false), "window remains minimized");
                    }
                    Ok(())
                })
                .context("window not shown")??;
            Ok(())
        }
        "assert-file-picker" | "assert-folder-picker" => {
            let panels = STATE.with(|s| {
                s.borrow()
                    .as_ref()
                    .map(|s| s.panels.values().cloned().collect::<Vec<_>>())
                    .unwrap_or_default()
            });
            ensure!(!panels.is_empty(), "no picker is open");
            for panel in panels {
                unsafe {
                    let directories: Bool = msg_send![&*panel, canChooseDirectories];
                    let files: Bool = msg_send![&*panel, canChooseFiles];
                    ensure!(
                        directories.as_bool() == (action == "assert-folder-picker")
                            && files.as_bool() == (action == "assert-file-picker"),
                        "picker kind mismatch"
                    );
                }
            }
            Ok(())
        }
        "assert-menu-visible" => {
            let visible = STATE.with(|s| {
                s.borrow().as_ref().is_some_and(|s| unsafe {
                    let visible: Bool = msg_send![&*s.native.status, isVisible];
                    visible.as_bool()
                })
            });
            ensure!(visible, "menu bar item is not visible");
            Ok(())
        }
        "menu" => {
            let native = STATE
                .with(|s| {
                    s.borrow()
                        .as_ref()
                        .map(|s| (s.native.status.clone(), s.native.menu.clone()))
                })
                .context("menu unavailable")?;
            // Queue after acknowledgement so the acceptance client can capture
            // the menu. Cancel in a tracking run-loop mode after four seconds.
            slint::Timer::single_shot(std::time::Duration::from_millis(50), move || unsafe {
                let modes =
                    NSArray::from_slice(&[&*NSString::from_str("NSEventTrackingRunLoopMode")]);
                let _: () = msg_send![&*native.1,performSelector:sel!(cancelTracking),withObject:Option::<&AnyObject>::None,afterDelay:4.0f64,inModes:&*modes];
                let _: () = msg_send![&*native.0,popUpStatusItemMenu:&*native.1];
            });
            Ok(())
        }
        "quit" => {
            // Exercise the same child-to-daemon shutdown event as Quit Pneu.
            STATE.with(|s| {
                if let Some(child) = s.borrow().as_ref().and_then(|s| s.child.as_ref()) {
                    child.action(0, String::new(), "quit".into());
                }
            });
            Ok(())
        }
        "clear-notifications" => {
            let identifiers = STATE
                .with(|s| {
                    s.borrow().as_ref().map(|s| {
                        s.notifications
                            .values()
                            .map(|notice| NSString::from_str(&notice.notification.identifier()))
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default();
            let identifiers =
                NSArray::from_slice(&identifiers.iter().map(|id| &**id).collect::<Vec<_>>());
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.removePendingNotificationRequestsWithIdentifiers(&identifiers);
            center.removeDeliveredNotificationsWithIdentifiers(&identifiers);
            Ok(())
        }
        "cancel-picker" => {
            let ids = STATE
                .with(|s| {
                    s.borrow()
                        .as_ref()
                        .map(|s| s.panels.keys().copied().collect::<Vec<_>>())
                })
                .unwrap_or_default();
            ensure!(!ids.is_empty(), "no picker is open");
            for id in ids {
                cancel(id);
            }
            Ok(())
        }
        _ => anyhow::bail!("unknown UI acceptance action"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, sync::atomic::AtomicBool};
    #[test]
    fn main_thread_only_preference_reads_fail_before_appkit_on_a_worker() {
        for read in [read_reduced_motion, read_twelve_hour_clock] {
            let panic = std::thread::spawn(read).join().unwrap_err();
            assert!(panic.downcast_ref::<String>().is_some_and(|message| {
                message.contains("Pneu AppKit calls must run on the native main thread")
            }));
        }
    }
    #[test]
    fn active_bridge_requests_require_an_installed_native_backend() {
        let (send, receive) = std::sync::mpsc::channel();
        let guard = RequestGuard::new(Arc::new(Lifecycle::new(None)), move |result| {
            send.send(result.unwrap_err().to_string()).unwrap();
        });
        request(
            1,
            Request::OpenUrl("https://example.com".into()),
            None,
            guard,
        );
        assert_eq!(
            receive.try_iter().collect::<Vec<_>>(),
            vec!["native UI backend is not installed"]
        );
        assert!(open_path(Path::new("/Pneu headless test")).is_err());
        assert!(open_url("https://example.com").is_err());
    }
    #[test]
    fn revoked_cached_and_fresh_viewers_never_reach_nsworkspace() -> Result<()> {
        for preview in [false, true] {
            let life = Arc::new(Lifecycle::new(None));
            let (send, receive) = std::sync::mpsc::channel();
            let guard = RequestGuard::new(life.clone(), move |r| send.send(r.is_ok()).unwrap());
            life.cancel();
            // Cached path: rejection happens before any NSWorkspace handoff.
            assert!(open_viewer_path(Path::new("/Pneu test revoked"), preview, &guard).is_err());
            let temp = tempfile::tempdir()?;
            let root = std::fs::canonicalize(temp.path())?;
            let source = root.join("Pneu test source");
            std::fs::write(&source, b"private test bytes")?;
            let cache = root.join("cache");
            crate::config::private_dir(&cache)?;
            let material = materialize(
                &cache,
                std::fs::File::open(source)?,
                "Pneu test",
                &AtomicBool::new(false),
            )?;
            let path = material.path.clone();
            guard.finish(open_viewer_path(&path, preview, &guard));
            drop(material);
            assert!(!path.exists());
            guard.finish(Ok(()));
            drop(guard);
            assert_eq!(receive.try_iter().collect::<Vec<_>>(), vec![false]);
        }
        Ok(())
    }
    #[test]
    fn cancelled_bridge_variants_acknowledge_without_native_state() {
        let variants = [
            Request::Open {
                name: "Pneu test".into(),
                preview: false,
            },
            Request::Notify(super::super::bridge::tests::test_notice()),
            Request::Reveal { paths: vec![] },
            Request::OpenUrl("https://example.com".into()),
            Request::ClearStale {
                namespace: "Pneu test".into(),
            },
            Request::Withdraw {
                id: 1,
                namespace: "Pneu test".into(),
                revision: "test".into(),
            },
        ];
        for request_kind in variants {
            let life = Arc::new(Lifecycle::new(None));
            let (send, receive) = std::sync::mpsc::channel();
            let guard = RequestGuard::new(life.clone(), move |r| send.send(r.is_ok()).unwrap());
            life.cancel();
            request(1, request_kind, None, guard);
            assert_eq!(receive.try_iter().collect::<Vec<_>>(), vec![false]);
        }
    }
    #[test]
    fn background_preferences_use_plain_defaults_without_native_backend() {
        std::thread::spawn(|| {
            for _ in 0..64 {
                assert!(!twelve_hour_clock());
                assert!(!reduced_motion());
            }
        })
        .join()
        .unwrap();
    }
    #[test]
    fn received_viewer_copy_retains_original_bytes_after_display_path_replacement() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let path = root.join("Pneu test original.txt");
        std::fs::write(&path, b"original received bytes")?;
        let descriptor = std::fs::File::open(&path)?;
        std::fs::remove_file(&path)?;
        std::fs::write(&path, b"replacement")?;
        let cache = root.join("viewers");
        crate::config::private_dir(&cache)?;
        let copy = materialize(
            &cache,
            descriptor,
            "Pneu test original.txt",
            &AtomicBool::new(false),
        )?;
        assert_eq!(std::fs::read(&copy.path)?, b"original received bytes");
        assert_eq!(
            std::fs::metadata(&copy.path)?.permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(std::fs::read(&path)?, b"replacement");
        let material = copy.path.clone();
        drop(copy);
        assert!(!material.exists());
        assert!(std::fs::read_dir(&cache)?.next().is_none());
        Ok(())
    }
    #[test]
    fn cancelled_viewer_copy_removes_its_private_materialization() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let path = root.join("source");
        std::fs::write(&path, b"bytes")?;
        let cache = root.join("viewers");
        crate::config::private_dir(&cache)?;
        assert!(
            materialize(
                &cache,
                std::fs::File::open(path)?,
                "Pneu test cancelled.txt",
                &AtomicBool::new(true)
            )
            .is_err()
        );
        assert!(std::fs::read_dir(cache)?.next().is_none());
        Ok(())
    }
}
