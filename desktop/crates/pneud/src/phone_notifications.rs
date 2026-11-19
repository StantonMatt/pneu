//! Connection-owned, bounded, ordered notification effects. No durable queue.
use super::*;
use crate::ui::notifications::{PhoneIcon, PhoneNotice};

use pneu_proto::{NotificationAction, NotificationEvent};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const STOP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct Stream {
    ready: bool,
    keys: HashSet<Id>,
}
impl Stream {
    pub fn accept(&mut self, event: &NotificationEvent) -> Result<()> {
        event.validate()?;
        match event.action {
            NotificationAction::Clear => {
                self.ready = true;
                self.keys.clear();
            }
            NotificationAction::Remove => {
                ensure!(self.ready, "notification stream requires clear");
                self.keys.remove(event.key.as_ref().context("missing key")?);
            }
            NotificationAction::Upsert => {
                ensure!(self.ready, "notification stream requires clear");
                let key = event.key.as_ref().context("missing key")?;
                ensure!(
                    self.keys.contains(key) || self.keys.len() < 128,
                    "notification stream capacity"
                );
                self.keys.insert(key.clone());
            }
        }
        Ok(())
    }
}

pub(super) struct Worker {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    send: Option<mpsc::Sender<NotificationEvent>>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Worker {
    pub fn start(
        core: Arc<Core>,
        store: Store,
        device: Id,
        scope: Option<ActionScope>,
        mut close: watch::Receiver<Option<CloseReason>>,
    ) -> Self {
        let (send, mut receive) = mpsc::channel::<NotificationEvent>(128);
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let session = crate::random_hex(8);
            let mut handles: HashMap<Id, u32> = HashMap::new();
            let mut icons = Icons::new(&core.paths.cache, &session);
            loop {
                let event = tokio::select! { biased;
                    _ = close.changed() => break,
                    _ = &mut stopped => break,
                    event = receive.recv() => match event { Some(e) => e, None => break },
                };
                if close.borrow().is_some() || !core.connection_current(&store, &device) {
                    break;
                }
                match event.action {
                    NotificationAction::Clear => withdraw(&core, &session, &mut handles).await,
                    NotificationAction::Remove => {
                        let key = event.key.expect("validated key");
                        if let Some(handle) = handles.remove(&key) {
                            remove(&core, &format!("{session}{key}"), handle).await;
                        }
                    }
                    NotificationAction::Upsert => {
                        let key = event.key.clone().expect("validated key");
                        let icon = icons.get(&event);
                        let notice = PhoneNotice {
                            app: event.app_label.expect("validated label"),
                            title: event.title.map(paragraph),
                            text: event.text.map(paragraph),
                            icon,
                        };
                        let id = format!("{session}{key}");
                        let Some(scope) = &scope else {
                            break;
                        };
                        let origin = ActionOrigin::captured(&store, &device);
                        let replaces = handles.get(&key).copied().unwrap_or(0);
                        let result = tokio::select! { biased;
                            _ = close.changed() => break,
                            _ = &mut stopped => break,
                            result = scope.notification(&origin, || core.actions.begin_phone_notification(&id, &notice, replaces)) => result,
                        };
                        match result {
                            Some(Ok(handle)) => {
                                handles.insert(key, handle);
                            }
                            Some(Err(_)) => {
                                core.error(Some(device.clone()), "Phone notification unavailable")
                            }
                            None => break,
                        }
                    }
                }
            }
            withdraw(&core, &session, &mut handles).await;
            // Dropping the connection cache removes the session's private icon files.
        });
        Self {
            stop: Some(stop),
            send: Some(send),
            task: Some(task),
        }
    }
    pub fn send(&self, event: NotificationEvent) -> Result<()> {
        self.send
            .as_ref()
            .context("notification worker ended")?
            .try_send(event)
            .map_err(|_| anyhow::anyhow!("notification dispatch capacity"))
    }
    pub async fn stop(&mut self) {
        self.stop.take();
        self.send.take();
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(STOP_TIMEOUT, &mut task).await.is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}
async fn remove(core: &Core, id: &str, handle: u32) {
    if !matches!(
        tokio::time::timeout(
            crate::actions::PHONE_WITHDRAW_TIMEOUT,
            core.actions.remove_phone_notification(id, handle),
        )
        .await,
        Ok(Ok(()))
    ) {
        core.error(None, "Phone notification withdrawal unavailable");
    }
}
async fn withdraw(core: &Core, session: &str, handles: &mut HashMap<Id, u32>) {
    if handles.is_empty() {
        return;
    }
    let handles: Vec<_> = handles
        .drain()
        .map(|(key, handle)| (format!("{session}{key}"), handle))
        .collect();
    // One native bulk request on macOS avoids overflowing its bridge admission
    // limit. Other backends still attempt every handle within one shared bound.
    if !matches!(
        tokio::time::timeout(
            crate::actions::PHONE_WITHDRAW_TIMEOUT,
            core.actions.remove_phone_notifications(&handles),
        )
        .await,
        Ok(Ok(()))
    ) {
        core.error(None, "Phone notification withdrawal unavailable");
    }
}

