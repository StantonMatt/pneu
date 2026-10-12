use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
};

/// An anchored received file. Reads use independent positional offsets; no
/// desktop consumer is allowed to resolve its display pathname again.
#[derive(Clone, Debug)]
pub struct ReceivedFile {
    path: PathBuf,
    file: Arc<std::fs::File>,
}
impl ReceivedFile {
    pub(crate) fn from_file(path: PathBuf, file: std::fs::File) -> Result<Self> {
        anyhow::ensure!(file.metadata()?.is_file(), "received file must be regular");
        Ok(Self {
            path,
            file: Arc::new(file),
        })
    }
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::from(rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        Self::from_file(path.to_owned(), file)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn viewer_descriptor(&self) -> Result<std::fs::File> {
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        // dup/try_clone shares the streaming cursor (usually at EOF). Reopen
        // through our still-owned descriptor, never through the received path,
        // to give each portal launch a read-only cursor starting at byte zero.
        let descriptor = std::fs::File::from(rustix::fs::open(
            format!("/proc/self/fd/{}", self.file.as_raw_fd()),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        let original = self.file.metadata()?;
        let opened = descriptor.metadata()?;
        anyhow::ensure!(
            opened.is_file() && (opened.dev(), opened.ino()) == (original.dev(), original.ino()),
            "viewer descriptor changed identity"
        );
        Ok(descriptor)
    }
    pub fn read_bounded(&self, limit: usize) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        anyhow::ensure!(
            self.file.metadata()?.len() <= limit as u64,
            "input exceeds bounded read limit"
        );
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        while bytes.len() <= limit {
            let cap = buffer.len().min(limit + 1 - bytes.len());
            let n = self.file.read_at(&mut buffer[..cap], bytes.len() as u64)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        anyhow::ensure!(bytes.len() <= limit, "input grew beyond bounded read limit");
        Ok(bytes)
    }
    pub(crate) fn is_image(&self, mime: &str) -> Result<bool> {
        use std::os::unix::fs::FileExt;
        let mut header = [0; 32];
        let n = self.file.read_at(&mut header, 0)?;
        let bytes = &header[..n];
        Ok(match mime {
            "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
            "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            "image/bmp" => bytes.starts_with(b"BM"),
            _ => false,
        })
    }
}

#[async_trait]
pub trait PcActions: Send + Sync {
    async fn clipboard_text(&self, text: &str) -> Result<()>;
    async fn clipboard_image(&self, path: &Path, mime: &str) -> Result<()>;
    async fn notify(&self, path: &Path, mime: &str) -> Result<()>;
    async fn open(&self, path: &Path) -> Result<()>;
    async fn notify_clipboard_image(&self, path: &Path, mime: &str) -> Result<()> {
        self.notify(path, mime).await
    }
    // Defaults keep lightweight integrations usable; the desktop backend consumes
    // descriptors for every data-bearing action and notification-button handoff.
    async fn clipboard_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.clipboard_image(file.path(), mime).await
    }
    async fn notify_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.notify(file.path(), mime).await
    }
    async fn notify_copied_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.notify_clipboard_image(file.path(), mime).await
    }
    async fn open_received(&self, file: &ReceivedFile) -> Result<()> {
        self.open(file.path()).await
    }
    async fn notify_clipboard_failure(&self) -> Result<()> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
/// Desktop integration owns its clipboard helper and one D-Bus signal task.
/// wl-clipboard-rs owns the selection bytes; large images are capped to bound
/// its unavoidable buffers. The helper then serves the
/// data-control selection without a fork from a multithreaded Tokio process.
pub struct DesktopActions {
    clipboard: Mutex<Option<Child>>,
    clipboard_gate: Mutex<()>,
    cache: PathBuf,
    notifications: Mutex<Option<Notifications>>,
    jobs: Arc<crate::work::Work>,
    #[cfg(test)]
    bus: Option<zbus::Connection>,
}
struct Notifications {
    proxy: zbus::Proxy<'static>,
    task: JoinHandle<()>,
    paths: Arc<Mutex<HashMap<u32, ReceivedFile>>>,
}
const NOTIFICATION_ACTION_LIMIT: usize = 256;
fn remember_notification(paths: &mut HashMap<u32, ReceivedFile>, id: u32, path: ReceivedFile) {
    if paths.len() == NOTIFICATION_ACTION_LIMIT
        && !paths.contains_key(&id)
        && let Some(old) = paths.keys().min().copied()
    {
        paths.remove(&old);
    }
    paths.insert(id, path);
}
impl DesktopActions {
    pub fn new(cache: PathBuf) -> Self {
        Self {
            clipboard: Mutex::new(None),
            clipboard_gate: Mutex::new(()),
            cache,
            notifications: Mutex::new(None),
            jobs: Arc::new(crate::work::Work::default()),
            #[cfg(test)]
            bus: None,
        }
    }
    async fn copy(
        &self,
        file: Option<&ReceivedFile>,
        text: Option<&str>,
        mime: &str,
    ) -> Result<()> {
        let _guard = self.clipboard_gate.lock().await;
        let bytes = if let Some(file) = file {
            let file = file.clone();
            Some(
                crate::work::start_blocking(vec![self.jobs.clone()], move || {
                    file.read_bounded(16 * 1024 * 1024)
                })
                .await??,
            )
        } else {
            None
        };
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("clipboard-helper")
            .arg(mime)
            .env("TMPDIR", &self.cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut input = child.stdin.take().context("helper stdin")?;
        if let Some(bytes) = bytes {
            input.write_all(&bytes).await?;
        }
        if let Some(text) = text {
            input.write_all(text.as_bytes()).await?;
        }
        drop(input);
        let mut output = BufReader::new(child.stdout.take().context("helper stdout")?.take(16));
        let mut line = String::new();
        let ready = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            output.read_line(&mut line),
        )
        .await;
        if !matches!(ready, Ok(Ok(n)) if n > 0) || line.trim() != "READY" {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("Wayland data-control clipboard unavailable");
        }
        if let Some(mut previous) = self.clipboard.lock().await.replace(child) {
            let _ = previous.kill().await;
            let _ = previous.wait().await;
        }
        Ok(())
    }
    async fn preview(
        &self,
        file: &ReceivedFile,
    ) -> Result<(i32, i32, i32, bool, i32, i32, Vec<u8>)> {
        let file = file.clone();
        crate::work::start_blocking(vec![self.jobs.clone()], move || {
            let image = decode_image(&file.read_bounded(16 * 1024 * 1024)?)?
                .thumbnail(256, 256)
                .to_rgba8();
            let width = image.width() as i32;
            let height = image.height() as i32;
            Ok((width, height, width * 4, true, 8, 4, image.into_raw()))
        })
        .await?
    }
    async fn bus_connection(&self) -> Result<zbus::Connection> {
        #[cfg(test)]
        if let Some(bus) = &self.bus {
            return Ok(bus.clone());
        }
        Ok(zbus::Connection::session().await?)
    }
    async fn notifications(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Notifications>>> {
        let mut current = self.notifications.lock().await;
        if current.is_none() {
            let connection = self.bus_connection().await?;
            let action_connection = connection.clone();
            let proxy = zbus::Proxy::new_owned(
                connection,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await?;
            let mut signals = proxy.receive_signal("ActionInvoked").await?;
            let mut closed = proxy.receive_signal("NotificationClosed").await?;
            let paths = Arc::new(Mutex::new(HashMap::<u32, ReceivedFile>::new()));
            let action_paths = paths.clone();
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        signal=closed.next()=>{
                            let Some(signal)=signal else {break;};
                            if let Ok((id,_reason))=signal.body().deserialize::<(u32,u32)>() {action_paths.lock().await.remove(&id);}
                        },
                        signal=signals.next()=>{
                            let Some(signal)=signal else {break;};
                            if let Ok((id,action))=signal.body().deserialize::<(u32,String)>() {
                                let path=action_paths.lock().await.get(&id).cloned();
                                if let Some(path)=path {match action.as_str() {"default"|"open"=>{let _=open_received_file(&path, &action_connection).await;},"show"=>{let _=show_path(path.path()).await;},_=>{}}}
                            }
                        }
                    }
                }
            });
            *current = Some(Notifications { proxy, task, paths });
        }
        Ok(current)
    }
}
#[async_trait]
impl PcActions for DesktopActions {
    async fn clipboard_text(&self, text: &str) -> Result<()> {
        self.copy(None, Some(text), "text/plain;charset=utf-8")
            .await
    }
    async fn clipboard_image(&self, path: &Path, mime: &str) -> Result<()> {
        self.clipboard_received(&ReceivedFile::open(path)?, mime)
            .await
    }
    async fn clipboard_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        self.copy(Some(file), None, mime).await
    }
    async fn notify(&self, path: &Path, mime: &str) -> Result<()> {
        self.notify_received(&ReceivedFile::open(path)?, mime).await
    }
    async fn notify_received(&self, file: &ReceivedFile, mime: &str) -> Result<()> {
        let path = file.path();
        let current = self.notifications().await?;
        let n = current.as_ref().context("notification proxy")?;
        let mut hints = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        if mime.starts_with("image/") {
            // Decode a bounded inline preview. The notification server never
            // receives a pathname from which it could read substituted bytes.
            let preview = self.preview(file).await?;
            hints.insert(
                "image-data",
                zbus::zvariant::Value::from(zbus::zvariant::Structure::from(preview)),
            );
        }
        hints.insert("desktop-entry", zbus::zvariant::Value::from("pneud"));
        let summary = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Received file");
        // Hold registration through Notify so an immediate closed signal cannot
        // race insertion of a stale live-notification mapping.
        let mut paths = n.paths.lock().await;
        let id: u32 = n
            .proxy
            .call(
                "Notify",
                &(
                    "Pneu",
                    0u32,
                    "image-x-generic",
                    summary,
                    "",
                    vec!["default", "Open", "open", "Open", "show", "Show in folder"],
                    hints,
                    -1i32,
                ),
            )
            .await?;
        remember_notification(&mut paths, id, file.clone());
        Ok(())
    }
    async fn notify_clipboard_image(&self, path: &Path, _mime: &str) -> Result<()> {
        self.notify_copied_received(&ReceivedFile::open(path)?, _mime)
            .await
    }
    async fn notify_copied_received(&self, file: &ReceivedFile, _mime: &str) -> Result<()> {
        let preview = self.preview(file).await?;
        let current = self.notifications().await?;
        let n = current.as_ref().context("notification proxy")?;
        let mut hints = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        hints.insert(
            "image-data",
            zbus::zvariant::Value::from(zbus::zvariant::Structure::from(preview)),
        );
        hints.insert("transient", zbus::zvariant::Value::from(true));
        hints.insert("urgency", zbus::zvariant::Value::from(0u8));
        let _: u32 = n
            .proxy
            .call(
                "Notify",
                &(
                    "Pneu",
                    0u32,
                    "pneu",
                    "Image copied from phone",
                    "",
                    Vec::<&str>::new(),
                    hints,
                    4000i32,
                ),
            )
            .await?;
        Ok(())
    }
    async fn notify_clipboard_failure(&self) -> Result<()> {
        let current = self.notifications().await?;
        let n = current.as_ref().context("notification proxy")?;
        let mut hints = HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        hints.insert("transient", zbus::zvariant::Value::from(true));
        let _: u32 = n
            .proxy
            .call(
                "Notify",
                &(
                    "Pneu",
                    0u32,
                    "pneu",
                    "Couldn't copy image",
                    "",
                    Vec::<&str>::new(),
                    hints,
                    4000i32,
                ),
            )
            .await?;
        Ok(())
    }
    async fn open(&self, path: &Path) -> Result<()> {
        self.open_received(&ReceivedFile::open(path)?).await
    }
    async fn open_received(&self, file: &ReceivedFile) -> Result<()> {
        open_received_file(file, &self.bus_connection().await?).await
    }
    async fn shutdown(&self) -> Result<()> {
        let _guard = self.clipboard_gate.lock().await;
        if let Some(mut child) = self.clipboard.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.jobs.drain().await;
        if let Some(n) = self.notifications.lock().await.take() {
            n.task.abort();
            let _ = n.task.await;
        }
        Ok(())
    }
}
fn file_uri(path: &Path) -> Result<String> {
    Ok(url::Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("invalid local path"))?
        .to_string())
}
/// OpenFile transfers an FD to the portal, including ownership across the
/// asynchronous viewer launch. There is deliberately no pathname fallback.
async fn open_received_file(file: &ReceivedFile, connection: &zbus::Connection) -> Result<()> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.OpenURI",
    )
    .await?;
    let descriptor = file.viewer_descriptor()?;
    let fd = zbus::zvariant::Fd::from(&descriptor);
    let _: zbus::zvariant::OwnedObjectPath = proxy
        .call(
            "OpenFile",
            &("", fd, HashMap::<&str, zbus::zvariant::Value<'_>>::new()),
        )
        .await?;
    Ok(())
}
async fn show_path(path: &Path) -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.FileManager1",
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
    )
    .await?;
    let _: () = proxy
        .call("ShowItems", &(vec![file_uri(path)?], ""))
        .await?;
    Ok(())
}
fn decode_image(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    Ok(reader.decode()?)
}
fn clipboard_sources(
    mime: String,
    bytes: Vec<u8>,
) -> Result<Vec<wl_clipboard_rs::copy::MimeSource>> {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Source};
    let mut sources = Vec::new();
    if mime.starts_with("image/") && mime != "image/png" {
        let mut png = std::io::Cursor::new(Vec::new());
        decode_image(&bytes)?.write_to(&mut png, image::ImageFormat::Png)?;
        anyhow::ensure!(
            png.get_ref().len() <= 32 * 1024 * 1024,
            "converted clipboard image too large"
        );
        sources.push(MimeSource {
            source: Source::Bytes(png.into_inner().into()),
            mime_type: MimeType::Specific("image/png".into()),
        });
    }
    sources.push(MimeSource {
        source: Source::Bytes(bytes.into()),
        mime_type: MimeType::Specific(mime),
    });
    Ok(sources)
}
pub fn clipboard_helper(mime: String) -> Result<()> {
    use std::io::{Read, Write};
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "clipboard helper input too large"
    );
    let mut options = wl_clipboard_rs::copy::Options::new();
    options.foreground(true);
    let selection = options.prepare_copy_multi(clipboard_sources(mime, bytes)?)?;
    println!("READY");
    std::io::stdout().flush()?;
    selection.serve()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipboard_offers_original_and_real_png_bytes() -> Result<()> {
        let image = image::DynamicImage::new_rgb8(2, 2);
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image.write_to(&mut jpeg, image::ImageFormat::Jpeg)?;
        let sources = clipboard_sources("image/jpeg".into(), jpeg.into_inner())?;
        assert_eq!(sources.len(), 2);
        let wl_clipboard_rs::copy::Source::Bytes(bytes) = &sources[0].source else {
            panic!("expected bytes")
        };
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(decode_image(bytes)?.width(), 2);
        Ok(())
    }
    #[tokio::test]
    async fn clipboard_preview_rejects_large_sparse_input_before_decoding() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("large.png");
        std::fs::File::create(&path)?.set_len(pneu_proto::MAX_FILE_SIZE)?;
        let actions = DesktopActions::new(dir.path().to_owned());
        let error = actions
            .notify_clipboard_image(&path, "image/png")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("bounded read"));
        Ok(())
    }
    #[tokio::test]
    async fn retained_file_and_preview_ignore_symlink_and_regular_replacements() -> Result<()> {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("received.png");
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3).write_to(&mut encoded, image::ImageFormat::Png)?;
        let bytes = encoded.into_inner();
        std::fs::write(&path, &bytes)?;
        let file = ReceivedFile::open(&path)?;
        assert!(file.is_image("image/png")?);
        let replacement = dir.path().join("secret");
        std::fs::write(&replacement, b"secret bytes")?;
        std::fs::remove_file(&path)?;
        symlink(&replacement, &path)?;
        assert!(ReceivedFile::open(&path).is_err());
        assert_eq!(file.read_bounded(16 * 1024 * 1024)?, bytes);
        let actions = DesktopActions::new(dir.path().to_owned());
        let preview = actions.preview(&file).await?;
        assert_eq!((preview.0, preview.1), (171, 256));
        std::fs::remove_file(&path)?;
        std::fs::write(&path, b"different inode")?;
        assert_eq!(file.read_bounded(16 * 1024 * 1024)?, bytes);
        let fifo = dir.path().join("pipe");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )?;
        assert!(
            ReceivedFile::open(&fifo).is_err(),
            "FIFO must fail without blocking"
        );
        actions.shutdown().await?;
        Ok(())
    }
    #[tokio::test]
    async fn shutdown_waits_for_canceled_preview_work() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let actions = Arc::new(DesktopActions::new(dir.path().to_owned()));
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, blocking) = std::sync::mpsc::channel();
        let job = crate::work::start_blocking(vec![actions.jobs.clone()], move || {
            let _ = entered.send(());
            blocking.recv()
        });
        wait.await?;
        job.abort(); // A started blocking job still owns its completion token.
        let shutdown = actions.clone();
        let task = tokio::spawn(async move { shutdown.shutdown().await });
        tokio::task::yield_now().await;
        let waiting = !task.is_finished();
        release.send(())?;
        task.await??;
        let _ = job.await;
        assert!(waiting);
        Ok(())
    }
    struct NotificationServer(
        std::sync::atomic::AtomicU32,
        Arc<std::sync::Mutex<HashMap<String, zbus::zvariant::OwnedValue>>>,
    );
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl NotificationServer {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            _app: &str,
            _replaces: u32,
            _icon: &str,
            _summary: &str,
            _body: &str,
            _actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _timeout: i32,
        ) -> u32 {
            *self.1.lock().unwrap() = hints;
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
        }
    }
    struct PortalServer(Arc<std::sync::Mutex<Vec<u8>>>);
    #[zbus::interface(name = "org.freedesktop.portal.OpenURI")]
    impl PortalServer {
        fn open_file(
            &self,
            _parent: &str,
            fd: zbus::zvariant::Fd<'_>,
            _options: HashMap<String, zbus::zvariant::OwnedValue>,
        ) -> zbus::zvariant::OwnedObjectPath {
            use std::io::Read;
            use std::os::fd::AsFd;
            let file = std::fs::File::from(fd.as_fd().try_clone_to_owned().unwrap());
            let mut bytes = Vec::new();
            file.take(1024).read_to_end(&mut bytes).unwrap();
            *self.0.lock().unwrap() = bytes;
            zbus::zvariant::OwnedObjectPath::try_from(
                "/org/freedesktop/portal/desktop/request/test",
            )
            .unwrap()
        }
    }
    #[tokio::test]
    async fn peer_notifications_cannot_grow_the_action_mapping_without_bound() {
        let mut paths = HashMap::new();
        for id in 0..NOTIFICATION_ACTION_LIMIT as u32 * 2 {
            remember_notification(
                &mut paths,
                id,
                ReceivedFile::from_file(
                    PathBuf::from(format!("{id}.png")),
                    tempfile::tempfile().unwrap(),
                )
                .unwrap(),
            );
            assert!(paths.len() <= NOTIFICATION_ACTION_LIMIT);
        }
        assert_eq!(paths.len(), NOTIFICATION_ACTION_LIMIT);
        assert!(!paths.contains_key(&0));
        let last = NOTIFICATION_ACTION_LIMIT as u32 * 2 - 1;
        remember_notification(
            &mut paths,
            last,
            ReceivedFile::from_file(
                PathBuf::from("replacement.png"),
                tempfile::tempfile().unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(paths.len(), NOTIFICATION_ACTION_LIMIT);
        assert_eq!(paths[&last].path(), Path::new("replacement.png"));
    }
    #[tokio::test]
    async fn live_notification_actions_survive_65th_and_closed_ids_are_removed() -> Result<()> {
        let bus_dir = tempfile::tempdir()?;
        let config = bus_dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth><apparmor mode=\"disabled\"/><policy context=\"default\"><allow user=\"*\"/><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",
                bus_dir.path().display()
            ),
        )?;
        let mut bus = tokio::process::Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut line = String::new();
        BufReader::new(bus.stdout.take().context("bus stdout")?)
            .read_line(&mut line)
            .await?;
        let address = line.trim();
        let portal_bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notification_hints = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let server = zbus::connection::Builder::address(address)?
            .name("org.freedesktop.Notifications")?
            .name("org.freedesktop.portal.Desktop")?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                PortalServer(portal_bytes.clone()),
            )?
            .serve_at(
                "/org/freedesktop/Notifications",
                NotificationServer(
                    std::sync::atomic::AtomicU32::new(0),
                    notification_hints.clone(),
                ),
            )?
            .build()
            .await?;
        let connection = zbus::connection::Builder::address(address)?.build().await?;
        let dir = tempfile::tempdir()?;
        let mut actions = DesktopActions::new(dir.path().to_owned());
        actions.bus = Some(connection);
        let original = dir.path().join("viewer.png");
        std::fs::write(&original, b"verified viewer bytes")?;
        let file = ReceivedFile::open(&original)?;
        rustix::fs::seek(&*file.file, rustix::fs::SeekFrom::End(0))?;
        std::fs::remove_file(&original)?;
        std::os::unix::fs::symlink("/etc/passwd", &original)?;
        actions.open_received(&file).await?;
        assert_eq!(&*portal_bytes.lock().unwrap(), b"verified viewer bytes");
        for n in 0..70 {
            std::fs::write(dir.path().join(format!("{n}.bin")), b"file")?;
            actions
                .notify(
                    &dir.path().join(format!("{n}.bin")),
                    "application/octet-stream",
                )
                .await?;
        }
        {
            let current = actions.notifications().await?;
            let paths = current.as_ref().unwrap().paths.lock().await;
            assert_eq!(paths.len(), 70);
            assert!(paths.contains_key(&1));
        }
        server
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "NotificationClosed",
                &(1u32, 2u32),
            )
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let current = actions.notifications().await?;
                if !current
                    .as_ref()
                    .unwrap()
                    .paths
                    .lock()
                    .await
                    .contains_key(&1)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                drop(current);
                tokio::task::yield_now().await;
            }
        })
        .await??;
        {
            let current = actions.notifications().await?;
            let paths = current.as_ref().unwrap().paths.lock().await;
            assert_eq!(paths.len(), 69);
            assert!(paths.contains_key(&2));
        }
        let image_path = dir.path().join("notification.png");
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3).write_to(&mut png, image::ImageFormat::Png)?;
        let image_bytes = png.into_inner();
        std::fs::write(&image_path, &image_bytes)?;
        let file = ReceivedFile::open(&image_path)?;
        std::fs::remove_file(&image_path)?;
        std::os::unix::fs::symlink("/etc/passwd", &image_path)?;
        actions.notify_received(&file, "image/png").await?;
        {
            let hints = notification_hints.lock().unwrap();
            assert!(hints.contains_key("image-data"));
            assert!(!hints.contains_key("image-path"));
        }
        actions.notify_copied_received(&file, "image/png").await?;
        server
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "ActionInvoked",
                &(71u32, "open"),
            )
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while *portal_bytes.lock().unwrap() != image_bytes {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        actions.shutdown().await?;
        drop(server);
        bus.kill().await?;
        bus.wait().await?;
        Ok(())
    }
}