pub(super) fn prepare_icons(cache: &Path) -> Result<()> {
    FileIcons::prepare(cache)?;
    Ok(())
}
fn paragraph(value: String) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
struct Icons {
    files: FileIcons,
    apps: HashMap<String, Option<PhoneIcon>>,
}
impl Icons {
    fn new(cache: &Path, session: &str) -> Self {
        Self {
            files: FileIcons::new(cache, session),
            apps: HashMap::new(),
        }
    }
    fn get(&mut self, event: &NotificationEvent) -> Option<PhoneIcon> {
        let app = event.app_package.as_ref()?;
        if let Some(icon) = self.apps.get(app) {
            return icon.clone();
        }
        if self.apps.len() >= 128 {
            return None;
        }
        let icon = event.icon_png.as_ref().and_then(|hex| {
            let bytes = hex::decode(hex).ok()?;
            let mut reader = image::ImageReader::with_format(
                std::io::Cursor::new(bytes),
                image::ImageFormat::Png,
            );
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(64);
            limits.max_image_height = Some(64);
            limits.max_alloc = Some(65_536);
            reader.limits(limits);
            let image = reader.decode().ok()?.to_rgba8();
            self.files.write(image, self.apps.len())
        });
        self.apps.insert(app.clone(), icon.clone());
        icon
    }
}

// Every desktop uses the same private, connection-owned icon files.
struct FileIcons {
    path: PathBuf,
    created: bool,
}
impl FileIcons {
    fn prepare(cache: &Path) -> Result<()> {
        let root = cache.join("phone-notification-icons");
        crate::config::private_dir(&root)?;
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            // Do not follow symlinks left in the dedicated private cache.
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
            } else {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    fn new(cache: &Path, session: &str) -> Self {
        Self {
            path: cache.join("phone-notification-icons").join(session),
            created: false,
        }
    }
    fn write(&mut self, image: image::RgbaImage, index: usize) -> Option<PhoneIcon> {
        // Re-encode to strip metadata and animation, never trust a peer path.
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut png, image::ImageFormat::Png)
            .ok()?;
        if png.get_ref().len() > 8192 {
            return None;
        }
        if !self.created {
            crate::config::private_dir(&self.path).ok()?;
            self.created = true;
        }
        let path = self.path.join(format!("{index}.png"));
        crate::config::atomic_private(&path, png.get_ref()).ok()?;
        Some(PhoneIcon::File(path))
    }
}
impl Drop for FileIcons {
    fn drop(&mut self) {
        if self.created {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(value: serde_json::Value) -> NotificationEvent {
        serde_json::from_value(value).unwrap()
    }
    fn upsert(n: u128) -> NotificationEvent {
        event(
            serde_json::json!({"action":"upsert", "key": format!("{n:032x}"), "app_package":"org.chat", "app_label":"Chat", "time":1}),
        )
    }
    #[test]
    fn phone_notifications_clear_update_remove_cap_and_new_connection() {
        let mut stream = Stream::default();
        let remove = event(serde_json::json!({"action":"remove", "key":format!("{:032x}", 1)}));
        assert!(stream.accept(&remove).is_err());
        assert!(stream.accept(&upsert(1)).is_err());
        let clear = event(serde_json::json!({"action":"clear"}));
        stream.accept(&clear).unwrap();
        stream.accept(&remove).unwrap();
        for n in 0..128 {
            stream.accept(&upsert(n)).unwrap();
        }
        stream.accept(&upsert(1)).unwrap(); // replace even at cap
        assert!(stream.accept(&upsert(128)).is_err());
        stream.accept(&remove).unwrap();
        stream.accept(&upsert(128)).unwrap();
        stream.accept(&clear).unwrap();
        assert!(stream.keys.is_empty());
        assert!(Stream::default().accept(&remove).is_err());
    }
    #[test]
    fn phone_notifications_section_8_3_vectors_and_icon_lifetime() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../../../docs/protocol-vectors/v1.json"))
                .unwrap();
        let scratch = crate::test_support::scratch().unwrap();
        prepare_icons(scratch.path()).unwrap();
        let mut icons = Icons::new(scratch.path(), "session");
        let mut stream = Stream::default();
        let mut icon = None;
        let mut count = 0;
        for frame in vectors["frames"].as_array().unwrap() {
            if frame["decoded"]["type"] != "notification_event" {
                continue;
            }
            let e: NotificationEvent =
                serde_json::from_value(frame["decoded"]["payload"].clone()).unwrap();
            e.validate().unwrap();
            stream.accept(&e).unwrap();
            if e.action == NotificationAction::Upsert {
                let cached = icons.get(&e);
                assert!(cached.is_some());
                if let Some(icon) = &icon {
                    assert_eq!(&cached, icon);
                }
                icon = Some(cached);
            }
            count += 1;
        }
        assert_eq!(count, 4);
        let icon = icon.unwrap().unwrap();
        stream
            .accept(&event(serde_json::json!({"action":"clear"})))
            .unwrap();
        let mut same_app = upsert(1);
        same_app.app_package = Some("org.example.chat".into());
        assert_eq!(icons.get(&same_app), Some(icon.clone()));
        assert_eq!(Icons::new(scratch.path(), "fresh").get(&same_app), None);
        let path = icon.file_path().expect("all desktops cache a private PNG");
        assert!(path.starts_with(scratch.path()));
        assert!(path.exists());
        drop(icons);
        assert!(!path.exists());
    }
    #[test]
    fn phone_notifications_icon_cache_bounds_and_invalid_png_fallback() {
        let scratch = crate::test_support::scratch().unwrap();
        prepare_icons(scratch.path()).unwrap();
        let mut icons = Icons::new(scratch.path(), "session");
        for n in 0..129 {
            let mut e = upsert(n);
            e.app_package = Some(format!("org.app{n}"));
            e.icon_png = Some("0000".into());
            assert_eq!(icons.get(&e), None);
        }
        assert_eq!(icons.apps.len(), 128);
        assert!(!icons.files.created);
        let mut large = upsert(1);
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(65, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        large.icon_png = Some(hex::encode(png.into_inner()));
        assert!(Icons::new(scratch.path(), "large").get(&large).is_none());
    }
    #[test]
    fn phone_notifications_private_icons_permissions_startup_wipe_and_session_cleanup() -> Result<()>
    {
        let scratch = crate::test_support::scratch()?;
        let cache = scratch.path().join("cache");
        crate::config::private_dir(&cache)?;
        FileIcons::prepare(&cache)?;
        let mut first = FileIcons::new(&cache, "first");
        let icon = first.write(image::RgbaImage::new(64, 64), 0).unwrap();
        let path = icon.file_path().unwrap();
        assert!(path.starts_with(&cache));
        assert_eq!(image::image_dimensions(path)?, (64, 64));
        let dir = crate::storage::Folder::open(path.parent().unwrap(), false)?;
        let file = crate::storage::open_regular_file(path)?;
        crate::platform::private::check(&dir.file)?;
        crate::platform::private::check(&file)?;
        assert!(crate::platform::fs::is_private_directory(&dir.file)?);
        assert!(crate::platform::fs::is_private_file(&file)?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(dir.file.metadata()?.permissions().mode() & 0o777, 0o700);
            assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
        }
        drop(dir);
        drop(file);
        let mut second = FileIcons::new(&cache, "second");
        let other = second.write(image::RgbaImage::new(1, 1), 0).unwrap();
        drop(first);
        assert!(!path.exists(), "disconnect removes its session");
        assert!(
            other.file_path().unwrap().exists(),
            "other sessions survive"
        );
        // Simulate a crash: keep files without invoking connection cleanup.
        second.created = false;
        drop(second);
        let unrelated = cache.join("unrelated");
        std::fs::write(&unrelated, b"keep")?;
        FileIcons::prepare(&cache)?;
        assert!(!other.file_path().unwrap().exists());
        assert_eq!(
            std::fs::read_dir(cache.join("phone-notification-icons"))?.count(),
            0
        );
        assert_eq!(std::fs::read(&unrelated)?, b"keep");
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn phone_notifications_worker_stop_aborts_stalled_teardown_and_releases_resources() {
        let alive = Arc::new(AtomicU64::new(1));
        struct Resource(Arc<AtomicU64>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.store(0, Ordering::SeqCst);
            }
        }
        let resource = Resource(alive.clone());
        let task = tokio::spawn(async move {
            let _resource = resource;
            std::future::pending::<()>().await;
        });
        let mut worker = Worker {
            stop: None,
            send: None,
            task: Some(task),
        };
        tokio::time::timeout(STOP_TIMEOUT + Duration::from_millis(100), worker.stop())
            .await
            .unwrap();
        assert_eq!(alive.load(Ordering::SeqCst), 0);
    }
}
